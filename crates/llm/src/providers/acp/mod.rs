//! ACP (Agent Communication Protocol) provider.
//!
//! Communicates with CLI agents (claude-code, gemini-cli) over stdio using the
//! sacp protocol. A background tokio task owns the `ClientToAgent` connection;
//! the public `AcpProvider` sends requests through an `mpsc` channel.

pub mod antigravity;
pub mod claude;
pub mod context;
pub mod gemini;
pub mod mcp_bridge;
pub mod openclaw;
pub mod tools;

// Re-export key schema types so downstream crates (e.g. nevoflux-daemon)
// can construct ContentBlock values without a direct sacp dependency.
pub use sacp::schema::{ContentBlock, StopReason, TextContent};

use sacp::schema::{
    Content, ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, SetSessionModeRequest, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate,
};
use sacp::{ClientToAgent, JrConnectionCx, JrMessage};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from ACP provider operations.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("{0}")]
    Internal(String),
}

impl From<String> for AcpError {
    fn from(s: String) -> Self {
        AcpError::Internal(s)
    }
}

type Result<T> = std::result::Result<T, AcpError>;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for spawning an ACP agent process.
#[derive(Clone, Debug)]
pub struct AcpProviderConfig {
    /// Path to the CLI executable (e.g. `claude` or `gemini`).
    pub command: PathBuf,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Extra environment variables to set.
    pub env: Vec<(String, String)>,
    /// Environment variables to remove before spawning.
    pub env_remove: Vec<String>,
    /// Working directory for the agent session.
    pub work_dir: PathBuf,
    /// Session mode to request (e.g. "plan", "code").
    pub session_mode: String,
    /// When true, use MCP server for native tool calling.
    /// When false, use <tool_call> XML extraction.
    pub use_mcp_bridge: bool,
    /// Whether to inject MCP URL into NewSessionRequest.
    /// false for OpenClaw (registers via gateway config instead).
    pub inject_mcp_url: bool,
    /// When true, the daemon-side HTTP MCP server enforces
    /// `McpToolBridge::request_permission` on every `tools/call` before
    /// executing. Needed for agents that never send
    /// `session/request_permission` themselves (antigravity-acp). Keep false
    /// for agents that self-report (claude-code) or gating would double-prompt.
    pub gate_tool_calls: bool,
    /// Per-session config options to apply after `session/new` via
    /// `session/set_config_option` (list of `(configId, value)`). Used to pass
    /// the model to agents (e.g. antigravity) whose model ids contain spaces
    /// and therefore cannot travel through whitespace-split env/args.
    pub config_options: Vec<(String, String)>,
}

/// Internal request sent from `AcpProvider` to the background client loop.
pub(crate) enum ClientRequest {
    NewSession {
        response_tx: oneshot::Sender<Result<NewSessionResponse>>,
    },
    Prompt {
        session_id: SessionId,
        content: Vec<ContentBlock>,
        response_tx: mpsc::Sender<AcpUpdate>,
    },
    Shutdown,
}

/// Updates streamed back from the ACP agent during a prompt.
#[derive(Debug)]
pub enum AcpUpdate {
    /// Incremental text from the agent's response.
    Text(String),
    /// Incremental thought/reasoning from the agent.
    Thought(String),
    /// The agent has finished responding.
    Complete(StopReason),
    /// A protocol-level error occurred.
    Error(String),
    /// A native tool call (the ACP agent's own Bash/Read/Edit/etc, as opposed
    /// to a NevoFlux MCP tool) reached a terminal status with result content.
    /// NevoFlux MCP tool calls are excluded — those are already recorded by
    /// `execute_mcp_tool` on the daemon side (`record_acp_tool_result`), and
    /// forwarding them here too would double-record.
    ToolResult { tool_name: String, content: String },
}

/// Metadata about a tool call, captured at creation time (`SessionUpdate::ToolCall`)
/// when `title` is guaranteed present, and reused at completion time
/// (`SessionUpdate::ToolCallUpdate`) where the ACP "only changed fields need to be
/// included" convention typically leaves `title` as `None`.
///
/// Without this cache, a completing claude-code tool call — opaque `toolu_...` id, no
/// marker, no title on the update — is misclassified as a native tool and its NevoFlux
/// MCP result (already recorded once by `execute_mcp_tool`) gets forwarded and
/// double-recorded here too.
#[derive(Debug, Clone)]
struct ToolCallMeta {
    /// Whether this tool call was classified as a NevoFlux MCP tool at creation time.
    is_mcp: bool,
    /// Best-effort tool name resolved at creation time, used as a fallback when the
    /// completion notification omits `title`.
    tool_name: Option<String>,
}

/// Per-connection cache of [`ToolCallMeta`] keyed by ACP `tool_call_id`. Populated on
/// `SessionUpdate::ToolCall` (creation) and consulted on `SessionUpdate::ToolCallUpdate`
/// (completion) so the MCP-vs-native classification made at creation time (when `title`
/// is present) survives to the completion notification (when it usually isn't).
/// Cleared at the start of each new prompt to bound its size.
type ToolCallCache = Arc<Mutex<HashMap<String, ToolCallMeta>>>;

/// ACP-based LLM provider that communicates with a CLI agent over stdio.
///
/// Use [`AcpProvider::new`] to create an instance, then [`AcpProvider::connect`]
/// to spawn the process and establish the protocol handshake.
pub struct AcpProvider {
    config: AcpProviderConfig,
    tx: Option<mpsc::Sender<ClientRequest>>,
    /// MCP tool bridge, only present when use_mcp_bridge is true.
    tool_bridge: Option<Arc<mcp_bridge::McpToolBridge>>,
}

impl AcpProvider {
    /// Create a new (disconnected) provider with the given config.
    pub fn new(config: AcpProviderConfig) -> Self {
        let tool_bridge = if config.use_mcp_bridge {
            let bridge = mcp_bridge::McpToolBridge::new();
            bridge.set_gate_tool_calls(config.gate_tool_calls);
            Some(Arc::new(bridge))
        } else {
            None
        };
        Self {
            config,
            tx: None,
            tool_bridge,
        }
    }

    /// Return the MCP tool bridge, if present.
    pub fn tool_bridge(&self) -> Option<&Arc<mcp_bridge::McpToolBridge>> {
        self.tool_bridge.as_ref()
    }

    /// Spawn the agent process and complete the ACP handshake.
    pub async fn connect(&mut self) -> Result<()> {
        let child = spawn_acp_process(&self.config).await?;
        let (tx, rx) = mpsc::channel(32);
        let (init_tx, init_rx) = oneshot::channel();
        let config = self.config.clone();

        let tool_bridge = self.tool_bridge.clone();
        tokio::spawn(async move {
            if let Err(e) = run_client_loop_direct(config, child, rx, init_tx, tool_bridge).await {
                tracing::error!(error = %e, "ACP client loop error");
            }
        });

        let init_result = init_rx
            .await
            .map_err(|_| AcpError::Internal("ACP client initialization cancelled".into()))?;
        let _init_response = init_result?;

        self.tx = Some(tx);
        Ok(())
    }

    /// Whether the background client loop is still running.
    pub fn is_alive(&self) -> bool {
        self.tx.as_ref().is_some_and(|tx| !tx.is_closed())
    }

    /// Reconnect if the background client loop has died.
    pub async fn ensure_connected(&mut self) -> Result<()> {
        if !self.is_alive() {
            tracing::info!("ACP connection lost, reconnecting...");
            self.connect().await?;
        }
        Ok(())
    }

    /// Create a new session on the connected agent.
    pub async fn new_session(&self) -> Result<SessionId> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| AcpError::Internal("ACP provider is not connected".into()))?;

        let (response_tx, response_rx) = oneshot::channel();
        tx.send(ClientRequest::NewSession { response_tx })
            .await
            .map_err(|_| AcpError::Internal("ACP client is unavailable".into()))?;

        let session = response_rx
            .await
            .map_err(|_| AcpError::Internal("ACP session/new cancelled".into()))??;
        Ok(session.session_id)
    }

    /// Send a prompt to the agent and receive streaming updates.
    pub async fn prompt(
        &self,
        session_id: SessionId,
        content: Vec<ContentBlock>,
    ) -> Result<mpsc::Receiver<AcpUpdate>> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| AcpError::Internal("ACP provider is not connected".into()))?;

        let (response_tx, response_rx) = mpsc::channel(64);
        tx.send(ClientRequest::Prompt {
            session_id,
            content,
            response_tx,
        })
        .await
        .map_err(|_| AcpError::Internal("ACP client is unavailable".into()))?;

        Ok(response_rx)
    }

    /// Shut down the agent process gracefully.
    pub async fn shutdown(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(ClientRequest::Shutdown).await;
        }
    }
}

impl Drop for AcpProvider {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            tokio::spawn(async move {
                let _ = tx.send(ClientRequest::Shutdown).await;
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Process spawning
// ---------------------------------------------------------------------------

async fn spawn_acp_process(config: &AcpProviderConfig) -> Result<Child> {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    // Ensure the child process has an extended PATH that includes common
    // locations for npm-installed CLIs (e.g. /usr/local/bin on macOS).
    // This is critical because shebang scripts like `gemini` use
    // `#!/usr/bin/env node` which needs `node` in the child's PATH.
    if let Some(extended_path) = crate::util::build_search_path() {
        cmd.env("PATH", extended_path);
    }

    for key in &config.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &config.env {
        cmd.env(key, value);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    tracing::info!(
        command = %config.command.display(),
        args = ?config.args,
        work_dir = %config.work_dir.display(),
        "ACP: spawning process"
    );

    cmd.spawn()
        .map_err(|e| AcpError::Internal(format!("failed to spawn ACP process: {e}")))
}

// ---------------------------------------------------------------------------
// Background client loop
// ---------------------------------------------------------------------------

async fn run_client_loop_direct(
    config: AcpProviderConfig,
    mut child: Child,
    mut rx: mpsc::Receiver<ClientRequest>,
    init_tx: oneshot::Sender<Result<InitializeResponse>>,
    tool_bridge: Option<Arc<mcp_bridge::McpToolBridge>>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stdin = child.stdin.take().ok_or("no stdin on ACP child process")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("no stdout on ACP child process")?;

    let transport = sacp::ByteStreams::new(stdin.compat_write(), stdout.compat());

    let prompt_response_tx: Arc<Mutex<Option<mpsc::Sender<AcpUpdate>>>> =
        Arc::new(Mutex::new(None));
    let tool_call_cache: ToolCallCache = Arc::new(Mutex::new(HashMap::new()));

    let error_notify_tx = prompt_response_tx.clone();

    let result = ClientToAgent::builder()
        .on_receive_notification(
            {
                let prompt_response_tx = prompt_response_tx.clone();
                let tool_call_cache = tool_call_cache.clone();
                // Use UntypedMessage to catch ALL notifications including unknown ones
                // like `usage_update` which would crash the loop if we used SessionNotification.
                // sacp 10.x / 11.x fails to deserialize `usage_update` variant, causing
                // `Some(Err(parse_error))` which terminates the client loop.
                async move |message: sacp::UntypedMessage, _cx| {
                    // Try to parse as SessionNotification — ignore parse failures silently
                    if let Some(Ok(notification)) =
                        SessionNotification::parse_message(&message.method, &message.params)
                    {
                        if let Some(tx) = prompt_response_tx
                            .lock()
                            .ok()
                            .as_ref()
                            .and_then(|g| g.as_ref())
                        {
                            match notification.update {
                                SessionUpdate::AgentMessageChunk(ContentChunk {
                                    content: ContentBlock::Text(TextContent { text, .. }),
                                    ..
                                }) => {
                                    let _ = tx.try_send(AcpUpdate::Text(text));
                                }
                                SessionUpdate::AgentThoughtChunk(ContentChunk {
                                    content: ContentBlock::Text(TextContent { text, .. }),
                                    ..
                                }) => {
                                    let _ = tx.try_send(AcpUpdate::Thought(text));
                                }
                                SessionUpdate::ToolCall(tc) => {
                                    if let Some(update) =
                                        handle_tool_call_notification(&tool_call_cache, &tc)
                                    {
                                        let _ = tx.try_send(update);
                                    }
                                }
                                SessionUpdate::ToolCallUpdate(update) => {
                                    if let Some(result) = handle_tool_call_update_notification(
                                        &tool_call_cache,
                                        &update,
                                    ) {
                                        let _ = tx.try_send(result);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    // Unknown notifications (usage_update, etc.) are silently ignored
                    Ok(())
                }
            },
            sacp::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let tool_bridge = tool_bridge.clone();
                // Permission handler: forwards to sidebar via McpToolBridge for user approval.
                // Session-level "Always Allow" decisions are cached to avoid repeated prompts.
                async move |request: RequestPermissionRequest, request_cx, _connection_cx| {
                    use mcp_bridge::PermissionResponse;
                    use sacp::schema::SelectedPermissionOutcome;

                    // Extract tool name from toolCallId or title.
                    // Gemini CLI uses toolCallId format: "mcp_nevoflux-tools_<tool_name>-<timestamp>"
                    // Claude Code uses title: "mcp__nevoflux-tools__<tool_name>"
                    let tool_call_id = request.tool_call.tool_call_id.0.to_string();
                    let tool_name = extract_tool_name_from_id(&tool_call_id)
                        .or_else(|| request.tool_call.fields.title.clone())
                        .unwrap_or_default();
                    let args_summary = request
                        .tool_call
                        .fields
                        .raw_input
                        .as_ref()
                        .and_then(|v| serde_json::to_string(v).ok())
                        .unwrap_or_default();

                    // Check with McpToolBridge — may ask sidebar or return cached decision
                    let decision = if let Some(ref bridge) = tool_bridge {
                        bridge.request_permission(&tool_name, &args_summary).await
                    } else {
                        // No bridge (non-MCP mode) — auto-approve
                        PermissionResponse::AllowOnce
                    };

                    // Map decision to ACP option ID
                    let option_id = match decision {
                        PermissionResponse::AllowAlways => {
                            // Pick the per-tool allow_always option (e.g. "proceed_always_tool"),
                            // NOT the per-server option ("proceed_always_server") which would
                            // bypass permission for ALL tools. Prefer option whose name contains
                            // the tool name (Gemini uses "Always Allow <tool_name>").
                            request
                                .options
                                .iter()
                                .find(|o| {
                                    matches!(
                                        o.kind,
                                        sacp::schema::PermissionOptionKind::AllowAlways
                                    ) && o.option_id.0.as_ref().contains("tool")
                                })
                                .or_else(|| {
                                    // Fallback: any allow_always
                                    request.options.iter().find(|o| {
                                        matches!(
                                            o.kind,
                                            sacp::schema::PermissionOptionKind::AllowAlways
                                        )
                                    })
                                })
                                .or_else(|| {
                                    request.options.iter().find(|o| {
                                        matches!(
                                            o.kind,
                                            sacp::schema::PermissionOptionKind::AllowOnce
                                        )
                                    })
                                })
                                .map(|o| o.option_id.0.to_string())
                                .unwrap_or_else(|| "allow".to_string())
                        }
                        PermissionResponse::AllowOnce => {
                            // Pick the allow_once option
                            request
                                .options
                                .iter()
                                .find(|o| {
                                    matches!(o.kind, sacp::schema::PermissionOptionKind::AllowOnce)
                                })
                                .map(|o| o.option_id.0.to_string())
                                .unwrap_or_else(|| "allow".to_string())
                        }
                        PermissionResponse::Reject => {
                            // Pick the reject option, or cancel
                            request
                                .options
                                .iter()
                                .find(|o| {
                                    matches!(o.kind, sacp::schema::PermissionOptionKind::RejectOnce)
                                })
                                .map(|o| o.option_id.0.to_string())
                                .unwrap_or_else(|| "cancel".to_string())
                        }
                    };

                    let response =
                        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(option_id),
                        ));
                    request_cx.respond(response)
                }
            },
            sacp::on_receive_request!(),
        )
        .connect_to(transport)?
        .run_until(|cx: JrConnectionCx<ClientToAgent>| {
            handle_requests(
                config,
                cx,
                &mut rx,
                prompt_response_tx,
                tool_call_cache,
                init_tx,
                tool_bridge,
            )
        })
        .await;

    // If the client loop exits (e.g. due to a parse error from an unknown
    // notification like "usage_update"), notify any active prompt receiver
    // so it doesn't hang forever waiting for AcpUpdate::Complete.
    if let Some(tx) = error_notify_tx.lock().ok().and_then(|mut g| g.take()) {
        let err_msg = match &result {
            Ok(()) => "ACP client loop exited".to_string(),
            Err(e) => format!("ACP client loop error: {e}"),
        };
        // Treat unexpected exit as completion — the agent likely already
        // finished its response (text chunks were delivered via notifications).
        // sacp 10.x doesn't know about "usage_update" which the agent sends
        // AFTER the response is complete, causing a parse error.
        tracing::warn!("{}, sending synthetic completion", err_msg);
        let _ = tx.try_send(AcpUpdate::Complete(StopReason::EndTurn));
    }

    result.map_err(|e| e.into())
}

/// Extract tool name from ACP toolCallId.
/// Gemini CLI: "mcp_nevoflux-tools_browser_get_markdown-1774240394151" → "browser_get_markdown"
/// Claude Code: "toolu_01BKyw4Ubz7YNgaL5vNouGCo" → None (use title instead)
fn extract_tool_name_from_id(tool_call_id: &str) -> Option<String> {
    // Gemini format: mcp_<server>_<tool_name>-<timestamp>
    if tool_call_id.starts_with("mcp_nevoflux-tools_") {
        let rest = &tool_call_id["mcp_nevoflux-tools_".len()..];
        // Remove trailing -<timestamp>
        if let Some(dash_pos) = rest.rfind('-') {
            let name = &rest[..dash_pos];
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // Claude Code format with double underscore
    if tool_call_id.contains("__nevoflux-tools__") {
        let parts: Vec<&str> = tool_call_id.split("__").collect();
        if let Some(name) = parts.last() {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// True when a tool call's id or title identifies it as a NevoFlux MCP tool
/// (browser_*, run_command, etc.), routed through the MCP bridge rather than
/// executed natively by the ACP agent. Those calls are already recorded to
/// `messages` by `execute_mcp_tool`/`record_acp_tool_result` on the daemon
/// side; forwarding them here too would double-record the same result.
fn is_nevoflux_mcp_tool_call(tool_call_id: &str, title: Option<&str>) -> bool {
    fn has_marker(s: &str) -> bool {
        s.contains("mcp_nevoflux-tools_") || s.contains("__nevoflux-tools__")
    }
    has_marker(tool_call_id) || title.is_some_and(has_marker)
}

/// Concatenate the text of a tool call's content blocks into a single
/// string. Non-text content (diffs, terminal embeds) is skipped — verify
/// checks match against text, and we have no generic textual rendering for
/// those variants here.
fn tool_call_content_to_text(content: &[ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolCallContent::Content(Content {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build an [`AcpUpdate::ToolResult`] from a tool call's current snapshot, or
/// `None` when it isn't (yet) a recordable native-tool result: the call
/// hasn't reached a terminal status, it belongs to the NevoFlux MCP bridge
/// (see [`is_nevoflux_mcp_tool_call`]), or it has no text content to record.
///
/// Terminal statuses are `Completed` and `Failed` — a failed native command's
/// output (e.g. Bash stderr) is still useful evidence for `/loop` verify and
/// `/goal` checks, so both are forwarded.
fn maybe_tool_result_update(
    tool_call_id: &str,
    title: Option<&str>,
    status: ToolCallStatus,
    content: &[ToolCallContent],
) -> Option<AcpUpdate> {
    if is_nevoflux_mcp_tool_call(tool_call_id, title) {
        return None;
    }
    build_tool_result_update(tool_call_id, title, status, content)
}

/// Same as [`maybe_tool_result_update`] but without the NevoFlux-MCP-tool skip check.
/// Used by the `SessionUpdate::ToolCallUpdate` (completion) handler, which resolves the
/// MCP-vs-native classification from the [`ToolCallCache`] populated at creation time
/// rather than re-deriving it from the completion notification's (usually absent)
/// `title` field.
fn build_tool_result_update(
    tool_call_id: &str,
    title: Option<&str>,
    status: ToolCallStatus,
    content: &[ToolCallContent],
) -> Option<AcpUpdate> {
    if !matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed) {
        return None;
    }
    let text = tool_call_content_to_text(content);
    if text.trim().is_empty() {
        return None;
    }
    let tool_name = extract_tool_name_from_id(tool_call_id)
        .or_else(|| title.map(|t| t.to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    Some(AcpUpdate::ToolResult {
        tool_name,
        content: text,
    })
}

/// Handle a `SessionUpdate::ToolCall` (creation) notification: `title` is guaranteed
/// present here, so this is the one point where MCP-vs-native classification can be
/// made reliably. Caches the classification (and a best-effort tool name) under
/// `tool_call_id` so the later completing `ToolCallUpdate` — which per the ACP "only
/// changed fields" convention typically omits `title` — can look it back up instead of
/// misclassifying an MCP call as native.
///
/// Also returns a recordable [`AcpUpdate::ToolResult`] in the (uncommon) case where the
/// tool call already arrives in a terminal status at creation time.
fn handle_tool_call_notification(cache: &ToolCallCache, tc: &ToolCall) -> Option<AcpUpdate> {
    let tool_call_id = tc.tool_call_id.0.to_string();
    let is_mcp = is_nevoflux_mcp_tool_call(&tool_call_id, Some(tc.title.as_str()));
    let tool_name = extract_tool_name_from_id(&tool_call_id).or_else(|| Some(tc.title.clone()));
    if let Ok(mut cache) = cache.lock() {
        cache.insert(tool_call_id.clone(), ToolCallMeta { is_mcp, tool_name });
    }
    maybe_tool_result_update(
        &tool_call_id,
        Some(tc.title.as_str()),
        tc.status,
        &tc.content,
    )
}

/// Handle a `SessionUpdate::ToolCallUpdate` (completion, typically) notification.
/// Resolves the MCP-vs-native classification from the [`ToolCallCache`] populated by
/// [`handle_tool_call_notification`] rather than re-deriving it from this notification's
/// own (usually absent) `title` field — this is the fix for the double-recording bug:
/// a claude-code native tool call's `tool_call_id` is an opaque `toolu_...` with no MCP
/// marker, and its completing update carries `title: None`, so without the cache lookup
/// an MCP tool call looks identical to a native one at this point and gets forwarded
/// (and double-recorded) here.
fn handle_tool_call_update_notification(
    cache: &ToolCallCache,
    update: &ToolCallUpdate,
) -> Option<AcpUpdate> {
    let (status, content) = (update.fields.status?, update.fields.content.as_deref()?);
    let tool_call_id = update.tool_call_id.0.to_string();
    let cached = cache
        .lock()
        .ok()
        .and_then(|c| c.get(&tool_call_id).cloned());

    // Prefer the classification cached at creation time (title was present then).
    // Fall back to computing from the update's own fields only when we never saw a
    // creation notification for this tool_call_id (defensive).
    let is_mcp = match &cached {
        Some(meta) => meta.is_mcp,
        None => is_nevoflux_mcp_tool_call(&tool_call_id, update.fields.title.as_deref()),
    };
    if is_mcp {
        return None;
    }

    let title_hint = update
        .fields
        .title
        .as_deref()
        .or_else(|| cached.as_ref().and_then(|m| m.tool_name.as_deref()));
    build_tool_result_update(&tool_call_id, title_hint, status, content)
}

async fn handle_requests(
    config: AcpProviderConfig,
    cx: JrConnectionCx<ClientToAgent>,
    rx: &mut mpsc::Receiver<ClientRequest>,
    prompt_response_tx: Arc<Mutex<Option<mpsc::Sender<AcpUpdate>>>>,
    tool_call_cache: ToolCallCache,
    init_tx: oneshot::Sender<Result<InitializeResponse>>,
    tool_bridge: Option<Arc<mcp_bridge::McpToolBridge>>,
) -> std::result::Result<(), sacp::Error> {
    let mut init_tx = Some(init_tx);

    let init_response = cx
        .send_request(InitializeRequest::new(ProtocolVersion::LATEST))
        .block_task()
        .await
        .map_err(|err| {
            let message = format!("ACP initialize failed: {err}");
            if let Some(tx) = init_tx.take() {
                let _ = tx.send(Err(AcpError::Internal(message.clone())));
            }
            sacp::Error::internal_error().data(message)
        })?;

    if let Some(tx) = init_tx.take() {
        let _ = tx.send(Ok(init_response));
    }

    while let Some(request) = rx.recv().await {
        match request {
            ClientRequest::NewSession { response_tx } => {
                tracing::info!(
                    cwd = %config.work_dir.display(),
                    "ACP: sending NewSessionRequest"
                );
                let mut request = NewSessionRequest::new(config.work_dir.clone());
                if config.inject_mcp_url {
                    if let Some(ref bridge) = tool_bridge {
                        if let Some(url) = bridge.mcp_server_url() {
                            use sacp::schema::McpServerHttp;
                            request.mcp_servers.push(sacp::schema::McpServer::Http(
                                McpServerHttp::new("nevoflux-tools", url),
                            ));
                            tracing::info!("ACP: injecting MCP server URL into session: {}", url);
                        }
                    }
                }
                let session = cx.send_request(request).block_task().await;

                let result = match session {
                    Ok(session) => {
                        let modes_str = session
                            .modes
                            .as_ref()
                            .map(|m| {
                                let available: Vec<&str> = m
                                    .available_modes
                                    .iter()
                                    .map(|mode| mode.id.0.as_ref())
                                    .collect();
                                format!(
                                    "current={}, available=[{}]",
                                    m.current_mode_id.0,
                                    available.join(", ")
                                )
                            })
                            .unwrap_or_else(|| "None".to_string());
                        tracing::info!(
                            session_id = %session.session_id.0,
                            modes = %modes_str,
                            "ACP: session created"
                        );
                        match apply_session_mode(&config, &cx, session).await {
                            Ok(session) => {
                                apply_config_options(&config, &cx, &session).await;
                                Ok(session)
                            }
                            Err(e) => Err(e),
                        }
                    }
                    Err(err) => Err(AcpError::Internal(format!("ACP session/new failed: {err}"))),
                };

                let _ = response_tx.send(result);
            }

            ClientRequest::Prompt {
                session_id,
                content,
                response_tx,
            } => {
                let content_len: usize = content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text(t) => t.text.len(),
                        _ => 0,
                    })
                    .sum();
                tracing::info!(
                    session_id = %session_id.0,
                    content_blocks = content.len(),
                    content_bytes = content_len,
                    "ACP: sending PromptRequest"
                );
                *prompt_response_tx.lock().unwrap() = Some(response_tx.clone());
                // Fresh cache per prompt turn: bounds memory and avoids stale
                // classifications from a prior turn's tool_call_ids leaking forward.
                tool_call_cache.lock().unwrap().clear();

                let response = cx
                    .send_request(PromptRequest::new(session_id, content))
                    .block_task()
                    .await;

                match response {
                    Ok(r) => {
                        tracing::info!(stop_reason = ?r.stop_reason, "ACP: prompt completed");
                        let _ = response_tx.try_send(AcpUpdate::Complete(r.stop_reason));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "ACP: prompt failed");
                        let _ = response_tx.try_send(AcpUpdate::Error(e.to_string()));
                    }
                }

                *prompt_response_tx.lock().unwrap() = None;
            }

            ClientRequest::Shutdown => break,
        }
    }

    Ok(())
}

async fn apply_session_mode(
    config: &AcpProviderConfig,
    cx: &JrConnectionCx<ClientToAgent>,
    session: NewSessionResponse,
) -> Result<NewSessionResponse> {
    if let Some(modes) = session.modes.as_ref() {
        if modes.current_mode_id.0.as_ref() != config.session_mode.as_str() {
            let available: Vec<String> = modes
                .available_modes
                .iter()
                .map(|mode| mode.id.0.to_string())
                .collect();

            if !available.iter().any(|id| id == &config.session_mode) {
                return Err(AcpError::Internal(format!(
                    "Requested mode '{}' not offered by agent. Available modes: {}",
                    config.session_mode,
                    available.join(", ")
                )));
            }

            cx.send_request(SetSessionModeRequest::new(
                session.session_id.clone(),
                config.session_mode.clone(),
            ))
            .block_task()
            .await
            .map_err(|err| {
                AcpError::Internal(format!("ACP agent rejected session/set_mode: {err}"))
            })?;
        }
    }

    Ok(session)
}

/// `session/set_config_option` request. Not in sacp 10.1.0's typed schema, but
/// the antigravity-acp adapter (and newer ACP) implement it. We send it to set
/// agy's model — whose ids contain spaces (e.g. "Gemini 3.5 Flash (Medium)") and
/// therefore cannot ride the whitespace-split `AGY_EXTRA_ARGS`. The adapter maps
/// configId "model" to a discrete `--model <id>` argv element when spawning agy.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SetConfigOptionRequest {
    session_id: sacp::schema::SessionId,
    config_id: String,
    value: String,
}

impl sacp::JrMessage for SetConfigOptionRequest {
    fn method(&self) -> &str {
        "session/set_config_option"
    }
    fn to_untyped_message(&self) -> std::result::Result<sacp::UntypedMessage, sacp::Error> {
        sacp::UntypedMessage::new(self.method(), self)
    }
    fn parse_message(
        _method: &str,
        _params: &impl serde::Serialize,
    ) -> Option<std::result::Result<Self, sacp::Error>> {
        // Outgoing-only request — the agent never sends this to us.
        None
    }
}

impl sacp::JrRequest for SetConfigOptionRequest {
    // The adapter replies with an (often empty) object; accept it untyped.
    type Response = serde_json::Value;
}

/// Apply per-session config options (e.g. model) after `session/new`, via
/// `session/set_config_option`. Failures warn but never abort the session — the
/// agent falls back to its default for that option (better a working chat on the
/// default model than a hard failure).
async fn apply_config_options(
    config: &AcpProviderConfig,
    cx: &JrConnectionCx<ClientToAgent>,
    session: &NewSessionResponse,
) {
    for (config_id, value) in &config.config_options {
        let req = SetConfigOptionRequest {
            session_id: session.session_id.clone(),
            config_id: config_id.clone(),
            value: value.clone(),
        };
        match cx.send_request(req).block_task().await {
            Ok(_) => tracing::info!("ACP: set config option {config_id}={value}"),
            Err(e) => {
                tracing::warn!("ACP: session/set_config_option {config_id}={value} rejected: {e}")
            }
        }
    }
}

#[cfg(test)]
mod tool_result_tests {
    use super::*;
    use sacp::schema::ToolCallUpdateFields;

    fn text_block(text: &str) -> ToolCallContent {
        ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text))))
    }

    // -- is_nevoflux_mcp_tool_call -------------------------------------------------

    #[test]
    fn mcp_tool_call_detected_by_gemini_style_id() {
        assert!(is_nevoflux_mcp_tool_call(
            "mcp_nevoflux-tools_browser_get_markdown-1774240394151",
            None
        ));
    }

    #[test]
    fn mcp_tool_call_detected_by_claude_style_title() {
        assert!(is_nevoflux_mcp_tool_call(
            "toolu_01BKyw4Ubz7YNgaL5vNouGCo",
            Some("mcp__nevoflux-tools__run_command")
        ));
    }

    #[test]
    fn native_tool_call_not_detected_as_mcp() {
        assert!(!is_nevoflux_mcp_tool_call(
            "toolu_01BKyw4Ubz7YNgaL5vNouGCo",
            Some("Bash")
        ));
    }

    // -- tool_call_content_to_text -------------------------------------------------

    #[test]
    fn concatenates_multiple_text_blocks_with_newline() {
        let content = vec![text_block("exit 0"), text_block("OK")];
        assert_eq!(tool_call_content_to_text(&content), "exit 0\nOK");
    }

    #[test]
    fn ignores_non_text_content_blocks() {
        let diff = ToolCallContent::Diff(sacp::schema::Diff::new("/tmp/f.txt", "new"));
        let content = vec![diff, text_block("kept")];
        assert_eq!(tool_call_content_to_text(&content), "kept");
    }

    #[test]
    fn empty_content_yields_empty_string() {
        assert_eq!(tool_call_content_to_text(&[]), "");
    }

    // -- maybe_tool_result_update ---------------------------------------------------

    #[test]
    fn records_completed_native_bash_result() {
        let content = vec![text_block("total 0\n-rw-r--r-- 1 a b 0 f.txt")];
        let update = maybe_tool_result_update(
            "toolu_01BKyw4Ubz7YNgaL5vNouGCo",
            Some("Bash"),
            ToolCallStatus::Completed,
            &content,
        );
        match update {
            Some(AcpUpdate::ToolResult { tool_name, content }) => {
                assert_eq!(tool_name, "Bash");
                assert_eq!(content, "total 0\n-rw-r--r-- 1 a b 0 f.txt");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn records_failed_native_result_too() {
        let content = vec![text_block("command not found: frobnicate")];
        let update =
            maybe_tool_result_update("toolu_02x", Some("Bash"), ToolCallStatus::Failed, &content);
        assert!(matches!(update, Some(AcpUpdate::ToolResult { .. })));
    }

    #[test]
    fn skips_pending_and_in_progress_status() {
        let content = vec![text_block("partial output")];
        for status in [ToolCallStatus::Pending, ToolCallStatus::InProgress] {
            let update = maybe_tool_result_update("toolu_03x", Some("Bash"), status, &content);
            assert!(update.is_none(), "status {status:?} should not record");
        }
    }

    #[test]
    fn skips_nevoflux_mcp_tool_calls_to_avoid_double_recording() {
        let content = vec![text_block("<html>page</html>")];
        let update = maybe_tool_result_update(
            "mcp_nevoflux-tools_browser_get_markdown-1774240394151",
            None,
            ToolCallStatus::Completed,
            &content,
        );
        assert!(update.is_none());
    }

    #[test]
    fn skips_empty_content() {
        let update =
            maybe_tool_result_update("toolu_04x", Some("Bash"), ToolCallStatus::Completed, &[]);
        assert!(update.is_none());
    }

    #[test]
    fn falls_back_to_title_when_id_has_no_extractable_name() {
        let content = vec![text_block("ok")];
        let update = maybe_tool_result_update(
            "toolu_05x",
            Some("Read(/etc/hosts)"),
            ToolCallStatus::Completed,
            &content,
        );
        match update {
            Some(AcpUpdate::ToolResult { tool_name, .. }) => {
                assert_eq!(tool_name, "Read(/etc/hosts)");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    // -- tool_call_cache: creation -> completion (reproduces the production bug) ----
    //
    // claude-code's tool_call_id is an opaque `toolu_...` with no MCP marker. Per the
    // ACP "only changed fields" convention, `title` is present on the creation
    // notification (SessionUpdate::ToolCall) but absent on the completing
    // ToolCallUpdate. Before the cache fix, `maybe_tool_result_update` was called
    // directly on the completion notification's own fields — title: None — so an MCP
    // tool call was misclassified as native and forwarded, double-recording it
    // alongside `execute_mcp_tool`'s own record.

    fn empty_cache() -> ToolCallCache {
        Arc::new(Mutex::new(std::collections::HashMap::new()))
    }

    #[test]
    fn mcp_tool_completion_is_skipped_even_though_update_has_no_title() {
        let cache = empty_cache();

        // Creation notification: title IS present and identifies the NevoFlux MCP tool.
        let creation = ToolCall::new("toolu_ABC", "mcp__nevoflux-tools__browser_get_markdown");
        let creation_result = handle_tool_call_notification(&cache, &creation);
        // Not terminal at creation (default status), so nothing to forward yet.
        assert!(creation_result.is_none());

        // Completion notification: same tool_call_id, but per the ACP "only changed
        // fields" convention, title is None here — this is the exact production shape.
        let completion = ToolCallUpdate::new(
            "toolu_ABC",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![text_block("<html>page</html>")]),
        );
        let completion_result = handle_tool_call_update_notification(&cache, &completion);

        assert!(
            completion_result.is_none(),
            "MCP tool completion must be skipped (already recorded by execute_mcp_tool), \
             got {completion_result:?}"
        );
    }

    #[test]
    fn native_tool_completion_is_forwarded_even_though_update_has_no_title() {
        let cache = empty_cache();

        // Creation notification for a genuine native tool (e.g. claude-code's own Bash).
        let creation = ToolCall::new("toolu_BASH1", "Bash");
        let creation_result = handle_tool_call_notification(&cache, &creation);
        assert!(creation_result.is_none());

        // Completion notification: title omitted, same as production.
        let completion = ToolCallUpdate::new(
            "toolu_BASH1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![text_block("total 0\n-rw-r--r-- 1 a b 0 f.txt")]),
        );
        let completion_result = handle_tool_call_update_notification(&cache, &completion);

        match completion_result {
            Some(AcpUpdate::ToolResult { tool_name, content }) => {
                assert_eq!(tool_name, "Bash");
                assert_eq!(content, "total 0\n-rw-r--r-- 1 a b 0 f.txt");
            }
            other => panic!("expected native ToolResult to be forwarded, got {other:?}"),
        }
    }

    #[test]
    fn update_without_prior_creation_falls_back_to_own_fields() {
        // Defensive path: no cache entry (e.g. creation notification was missed).
        let cache = empty_cache();
        let completion = ToolCallUpdate::new(
            "mcp_nevoflux-tools_browser_get_markdown-1774240394151",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![text_block("<html>page</html>")]),
        );
        let result = handle_tool_call_update_notification(&cache, &completion);
        assert!(
            result.is_none(),
            "id-based MCP marker should still be caught without a cache entry"
        );
    }
}
