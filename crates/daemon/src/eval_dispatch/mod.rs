//! Eval-mode agent dispatch helper.
//!
//! Mirrors the WebSocket-path agent dispatch in `crates/daemon/src/server.rs`
//! (lines ~4336-4442) with eval-specific simplifications:
//! - No sidebar forwarder task (eval consumes events via EventBus SSE)
//! - No cancellation/interrupt registry wiring (eval has its own task timeout)
//! - No proxy_id/channel/trace_collector chaining (not needed for outcome-only)
//! - No plan re-run path (treat AgentOutput as terminal)
//!
//! `soul_context` is skipped: eval services have `knowledge_retriever = None`,
//! so `build_soul_context` would return `None` anyway.

// Re-export mock server when the feature is enabled (Task 4 lands the impl).
// #[cfg(feature = "eval-mock-llm")]
// pub mod mock_llm_server;

use crate::agent_host::{DaemonHostFunctions, SidebarStreamChunk};
use crate::config::AgentConfig;
use crate::session::SessionManager;
use crate::wasm::HostServices;
use nevoflux_builtin_wasm::{Agent, AgentInput, AgentMode};
use nevoflux_protocol::subagent::ToolsConfig;
use std::sync::{atomic::AtomicBool, Arc};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Services bundle consumed by [`dispatch_eval_turn`].
///
/// All fields are `Arc`-wrapped and cheap to clone from the daemon's
/// long-lived service bundle. The `#[derive(Clone)]` intentionally does
/// NOT clone the `runtime` field (which is `Handle::current()` anyway);
/// callers should pass `Handle::current()` at the call site instead.
#[derive(Clone)]
pub struct EvalDispatchServices {
    /// Wasm host services (database, skills, llm_config, etc.).
    pub services: HostServices,
    /// Daemon-wide agent configuration.
    pub config: Arc<AgentConfig>,
    /// Tokio runtime handle for `DaemonHostFunctions::new`.
    pub runtime: Handle,
    /// Session manager for loading conversation history.
    pub session_manager: Arc<SessionManager>,
    /// Canvas video render pipeline (required by `with_canvas_video_service`).
    pub canvas_video_service: Arc<crate::canvas_video::CanvasVideoService>,
}

/// Errors from [`dispatch_eval_turn`].
#[derive(Debug, thiserror::Error)]
pub enum EvalDispatchError {
    #[error("agent run failed: {0}")]
    AgentRun(String),
    #[error("spawn_blocking join failed: {0}")]
    Join(String),
}

/// Load recent messages from the session manager and convert them to the
/// `Message` type expected by [`AgentInput`].
///
/// Mirrors `load_session_history` in `server.rs:3911`.
async fn load_history(
    session_manager: &SessionManager,
    session_id: &str,
    max_messages: u32,
) -> Vec<nevoflux_builtin_wasm::Message> {
    // Fetch max_messages + 1 so we can pop the current user message and still
    // have max_messages of history (mirrors server.rs behaviour).
    match session_manager
        .get_recent_messages(session_id, max_messages + 1)
        .await
    {
        Ok(mut messages) => {
            // Remove the last message (the current user message just saved).
            if !messages.is_empty() {
                messages.pop();
            }
            messages
                .into_iter()
                .filter_map(|msg| match msg.role {
                    nevoflux_storage::MessageRole::User => {
                        Some(nevoflux_builtin_wasm::Message::user(msg.content))
                    }
                    nevoflux_storage::MessageRole::Assistant => {
                        if msg.content.trim().is_empty() {
                            None
                        } else {
                            Some(nevoflux_builtin_wasm::Message::assistant(msg.content))
                        }
                    }
                    // System and Tool messages are not surfaced in history.
                    _ => None,
                })
                .collect()
        }
        Err(e) => {
            warn!(%session_id, error = %e, "load_session_history failed; using empty history");
            vec![]
        }
    }
}

/// Dispatch an agent turn in eval mode.
///
/// Used by `src/main.rs`'s `agent_turn_rx` consumer. Returns when
/// `Agent::run` completes (or errors). During execution, daemon events flow
/// through the EventBus and eval clients see them via the SSE bridge.
///
/// The returned [`nevoflux_builtin_wasm::AgentOutput`] is treated as
/// terminal — the plan re-run path from `server.rs` is intentionally
/// omitted for eval.
pub async fn dispatch_eval_turn(
    svc: &EvalDispatchServices,
    session_id: &str,
    prompt: &str,
) -> Result<nevoflux_builtin_wasm::AgentOutput, EvalDispatchError> {
    info!(%session_id, prompt_len = prompt.len(), "eval dispatch starting");

    // 1. Build a fresh per-session SessionMemoryExtractor.
    //    Mirrors server.rs:4352-4367 (get-or-insert from extraction_registry).
    //    For eval we always create a fresh one; sessions are short-lived.
    let session_extractor = Arc::new(
        crate::learning::session_extractor::SessionMemoryExtractor::new(
            svc.config.learning.extraction_interval,
        ),
    );

    // 2. Clone services and set per-session fields.
    //    We use empty identity/proxy_id (no sidebar proxy in eval).
    //    Mirrors server.rs:4337-4344.
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    let mut services_with_ctx = svc
        .services
        .clone()
        .with_client_context(vec![], String::new())
        .with_session_id(session_id.to_string());
    services_with_ctx.interrupt_flag = interrupt_flag;
    services_with_ctx.session_extractor = Some(session_extractor.clone());

    // 3. Drop-sink for stream chunks — eval has no sidebar to forward to.
    //    We drop the receiver immediately so the channel acts as /dev/null.
    let (stream_tx, stream_rx) = mpsc::unbounded_channel::<SidebarStreamChunk>();
    // Drain asynchronously so the sender never blocks (unbounded channel
    // itself never blocks, but we keep it tidy).
    let _drainer = tokio::spawn(async move {
        let mut rx = stream_rx;
        while rx.recv().await.is_some() {
            // discard
        }
    });

    // 4. Build DaemonHostFunctions via the builder chain.
    //    Mirrors server.rs:4372-4378. We intentionally skip:
    //    - with_trace_collector (no trace file needed for eval)
    //    - with_skill_base_path (no skill invocation context)
    let host = DaemonHostFunctions::new(svc.config.clone(), svc.runtime.clone())
        .with_services(services_with_ctx)
        .with_sidebar_stream(stream_tx)
        .with_session_id(session_id.to_string())
        .with_session_extractor(session_extractor.clone())
        .with_canvas_video_service(svc.canvas_video_service.clone());

    // 5. Notify extractor of the new user message (mirrors server.rs:4388-4389).
    session_extractor.on_user_message();
    session_extractor.reset_turn_flags();

    // 6. Load conversation history.
    let history = load_history(
        &svc.session_manager,
        session_id,
        svc.config.daemon.context.max_history_messages,
    )
    .await;

    // 7. Load enabled MCP server names for system-prompt injection
    //    (mirrors server.rs:4403-4411).
    let mcp_servers: Vec<String> = crate::mcp_config::McpServersConfig::load()
        .map(|c| {
            c.servers
                .iter()
                .filter(|s| s.enabled)
                .map(|s| s.name.clone())
                .collect()
        })
        .unwrap_or_default();

    // 8. Build AgentInput with eval-appropriate defaults.
    //    Mirrors server.rs:4413-4442. Eval-specific overrides:
    //    - mode: AgentMode::Chat (default, lowest tool surface)
    //    - attachments / local_files / tab_ids: empty
    //    - tab_id / skill_context / soul_context: None
    //    - tools_config: Some(ToolsConfig::None) disables all tools so the
    //      agent produces a plain text response (no browser/bash hang risk).
    //      Callers that need tool execution can set this via a wrapper.
    let input = AgentInput {
        session_id: session_id.to_string(),
        mode: AgentMode::default(), // Chat
        user_message: prompt.to_string(),
        history,
        attachments: vec![],
        local_files: vec![],
        custom_system_prompt: None,
        tab_id: None,
        tab_ids: vec![],
        skill_context: None,
        available_models: svc.config.llm.configured_providers(),
        mcp_servers,
        soul_context: None, // knowledge_retriever is None in eval
        tools_config: Some(ToolsConfig::None),
        os_platform: Some(std::env::consts::OS.to_string()),
    };

    // 9. Run Agent::run synchronously on a blocking thread
    //    (mirrors server.rs:4396 + 4655).
    let agent = Agent::new(host);
    let join = tokio::task::spawn_blocking(move || agent.run(&input)).await;

    match join {
        Ok(Ok(output)) => {
            info!(%session_id, "eval agent run complete");
            Ok(output)
        }
        Ok(Err(e)) => {
            error!(%session_id, error = ?e, "agent.run returned error");
            Err(EvalDispatchError::AgentRun(format!("{e:?}")))
        }
        Err(join_err) => {
            error!(%session_id, error = %join_err, "spawn_blocking join failed");
            Err(EvalDispatchError::Join(format!("{join_err}")))
        }
    }
}
