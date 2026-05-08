//! Per-iteration execution (spec §7).
//!
//! ## Phase 9c status (2026-05-08)
//!
//! Phase 9c lights up **real LLM-driven iteration execution** by reusing
//! the production agent host the chat-send path uses. Iteration flow:
//!
//! 1. Read the loop record, increment `iteration_count`, insert a
//!    `loop_iterations` row with `status=running`, emit
//!    `system:loop:iteration_start`.
//! 2. Validate `allowed_tool_classes`; on parse failure record an
//!    iteration error and bump `consecutive_failures`.
//! 3. Build the §7.2 LOOP-CONTEXT block + per-class tool-name allowlist.
//! 4. **Production path** (when [`HostServices`] are wired via
//!    [`IterationExecutor::with_services`]): spawn a
//!    [`crate::agent_host::DaemonHostFunctions`] via the daemon's
//!    [`HostServices::agent_config`] + [`HostServices::runtime_handle`]
//!    snapshots, then call
//!    `nevoflux_builtin_wasm::Agent::new(host).run(&AgentInput { tools_config: Allow(...), ..})`.
//!    The host's pre-LLM tool filter (`Agent::filter_tools`) honors the
//!    allowlist, so no extra runtime guard is needed.
//! 5. **Stub path** (no services — unit tests only): record a no-op
//!    success without invoking an LLM. This preserves Phase-6 test
//!    semantics while production traffic gets real execution.
//! 6. On success → reset `consecutive_failures` to 0; on error → bump.
//!    The dispatcher in `manager.rs` auto-cancels at >= 3 strikes
//!    (spec §8.4).
//! 7. Emit `system:loop:iteration_end` with the final status + a small
//!    `tool_calls_summary` array.
//!
//! ## What is intentionally **NOT** wired here
//!
//! - **Sidebar streaming**. Iterations don't push their delta-chunks to
//!   the chat sidebar; the §7-mandated visibility surface is the sticky
//!   loop card + `system:loop:iteration_end` event, not the streaming
//!   `stream_chunk` channel. So we skip `with_sidebar_stream`.
//! - **Session memory extraction**. Loop iterations would otherwise
//!   pollute `session_memories` with one extraction per tick; we skip
//!   `with_session_extractor` to keep memory pristine.
//! - **Trace collection**. Iterations write their own
//!   `tool_calls_summary` to `loop_iterations.summary_json`; the
//!   per-chat trace collector is intentionally bypassed.
//! - **Canvas video service**. Loops can't render video (the
//!   `canvas_*` tools are in the `Write` class, which isn't in the
//!   default tool-class set); we don't bother wiring
//!   `with_canvas_video_service`. If a loop opts into the `Write`
//!   class, the call will fail with a clear "tool not configured"
//!   error — acceptable for MVP.
//!
//! Phase 12.2 (`time:dynamic` reschedule) reads
//! [`ExecResult::final_text`] in `manager.rs` and parses the
//! `loop-meta` block out of the assistant text.

use crate::loops::events::LoopEvents;
use crate::loops::tool_classes::{filter_tool_names_by_classes, parse_class_list};
use crate::loops::types::LoopId;
use crate::wasm::services::HostServices;
use nevoflux_storage::models::{current_timestamp, IterationStatus, LoopRecord};
use nevoflux_storage::repositories::LoopRepository;
use nevoflux_storage::Database;
use std::sync::Arc;

#[derive(Debug)]
pub enum ExecResult {
    /// Iteration completed successfully.
    Ok,
    /// Iteration completed successfully and carried final assistant text.
    /// Phase 12.2's `time:dynamic` reschedule reads `loop-meta` from this
    /// text. `None` means the iteration ran but produced no text (e.g.
    /// when `HostServices` is not wired — unit tests only).
    OkWithText(Option<String>),
    /// Iteration failed; the string is a short human-readable reason.
    Error(String),
}

impl ExecResult {
    /// True for any successful completion (with or without text).
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok | Self::OkWithText(_))
    }

    /// Final assistant text, when available.
    pub fn final_text(&self) -> Option<&str> {
        match self {
            Self::OkWithText(Some(t)) => Some(t.as_str()),
            _ => None,
        }
    }
}

pub struct IterationExecutor {
    db: Database,
    events: Arc<LoopEvents>,
    /// `HostServices` snapshot for spawning a production
    /// [`crate::agent_host::DaemonHostFunctions`]. `None` in unit tests
    /// → stub success path. Set during server boot via
    /// [`Self::with_services`] (see `manager.rs::start_with_bus`).
    services: Option<HostServices>,
}

impl IterationExecutor {
    pub fn new(db: Database) -> Self {
        Self::new_with_events(db, Arc::new(LoopEvents::new(None)))
    }

    pub fn new_with_events(db: Database, events: Arc<LoopEvents>) -> Self {
        Self {
            db,
            events,
            services: None,
        }
    }

    /// Wire the `HostServices` snapshot the executor uses to invoke a
    /// real production agent. When unset, [`Self::execute`] short-circuits
    /// to the Phase-6 stub path that records the iteration as `ok`
    /// without invoking an LLM. Phase 9c.
    pub fn with_services(mut self, services: HostServices) -> Self {
        self.services = Some(services);
        self
    }

    /// Cheap clone of the underlying Database handle.
    /// Phase 7's LoopManager dispatcher needs this to construct
    /// short-lived `LoopRepository` instances at fire time.
    pub fn database(&self) -> Database {
        self.db.clone()
    }

    /// Run a single iteration. See module-level docs for the
    /// production-vs-stub split and intentionally-skipped wiring.
    pub async fn execute(&self, loop_id: LoopId, fire_reason: String) -> ExecResult {
        let repo = LoopRepository::new(&self.db);
        let now = current_timestamp();

        let rec = match repo.get(loop_id.as_ref()) {
            Ok(Some(r)) => r,
            Ok(None) => return ExecResult::Error(format!("loop {} vanished", loop_id)),
            Err(e) => return ExecResult::Error(e.to_string()),
        };
        let session_id = rec.session_id.clone();

        let seq = match repo.increment_iteration_count(loop_id.as_ref(), now) {
            Ok(s) => s,
            Err(e) => return ExecResult::Error(e.to_string()),
        };
        let iter_id = match repo.insert_iteration(
            loop_id.as_ref(),
            seq,
            now,
            IterationStatus::Running,
        ) {
            Ok(i) => i,
            Err(e) => return ExecResult::Error(e.to_string()),
        };

        self.events
            .iteration_start(&session_id, &loop_id, seq, now, &fire_reason)
            .await;

        // Validate allowed_tool_classes + compute the tool-name allowlist
        // for `AgentInput.tools_config = Allow(...)`. Pre-LLM filter inside
        // `nevoflux_builtin_wasm::Agent::filter_tools` honors this list.
        let allowed_classes = match parse_class_list(&rec.allowed_tool_classes) {
            Ok(s) => s,
            Err(e) => {
                return self
                    .finalize_iteration_error(
                        iter_id,
                        format!("bad allowed_tool_classes: {e}"),
                        &session_id,
                        &loop_id,
                        seq,
                        &rec,
                    )
                    .await;
            }
        };

        let user_message = build_user_message(&rec, seq, &fire_reason);
        let allowlist =
            filter_tool_names_by_classes(&known_chat_tool_catalog(), &allowed_classes);

        // Stub path: no services → record ok without invoking LLM.
        // Preserves Phase-6 test semantics. Production callers always
        // wire services via `LoopManager::start_with_bus`.
        let services = match self.services.as_ref() {
            Some(s) => s.clone(),
            None => {
                tracing::debug!(
                    loop_id = %loop_id,
                    seq = seq,
                    "IterationExecutor: no HostServices wired — skipping LLM (stub path)"
                );
                return self
                    .finalize_iteration_ok(
                        iter_id,
                        None,
                        serde_json::json!([]),
                        &session_id,
                        &loop_id,
                        seq,
                        &rec,
                    )
                    .await;
            }
        };

        // Production path: spawn a real agent host and run one turn.
        // Skip sidebar streaming (sticky card + iteration_end events
        // suffice), session extraction (would pollute memories), and
        // trace collection (we have our own tool_calls_summary).
        let agent_config = match services.agent_config.as_ref() {
            Some(c) => c.clone(),
            None => {
                return self
                    .finalize_iteration_error(
                        iter_id,
                        "HostServices has no agent_config — bug at server boot".into(),
                        &session_id,
                        &loop_id,
                        seq,
                        &rec,
                    )
                    .await;
            }
        };
        let runtime_handle = match services.runtime_handle.as_ref() {
            Some(h) => h.clone(),
            None => {
                return self
                    .finalize_iteration_error(
                        iter_id,
                        "HostServices has no runtime_handle — bug at server boot".into(),
                        &session_id,
                        &loop_id,
                        seq,
                        &rec,
                    )
                    .await;
            }
        };

        let mut services_for_iter = services.clone();
        services_for_iter.session_id = session_id.clone();
        // Reset interrupt flag so a stray prior cancel doesn't poison
        // this iteration. Note: the loop's own cancel_token is checked
        // by `manager.rs::cancel_loop_inner`, not here.
        services_for_iter.reset_interrupt();

        let host = crate::agent_host::DaemonHostFunctions::new(agent_config, runtime_handle)
            .with_services(services_for_iter)
            .with_session_id(session_id.clone());

        let agent = nevoflux_builtin_wasm::Agent::new(host);
        let input = nevoflux_builtin_wasm::AgentInput {
            session_id: session_id.clone(),
            mode: nevoflux_builtin_wasm::AgentMode::Chat,
            user_message,
            history: vec![],
            attachments: vec![],
            local_files: vec![],
            custom_system_prompt: None,
            tab_id: None,
            tab_ids: vec![],
            skill_context: None,
            available_models: vec![],
            mcp_servers: vec![],
            soul_context: None,
            tools_config: Some(nevoflux_protocol::subagent::ToolsConfig::Allow(allowlist)),
            os_platform: Some(std::env::consts::OS.to_string()),
        };

        // `Agent::run` is synchronous; the host functions block on the
        // runtime handle stashed at construction time for any async LLM
        // calls. Wrapping in `spawn_blocking` keeps us from hogging the
        // dispatcher's executor thread.
        let outcome: Result<nevoflux_builtin_wasm::AgentOutput, nevoflux_builtin_wasm::HostError> =
            tokio::task::spawn_blocking(move || agent.run(&input))
                .await
                .unwrap_or_else(|e| {
                    Err(nevoflux_builtin_wasm::HostError {
                        code: 500,
                        message: format!("agent task panicked: {e}"),
                    })
                });

        match outcome {
            Ok(out) => {
                let trace = serde_json::Value::Array(
                    out.tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "name": tc.name,
                                "ok": true,
                            })
                        })
                        .collect(),
                );
                self.finalize_iteration_ok(
                    iter_id,
                    Some(out.text),
                    trace,
                    &session_id,
                    &loop_id,
                    seq,
                    &rec,
                )
                .await
            }
            Err(e) => {
                self.finalize_iteration_error(
                    iter_id,
                    e.message,
                    &session_id,
                    &loop_id,
                    seq,
                    &rec,
                )
                .await
            }
        }
    }

    /// Finish row + emit event for a successful iteration. Resets
    /// `consecutive_failures` to 0.
    async fn finalize_iteration_ok(
        &self,
        iter_id: i64,
        final_text: Option<String>,
        trace_summary: serde_json::Value,
        session_id: &str,
        loop_id: &LoopId,
        seq: i64,
        _rec: &LoopRecord,
    ) -> ExecResult {
        let end_now = current_timestamp();
        let repo = LoopRepository::new(&self.db);
        let _ = repo.set_consecutive_failures(loop_id.as_ref(), 0, end_now);
        let _ = repo.finish_iteration(
            iter_id,
            end_now,
            IterationStatus::Ok,
            None,
            Some(&serde_json::to_string(&trace_summary).unwrap_or_default()),
        );
        self.events
            .iteration_end(session_id, loop_id, seq, end_now, "ok", trace_summary)
            .await;
        ExecResult::OkWithText(final_text)
    }

    /// Finish row + emit event for a failed iteration. Bumps
    /// `consecutive_failures`.
    async fn finalize_iteration_error(
        &self,
        iter_id: i64,
        err: String,
        session_id: &str,
        loop_id: &LoopId,
        seq: i64,
        rec: &LoopRecord,
    ) -> ExecResult {
        let end_now = current_timestamp();
        let repo = LoopRepository::new(&self.db);
        let new_failures = rec.consecutive_failures + 1;
        let _ = repo.set_consecutive_failures(loop_id.as_ref(), new_failures, end_now);
        let _ = repo.finish_iteration(iter_id, end_now, IterationStatus::Error, Some(&err), None);
        self.events
            .iteration_end(
                session_id,
                loop_id,
                seq,
                end_now,
                "error",
                serde_json::json!([]),
            )
            .await;
        ExecResult::Error(err)
    }
}

/// Static catalog of tool names known to be in the chat-mode tool set.
/// Used to translate the loop's `allowed_tool_classes` into an explicit
/// `AgentInput.tools_config = Allow(...)` allowlist.
///
/// Keep in sync with:
/// - `nevoflux_builtin_wasm::Agent::get_chat_tools()` (the actual
///   tool definitions the LLM sees), and
/// - `crate::loops::tool_classes::class_for()` (the class assignment).
///
/// Anything that ends up classified as `Write` but isn't in this list
/// will silently never be exposed to a loop iteration. That's the
/// fail-closed behavior we want.
fn known_chat_tool_catalog() -> Vec<String> {
    [
        // read class
        "read",
        "list_files",
        "fetch_page",
        "dom_query",
        "screenshot",
        "loop.scratchpad.get",
        "memory_search",
        "web_fetch",
        "web_search",
        "browser_query",
        "browser_inspect",
        // scratchpad-write
        "loop.scratchpad.set",
        // event-subscribe
        "events.subscribe",
        // dom-click
        "browser_click",
        "browser_click_by_id",
        "browser_type",
        "browser_type_by_id",
        "browser_fill",
        "browser_fill_by_id",
        "browser_key_press",
        // nav
        "browser_navigate",
        "browser_go_back",
        "browser_go_forward",
        "browser_open_tab",
        "browser_close_tab",
        // write
        "write",
        "edit",
        "bash",
        "create_artifact",
        "browser_edit_artifact",
        "memory_create",
        "canvas_create_composition",
        "canvas_apply_design_md",
        "canvas_create_from_visual_identity",
        "canvas_attach_asset",
        "canvas_render_video",
        // baseline tools always exposed by Agent::get_chat_tools()
        "think",
        "plan",
        "switch_model",
        "ask_user",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Build the §7.2 LOOP-CONTEXT-prefixed user message for an iteration.
/// Public-in-crate for unit testing and for Phase 9c's AgentInput construction.
pub(crate) fn build_user_message(rec: &LoopRecord, sequence: i64, fire_reason: &str) -> String {
    let scratchpad = if rec.scratchpad.is_empty() {
        "(empty)"
    } else {
        rec.scratchpad.as_str()
    };
    let body = rec
        .prompt_text
        .as_deref()
        .unwrap_or("(wrapped skill — Phase 21)");
    format!(
        "<LOOP-CONTEXT>\n\
         loop_id={}\n\
         iteration={}\n\
         trigger={}\n\
         fire_reason={}\n\
         scratchpad_bytes={}\n\
         scratchpad:\n{}\n\
         </LOOP-CONTEXT>\n\
         \n\
         {}",
        rec.id,
        sequence,
        rec.trigger_expr,
        fire_reason,
        rec.scratchpad.len(),
        scratchpad,
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_storage::models::{CreateSessionParams, LoopState};
    use nevoflux_storage::Storage;

    fn sample_loop(id: &str) -> LoopRecord {
        LoopRecord {
            id: id.into(),
            session_id: "s1".into(),
            trigger_expr: "time:5m".into(),
            prompt_text: Some("check PR".into()),
            wrapped_skill: None,
            allowed_tool_classes: vec!["read".into()],
            scratchpad: "k=v".into(),
            state: LoopState::Running,
            consecutive_failures: 0,
            skipped_triggers: 0,
            iteration_count: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn loop_context_block_includes_required_fields() {
        let rec = sample_loop("abcd1234");
        let s = build_user_message(&rec, 1, "time");
        assert!(s.contains("loop_id=abcd1234"));
        assert!(s.contains("iteration=1"));
        assert!(s.contains("trigger=time:5m"));
        assert!(s.contains("fire_reason=time"));
        assert!(s.contains("scratchpad_bytes=3"));
        assert!(s.contains("k=v"));
        assert!(s.contains("check PR"));
    }

    #[test]
    fn loop_context_block_marks_empty_scratchpad() {
        let mut rec = sample_loop("a");
        rec.scratchpad.clear();
        let s = build_user_message(&rec, 1, "time");
        assert!(s.contains("scratchpad_bytes=0"));
        assert!(s.contains("(empty)"));
    }

    #[test]
    fn loop_context_block_falls_back_for_wrapped_skill() {
        let mut rec = sample_loop("a");
        rec.prompt_text = None;
        rec.wrapped_skill = Some(r#"{"name":"video","args":{}}"#.into());
        let s = build_user_message(&rec, 1, "time");
        assert!(s.contains("wrapped skill — Phase 21"));
    }

    #[tokio::test]
    async fn execute_advances_iteration_count_and_writes_row() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        storage.loops().create(&sample_loop("abc")).unwrap();

        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor.execute(LoopId("abc".into()), "time".into()).await;

        assert!(result.is_ok(), "expected ok-variant, got {:?}", result);
        // Stub path (no services wired) — claims success without text.
        assert!(matches!(result, ExecResult::OkWithText(None)));

        let rec = storage.loops().get("abc").unwrap().unwrap();
        assert_eq!(rec.iteration_count, 1);
    }

    #[tokio::test]
    async fn execute_returns_error_for_missing_loop() {
        let storage = Storage::open_in_memory().unwrap();
        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor.execute(LoopId("nope".into()), "time".into()).await;
        assert!(matches!(result, ExecResult::Error(_)));
    }

    #[tokio::test]
    async fn execute_resets_failure_counter_on_success() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let mut rec = sample_loop("rst");
        rec.consecutive_failures = 2;
        storage.loops().create(&rec).unwrap();

        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor.execute(LoopId("rst".into()), "time".into()).await;
        assert!(result.is_ok());

        let after = storage.loops().get("rst").unwrap().unwrap();
        assert_eq!(
            after.consecutive_failures, 0,
            "successful iteration must reset consecutive_failures"
        );
    }

    #[tokio::test]
    async fn execute_bumps_failure_counter_on_class_parse_error() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let mut rec = sample_loop("bad");
        rec.allowed_tool_classes = vec!["bogus-class".into()];
        rec.consecutive_failures = 1;
        storage.loops().create(&rec).unwrap();

        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor.execute(LoopId("bad".into()), "time".into()).await;
        match result {
            ExecResult::Error(e) => assert!(e.contains("bogus-class"), "unexpected msg: {e}"),
            other => panic!("expected Error, got {:?}", other),
        }

        let after = storage.loops().get("bad").unwrap().unwrap();
        assert_eq!(
            after.consecutive_failures, 2,
            "error iteration must bump consecutive_failures"
        );

        // The iteration row was inserted with status=running, then
        // updated to error via finish_iteration. Spot-check via direct
        // SQL since LoopRepository doesn't expose a list-iterations API.
        let (status, err_msg): (String, Option<String>) = storage
            .database()
            .with_connection(|conn| {
                let row = conn.query_row(
                    "SELECT status, error_message FROM loop_iterations WHERE loop_id = ?1",
                    rusqlite::params!["bad"],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )?;
                Ok(row)
            })
            .unwrap();
        assert_eq!(status, "error");
        assert!(err_msg.unwrap_or_default().contains("bogus-class"));
    }
}
