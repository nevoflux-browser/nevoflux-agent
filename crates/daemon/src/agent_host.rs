//! Host function implementation for the Agent.
//!
//! This module provides the daemon's implementation of the `HostFunctions` trait
//! from `nevoflux-builtin-wasm`, bridging the Agent to actual services.
//!
//! # Subagent Execution
//!
//! Subagents can be executed in two modes:
//!
//! 1. **WASM Sandboxed Mode** (preferred): When a `SubagentExecutor` is available
//!    via `HostServices`, subagents run in isolated WASM instances with resource
//!    limits (memory, fuel, timeout).
//!
//! 2. **Legacy Mode**: When no executor is available, subagents run as Tokio tasks
//!    using the internal registry. This provides no sandboxing.
//!
//! The sandboxed mode ensures sub-agents cannot:
//! - Access parent agent's memory
//! - Run indefinitely without limits
//! - Access resources without going through the host function boundary

use crate::config::AgentConfig;
use crate::context::{CompressionResult, ContextCompressor, ContextMessage, TokenBudget};
use crate::trace::collector::TraceCollector;
use crate::wasm::llm::{
    execute_llm_chat, start_llm_stream, LlmChatRequest, LlmMessage as DaemonLlmMessage,
    LlmStreamRegistry,
};
use crate::wasm::HostServices;
use nevoflux_builtin_wasm::{
    Agent, AgentInput, AgentMode, AgentOutput, BashResult, BashStatus, BrowserToolResult,
    GrepMatch, GrepResult, HostError, HostFunctions, HostResult, LlmRequest, LlmResponse,
    MemoryChunk, ReadResult, SkillSummary, SubagentInfo, ToolSearchResult,
};
use nevoflux_llm::ProviderType;
use nevoflux_mcp::ToolResultContent;
use nevoflux_protocol::BrowserToolAction;
use nevoflux_storage::VectorSearchResult;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

/// Status of a sub-agent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

impl SubagentStatus {
    fn as_str(&self) -> &'static str {
        match self {
            SubagentStatus::Running => "running",
            SubagentStatus::Completed => "completed",
            SubagentStatus::Failed => "failed",
            SubagentStatus::Killed => "killed",
        }
    }
}

/// Entry for tracking a spawned sub-agent.
struct SubagentEntry {
    /// Task description.
    task: String,
    /// Execution mode.
    mode: String,
    /// Current status.
    status: SubagentStatus,
    /// Result text (set when completed).
    result: Option<String>,
    /// Channel to signal completion (used by wait).
    completion_rx: Option<oneshot::Receiver<String>>,
}

/// Registry for managing sub-agents.
struct SubagentRegistry {
    /// Next available sub-agent ID.
    next_id: AtomicU64,
    /// Map of sub-agent ID to entry.
    entries: RwLock<HashMap<u64, SubagentEntry>>,
}

impl SubagentRegistry {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            entries: RwLock::new(HashMap::new()),
        }
    }

    fn allocate_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Trace metadata stored during a streaming LLM call.
struct StreamTraceData {
    /// When the stream was started.
    start: std::time::Instant,
    /// Serialized request payload (captured before the request is consumed).
    request: Option<serde_json::Value>,
    /// Accumulated text content from all stream chunks.
    accumulated_text: String,
    /// Accumulated tool calls from all stream chunks.
    accumulated_tool_calls: Vec<crate::wasm::llm::LlmToolCall>,
}

/// A streaming chunk to send to the sidebar.
#[derive(Debug, Clone)]
pub struct SidebarStreamChunk {
    /// The incremental text content.
    pub text: String,
    /// Whether this is the final chunk.
    pub done: bool,
    /// Optional tool event to send alongside the text stream.
    pub event: Option<nevoflux_protocol::ToolEvent>,
    /// Optional thinking event for reasoning content.
    pub thinking_event: Option<nevoflux_protocol::ThinkingEvent>,
}

/// Daemon's implementation of HostFunctions for the Agent.
///
/// This bridges the WASM agent's host function calls to the actual daemon services.
pub struct DaemonHostFunctions {
    /// Agent configuration (contains LLM settings).
    config: Arc<AgentConfig>,
    /// Tokio runtime handle for async operations.
    runtime: Handle,
    /// Optional services for tool search and dynamic tool calls.
    services: Option<HostServices>,
    /// Registry for tracking sub-agents.
    subagent_registry: Arc<SubagentRegistry>,
    /// Registry for tracking LLM streams.
    stream_registry: Arc<LlmStreamRegistry>,
    /// Optional sender for streaming chunks to the sidebar.
    sidebar_stream_tx: Option<tokio::sync::mpsc::UnboundedSender<SidebarStreamChunk>>,
    /// Current session ID for browser tool requests.
    session_id: Option<String>,
    /// Optional trace collector for recording LLM call spans.
    trace_collector: Option<Arc<TraceCollector>>,
    /// Current agent iteration counter (for trace recording).
    current_iteration: AtomicU32,
    /// Trace metadata for in-flight streaming LLM calls, keyed by stream_id.
    stream_trace_data: Arc<Mutex<HashMap<u64, StreamTraceData>>>,
    /// Override for the active LLM provider (set via switch_model tool).
    model_override_provider: Arc<Mutex<Option<String>>>,
    /// Override for the active LLM model (set via switch_model tool).
    model_override_model: Arc<Mutex<Option<String>>>,
    /// Base path for skill's auxiliary files (for relative path resolution in tool_read/tool_write).
    skill_base_path: Option<std::path::PathBuf>,
    /// Sandbox directory for subagent writes. When set, tool_write and tool_edit
    /// are restricted to paths within this directory.
    subagent_sandbox: Option<String>,
    /// Current thinking block ID for reasoning→ThinkingEvent conversion.
    current_thinking_id: Arc<Mutex<Option<String>>>,
}

impl DaemonHostFunctions {
    /// Create a new DaemonHostFunctions with the given configuration.
    pub fn new(config: Arc<AgentConfig>, runtime: Handle) -> Self {
        Self {
            config,
            runtime,
            services: None,
            subagent_registry: Arc::new(SubagentRegistry::new()),
            stream_registry: Arc::new(LlmStreamRegistry::new()),
            sidebar_stream_tx: None,
            session_id: None,
            trace_collector: None,
            current_iteration: AtomicU32::new(0),
            stream_trace_data: Arc::new(Mutex::new(HashMap::new())),
            model_override_provider: Arc::new(Mutex::new(None)),
            model_override_model: Arc::new(Mutex::new(None)),
            skill_base_path: None,
            subagent_sandbox: None,
            current_thinking_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Add services to enable tool search and dynamic tool calls.
    pub fn with_services(mut self, services: HostServices) -> Self {
        self.services = Some(services);
        self
    }

    /// Add a sidebar stream sender for streaming chunks.
    pub fn with_sidebar_stream(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<SidebarStreamChunk>,
    ) -> Self {
        self.sidebar_stream_tx = Some(tx);
        self
    }

    /// Set the session ID for browser tool requests.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set the trace collector for recording LLM call spans.
    pub fn with_trace_collector(mut self, collector: Arc<TraceCollector>) -> Self {
        self.trace_collector = Some(collector);
        self
    }

    /// Set the skill base path for resolving relative file paths in tool_read/tool_write.
    pub fn with_skill_base_path(mut self, base_path: impl Into<std::path::PathBuf>) -> Self {
        self.skill_base_path = Some(base_path.into());
        self
    }

    /// Set the subagent sandbox directory for write/edit path restrictions.
    pub fn with_subagent_sandbox(mut self, path: String) -> Self {
        self.subagent_sandbox = Some(path);
        self
    }

    /// Validate that a write/edit target path is within the subagent sandbox.
    /// Returns Ok(()) for main agents (sandbox=None) or if path is in sandbox.
    /// Returns Err(403) if path escapes the sandbox.
    fn validate_sandbox_path(&self, path: &str) -> HostResult<()> {
        if let Some(ref sandbox) = self.subagent_sandbox {
            let canonical_result = std::path::Path::new(path).canonicalize();
            let check_path = if let Ok(canonical) = canonical_result {
                canonical
            } else {
                // File doesn't exist yet -- check parent directory
                std::path::Path::new(path)
                    .parent()
                    .and_then(|p| p.canonicalize().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from(path))
            };
            if !check_path.starts_with(sandbox) {
                return Err(HostError {
                    code: 403,
                    message: format!(
                        "Subagent writes restricted to {}. \
                         Return content to main agent for writing elsewhere.",
                        sandbox
                    ),
                });
            }
        }
        Ok(())
    }

    /// Resolve a file path using skill_base_path if available.
    ///
    /// For reads:
    /// - Relative paths are resolved against skill_base_path
    /// - Absolute paths that don't exist fall back to skill_base_path/filename
    ///
    /// For writes:
    /// - Only relative paths are resolved (no fallback for absolute paths)
    ///
    /// Returns None if no resolution is possible (no skill_base_path set).
    fn resolve_skill_path(
        &self,
        path: &str,
        allow_absolute_fallback: bool,
    ) -> Option<std::path::PathBuf> {
        use std::path::{Component, Path};

        let skill_base = self.skill_base_path.as_ref()?;
        let p = Path::new(path);

        // Security: reject paths with ".." components to prevent path traversal
        if p.components().any(|c| matches!(c, Component::ParentDir)) {
            warn!("Rejecting path with traversal components: {}", path);
            return None;
        }

        if p.is_relative() {
            // Relative path: resolve against skill base
            let resolved = skill_base.join(p);
            debug!(
                "Resolved relative skill path: {} -> {}",
                path,
                resolved.display()
            );
            Some(resolved)
        } else if allow_absolute_fallback && !p.exists() {
            // Absolute path that doesn't exist: try filename in skill directory
            if let Some(filename) = p.file_name() {
                let fallback = skill_base.join(filename);
                if fallback.exists() {
                    debug!(
                        "Resolved absolute path via skill fallback: {} -> {}",
                        path,
                        fallback.display()
                    );
                    return Some(fallback);
                }
            }
            None
        } else {
            None
        }
    }

    /// Update the current iteration counter for trace recording.
    pub fn set_iteration(&self, iteration: u32) {
        self.current_iteration.store(iteration, Ordering::Relaxed);
    }

    /// Record a tool execution span if trace collection is enabled.
    #[allow(clippy::too_many_arguments)]
    fn record_tool(
        &self,
        tool_name: &str,
        params_summary: Option<String>,
        success: bool,
        error_code: Option<String>,
        error_msg: Option<String>,
        duration_ms: u64,
        full_params: Option<serde_json::Value>,
        full_result: Option<serde_json::Value>,
    ) {
        if let Some(tc) = &self.trace_collector {
            let session_id = self.session_id.as_deref().unwrap_or("unknown");
            let iteration = self.current_iteration.load(Ordering::Relaxed);
            tc.record_tool_exec(
                session_id,
                iteration,
                tool_name,
                params_summary,
                success,
                error_code,
                error_msg,
                duration_ms,
                full_params,
                full_result,
            );
        }
    }

    /// Convert agent LlmRequest to daemon LlmChatRequest with custom messages.
    ///
    /// This is used when context compression has produced a different set of messages.
    fn convert_request_with_messages(
        &self,
        request: &LlmRequest,
        context_messages: &[ContextMessage],
    ) -> LlmChatRequest {
        // Convert context messages to daemon messages
        let messages: Vec<DaemonLlmMessage> = context_messages
            .iter()
            .map(|m| DaemonLlmMessage {
                role: m.role.clone(),
                content: m.content.clone(),
                tool_calls: None,
                tool_call_id: None,
                attachments: Vec::new(),
            })
            .collect();

        // Extract system message from compressed messages (may include summary)
        let system = context_messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone());

        // Convert tools from original request
        let tools = if request.tools.is_empty() {
            None
        } else {
            Some(
                request
                    .tools
                    .iter()
                    .map(|t| crate::wasm::llm::LlmToolDefinition {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.input_schema.clone(),
                    })
                    .collect(),
            )
        };

        LlmChatRequest {
            messages,
            system,
            temperature: Some(self.config.llm.temperature),
            max_tokens: Some(self.config.llm.max_tokens),
            tools,
        }
    }

    /// Convert agent LlmRequest to daemon LlmChatRequest.
    fn convert_request_to_daemon(&self, request: &LlmRequest) -> LlmChatRequest {
        // Convert messages to daemon format
        let messages: Vec<DaemonLlmMessage> = request
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    nevoflux_builtin_wasm::MessageRole::System => "system",
                    nevoflux_builtin_wasm::MessageRole::User => "user",
                    nevoflux_builtin_wasm::MessageRole::Assistant => "assistant",
                    nevoflux_builtin_wasm::MessageRole::Tool => "tool",
                };
                // Convert tool_calls from builtin-wasm format to daemon format
                let tool_calls = if m.tool_calls.is_empty() {
                    None
                } else {
                    Some(
                        m.tool_calls
                            .iter()
                            .map(|tc| crate::wasm::llm::LlmToolCall {
                                id: tc.id.clone(),
                                call_id: tc.call_id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                                signature: tc.signature.clone(),
                            })
                            .collect(),
                    )
                };
                // Convert attachments from builtin-wasm format to daemon format
                let attachments = m
                    .attachments
                    .iter()
                    .map(|a| crate::wasm::llm::LlmAttachment {
                        name: a.name.clone(),
                        mime_type: a.mime_type.clone(),
                        data: a.data.clone(),
                    })
                    .collect();

                DaemonLlmMessage {
                    role: role.to_string(),
                    content: m.content.clone(),
                    tool_calls,
                    tool_call_id: m.tool_call_id.clone(),
                    attachments,
                }
            })
            .collect();

        // Extract system message
        let system = request
            .messages
            .iter()
            .find(|m| matches!(m.role, nevoflux_builtin_wasm::MessageRole::System))
            .map(|m| m.content.clone());

        // Convert tools
        let tools = if request.tools.is_empty() {
            None
        } else {
            Some(
                request
                    .tools
                    .iter()
                    .map(|t| crate::wasm::llm::LlmToolDefinition {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.input_schema.clone(),
                    })
                    .collect(),
            )
        };

        LlmChatRequest {
            messages,
            system,
            temperature: Some(self.config.llm.temperature),
            max_tokens: Some(self.config.llm.max_tokens),
            tools,
        }
    }

    /// Resolve the active provider name, API key, and model.
    /// Uses model override if set, otherwise falls back to config.
    fn resolve_provider_and_model(&self) -> Result<(String, String, String), HostError> {
        let override_provider = self.model_override_provider.lock().unwrap().clone();
        let override_model = self.model_override_model.lock().unwrap().clone();

        if let (Some(provider), Some(model)) = (override_provider, override_model) {
            let api_key = self.get_api_key_for_provider(&provider)?;
            Ok((provider, api_key, model))
        } else {
            let provider = self
                .config
                .llm
                .active_provider()
                .ok_or_else(|| HostError {
                    code: 1,
                    message: "No LLM provider configured".into(),
                })?
                .to_string();
            let api_key = self
                .config
                .llm
                .active_api_key()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| HostError {
                    code: 2,
                    message: "No API key configured".into(),
                })?
                .to_string();
            let model = self
                .config
                .llm
                .active_model()
                .unwrap_or("gpt-4o-mini")
                .to_string();
            Ok((provider, api_key, model))
        }
    }

    /// Get API key for a specific provider from config or environment.
    fn get_api_key_for_provider(&self, provider: &str) -> Result<String, HostError> {
        // First check config struct for known providers
        let key = match provider {
            "anthropic" => self.config.llm.anthropic.api_key.as_deref(),
            "openai" => self.config.llm.openai.api_key.as_deref(),
            "qwen" => self.config.llm.qwen.api_key.as_deref(),
            "deepseek" => self.config.llm.deepseek.api_key.as_deref(),
            "openrouter" => self.config.llm.openrouter.api_key.as_deref(),
            "claude-code" | "claude_code" => self.config.llm.claude_code.api_key.as_deref(),
            "kimi-agent" | "kimi_agent" | "kimi" => self.config.llm.kimi_agent.api_key.as_deref(),
            _ => None,
        };

        if let Some(k) = key.filter(|k| !k.is_empty()) {
            return Ok(k.to_string());
        }

        // Fall back to environment variable
        let pt = ProviderType::from_str(provider).map_err(|_| HostError {
            code: 3,
            message: format!("Invalid provider: {}", provider),
        })?;
        let env_var = nevoflux_llm::api_key_env_var(pt);
        match std::env::var(env_var) {
            Ok(k) => Ok(k),
            Err(_) if pt == ProviderType::ClaudeCode => {
                // Claude Code CLI manages its own auth; return a placeholder
                Ok("claude-code-cli".to_string())
            }
            Err(_) if pt == ProviderType::KimiAgent => {
                // Kimi Agent CLI manages its own auth; return a placeholder
                Ok("kimi-agent-cli".to_string())
            }
            Err(_) => Err(HostError {
                code: 2,
                message: format!("No API key found for provider: {}", provider),
            }),
        }
    }
}

/// Expand ~ to actual home directory in a file path.
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&path[2..]);
        }
    }
    std::path::PathBuf::from(path)
}

impl HostFunctions for DaemonHostFunctions {
    fn llm_chat(&self, request: &LlmRequest) -> HostResult<LlmResponse> {
        // Resolve provider (uses override if set, otherwise config)
        let (provider_name, api_key, model) = self.resolve_provider_and_model()?;

        let provider = ProviderType::from_str(&provider_name).map_err(|_| HostError {
            code: 3,
            message: format!("Invalid provider: {}", provider_name),
        })?;

        debug!(
            "llm_chat: provider={}, model={}, messages={}",
            provider_name,
            model,
            request.messages.len()
        );

        // Convert request messages to ContextMessage for compression check
        let context_messages: Vec<ContextMessage> = request
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    nevoflux_builtin_wasm::MessageRole::System => "system",
                    nevoflux_builtin_wasm::MessageRole::User => "user",
                    nevoflux_builtin_wasm::MessageRole::Assistant => "assistant",
                    nevoflux_builtin_wasm::MessageRole::Tool => "tool",
                };
                ContextMessage {
                    role: role.to_string(),
                    content: m.content.clone(),
                }
            })
            .collect();

        // Estimate tokens and calculate budget
        let estimated_tokens = ContextCompressor::estimate_tokens(&context_messages);
        let token_budget = TokenBudget::for_model(
            self.config.llm.context_window(),
            self.config.llm.max_tokens,
            &self.config.daemon.context,
        );

        // Attempt compression if needed
        let compressor = ContextCompressor::new(self.config.clone(), self.runtime.clone());
        let compression_result = compressor.compress_if_needed(
            &context_messages,
            estimated_tokens,
            token_budget.for_history,
        );

        // Convert to daemon request
        // IMPORTANT: When no compression happens, use convert_request_to_daemon() to preserve
        // tool_calls and tool_call_id fields. Only use convert_request_with_messages() when
        // compression actually modified the messages.
        let daemon_request = match compression_result {
            CompressionResult::Compressed {
                summary,
                recent,
                saved,
            } => {
                debug!("Compressed context, saved {} tokens", saved);
                // Prepend summary to recent messages
                let mut final_messages = vec![ContextMessage {
                    role: "system".into(),
                    content: format!("[Conversation summary]\n{}", summary),
                }];
                final_messages.extend(recent);
                // Use convert_request_with_messages for compressed messages
                // Note: This will lose tool_calls - compression and tool calling are incompatible
                self.convert_request_with_messages(request, &final_messages)
            }
            CompressionResult::NotNeeded | CompressionResult::Skipped { .. } => {
                // No compression - use direct conversion to preserve tool_calls and tool_call_id
                debug!("No compression needed, preserving tool_calls");
                self.convert_request_to_daemon(request)
            }
        };

        // Serialize the request for trace recording before it's consumed by execute_llm_chat
        let trace_request_value = if self.trace_collector.is_some() {
            serde_json::to_value(&daemon_request).ok()
        } else {
            None
        };

        let llm_start = std::time::Instant::now();

        // Execute LLM call synchronously using block_in_place
        // (allows blocking in async context by moving to blocking thread pool)
        let runtime = self.runtime.clone();
        let result = tokio::task::block_in_place(|| {
            runtime.block_on(async {
                execute_llm_chat(provider, &api_key, &model, daemon_request).await
            })
        });

        match result {
            Ok(response) => {
                let duration_ms = llm_start.elapsed().as_millis() as u64;
                let iteration = self.current_iteration.load(Ordering::Relaxed);
                if let Some(tc) = &self.trace_collector {
                    let session_id = self.session_id.as_deref().unwrap_or("unknown");
                    let full_response = serde_json::to_value(&response).ok();
                    tc.record_llm_call(
                        session_id,
                        iteration,
                        true,
                        None,
                        None,
                        duration_ms,
                        trace_request_value,
                        full_response,
                    );
                }

                // Convert tool calls, preserving call_id for OpenAI Responses API compatibility
                let tool_calls = response
                    .tool_calls
                    .unwrap_or_default()
                    .into_iter()
                    .map(|tc| nevoflux_builtin_wasm::ToolCall {
                        id: tc.id,
                        call_id: tc.call_id,
                        name: tc.name,
                        arguments: tc.arguments,
                        signature: tc.signature,
                    })
                    .collect();

                Ok(LlmResponse {
                    text: response.content,
                    tool_calls,
                })
            }
            Err(e) => {
                let duration_ms = llm_start.elapsed().as_millis() as u64;
                let iteration = self.current_iteration.load(Ordering::Relaxed);
                if let Some(tc) = &self.trace_collector {
                    let session_id = self.session_id.as_deref().unwrap_or("unknown");
                    tc.record_llm_call(
                        session_id,
                        iteration,
                        false,
                        Some(format!("{:?}", e)),
                        Some(e.to_string()),
                        duration_ms,
                        trace_request_value,
                        None,
                    );
                }
                error!("LLM chat failed: {}", e);
                Err(HostError {
                    code: 100,
                    message: format!("LLM error: {}", e),
                })
            }
        }
    }

    fn llm_stream_start(&self, request: &LlmRequest) -> HostResult<u64> {
        // Resolve provider (uses override if set, otherwise config)
        let (provider_name, api_key, model) = self.resolve_provider_and_model()?;

        let provider = ProviderType::from_str(&provider_name).map_err(|_| HostError {
            code: 3,
            message: format!("Invalid provider: {}", provider_name),
        })?;

        debug!(
            "llm_stream_start: provider={}, model={}, messages={}",
            provider_name,
            model,
            request.messages.len()
        );

        // Convert request to daemon format
        let daemon_request = self.convert_request_to_daemon(request);

        // Serialize request for trace recording before it's consumed
        let trace_request_value = self
            .trace_collector
            .as_ref()
            .and_then(|_| serde_json::to_value(&daemon_request).ok());
        let llm_start = std::time::Instant::now();

        // Start the stream
        let registry = Arc::clone(&self.stream_registry);

        let stream_id = self
            .runtime
            .block_on(async {
                start_llm_stream(provider, &api_key, &model, daemon_request, registry).await
            })
            .map_err(|e| HostError {
                code: 100,
                message: format!("Failed to start stream: {}", e),
            })?;

        // Store trace data for this stream
        if self.trace_collector.is_some() {
            self.stream_trace_data.lock().unwrap().insert(
                stream_id,
                StreamTraceData {
                    start: llm_start,
                    request: trace_request_value,
                    accumulated_text: String::new(),
                    accumulated_tool_calls: Vec::new(),
                },
            );
        }

        debug!("llm_stream_start: stream_id={}", stream_id);
        Ok(stream_id)
    }

    fn llm_stream_next(
        &self,
        stream_id: u64,
    ) -> HostResult<Option<nevoflux_builtin_wasm::LlmChunk>> {
        match self.stream_registry.next_chunk(stream_id) {
            Ok(Some(chunk)) => {
                // Accumulate for trace recording
                if self.trace_collector.is_some() {
                    if let Ok(mut trace_map) = self.stream_trace_data.lock() {
                        if let Some(data) = trace_map.get_mut(&stream_id) {
                            if let Some(ref text) = chunk.text {
                                data.accumulated_text.push_str(text);
                            }
                            data.accumulated_tool_calls.extend(chunk.tool_calls.clone());
                        }
                    }
                }

                // Convert reasoning content to ThinkingEvent for sidebar display
                if let Some(ref reasoning) = chunk.reasoning {
                    let thinking_id = {
                        let mut guard = self.current_thinking_id.lock().unwrap();
                        match guard.as_ref() {
                            Some(id) => id.clone(),
                            None => {
                                let id = uuid::Uuid::new_v4().to_string();
                                *guard = Some(id.clone());
                                let _ = self.stream_thinking_event(
                                    nevoflux_protocol::ThinkingEvent::Start {
                                        thinking_id: id.clone(),
                                    },
                                );
                                id
                            }
                        }
                    };
                    let _ = self.stream_thinking_event(nevoflux_protocol::ThinkingEvent::Delta {
                        thinking_id,
                        content: reasoning.clone(),
                    });
                } else if chunk.text.is_some() || chunk.done {
                    // Reasoning ended — text chunk or done signal after reasoning
                    let ended_id = self.current_thinking_id.lock().unwrap().take();
                    if let Some(id) = ended_id {
                        let _ = self.stream_thinking_event(nevoflux_protocol::ThinkingEvent::End {
                            thinking_id: id,
                            duration_ms: None,
                        });
                    }
                }

                // Convert daemon chunk to builtin-wasm chunk, preserving call_id
                let wasm_chunk = nevoflux_builtin_wasm::LlmChunk {
                    text: chunk.text,
                    tool_calls: chunk
                        .tool_calls
                        .into_iter()
                        .map(|tc| nevoflux_builtin_wasm::ToolCall {
                            id: tc.id,
                            call_id: tc.call_id,
                            name: tc.name,
                            arguments: tc.arguments,
                            signature: tc.signature,
                        })
                        .collect(),
                    done: chunk.done,
                };
                Ok(Some(wasm_chunk))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(HostError {
                code: 100,
                message: format!("Stream error: {}", e),
            }),
        }
    }

    fn llm_stream_close(&self, stream_id: u64) -> HostResult<()> {
        debug!("llm_stream_close: stream_id={}", stream_id);

        // End any open thinking block before closing the stream
        let ended_id = self.current_thinking_id.lock().unwrap().take();
        if let Some(id) = ended_id {
            let _ = self.stream_thinking_event(nevoflux_protocol::ThinkingEvent::End {
                thinking_id: id,
                duration_ms: None,
            });
        }

        // Record trace for this stream
        if let Some(tc) = &self.trace_collector {
            let trace_data = self.stream_trace_data.lock().unwrap().remove(&stream_id);
            if let Some(data) = trace_data {
                let duration_ms = data.start.elapsed().as_millis() as u64;
                let iteration = self.current_iteration.load(Ordering::Relaxed);
                let session_id = self.session_id.as_deref().unwrap_or("unknown");
                let full_response =
                    if data.accumulated_text.is_empty() && data.accumulated_tool_calls.is_empty() {
                        None
                    } else {
                        Some(serde_json::json!({
                            "content": data.accumulated_text,
                            "tool_calls": data.accumulated_tool_calls,
                        }))
                    };
                tc.record_llm_call(
                    session_id,
                    iteration,
                    true,
                    None,
                    None,
                    duration_ms,
                    data.request,
                    full_response,
                );
            }
        }

        self.stream_registry.close(stream_id);
        Ok(())
    }

    fn memory_search(&self, query: &str, limit: usize) -> HostResult<Vec<MemoryChunk>> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!("memory_search: query='{}', limit={}", query, limit);

        let fetch_limit = limit * 3; // Fetch more candidates for merging

        // Path 1: FTS5 keyword search
        let fts_results = services
            .database
            .memory()
            .search_fts(query, fetch_limit)
            .map_err(|e| HostError {
                code: 100,
                message: format!("Memory search failed: {}", e),
            })?;

        // Path 2: Vector semantic search (if embedding provider is available)
        let semantic_results = if let Some(ref provider) = services.embedding {
            let runtime = self.runtime.clone();
            let provider = Arc::clone(provider);
            let query_owned = query.to_string();
            let embed_result = tokio::task::block_in_place(|| {
                runtime.block_on(async { provider.embed(&query_owned).await })
            });
            match embed_result {
                Ok(query_emb) => {
                    if let Ok(idx) = services.vector_index.read() {
                        idx.search(&query_emb, fetch_limit)
                    } else {
                        vec![]
                    }
                }
                Err(e) => {
                    warn!("Failed to generate query embedding: {}", e);
                    vec![]
                }
            }
        } else {
            vec![]
        };

        // If no semantic results, return FTS results directly (existing behavior)
        if semantic_results.is_empty() {
            return Ok(fts_results
                .into_iter()
                .take(limit)
                .map(|chunk| MemoryChunk {
                    id: chunk.id,
                    content: chunk.content,
                    session_id: chunk.session_id,
                    score: 1.0,
                })
                .collect());
        }

        // Hybrid merge: combine FTS and semantic scores
        let merged =
            merge_search_results(&fts_results, &semantic_results, limit, &services.database);
        Ok(merged)
    }

    fn memory_create(&self, content: &str, metadata: &serde_json::Value) -> HostResult<String> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!(
            "memory_create: content_len={}, metadata={}",
            content.len(),
            metadata
        );

        // Create a new memory chunk using the storage crate
        let chunk = nevoflux_storage::MemoryChunk::new(content).with_metadata(metadata.clone());
        let chunk_id = chunk.id.clone();

        services
            .database
            .memory()
            .create(&chunk)
            .map_err(|e| HostError {
                code: 100,
                message: format!("Memory create failed: {}", e),
            })?;

        // Generate embedding and update vector index if provider is available
        if let Some(ref provider) = services.embedding {
            let runtime = self.runtime.clone();
            let provider = Arc::clone(provider);
            let content_owned = content.to_string();
            let embed_result = tokio::task::block_in_place(|| {
                runtime.block_on(async { provider.embed(&content_owned).await })
            });
            match embed_result {
                Ok(embedding) => {
                    // Persist embedding in storage
                    if let Err(e) = services
                        .database
                        .memory()
                        .update_embedding(&chunk_id, &embedding)
                    {
                        warn!("Failed to persist embedding for chunk {}: {}", chunk_id, e);
                    }
                    // Add to in-memory vector index
                    if let Ok(mut idx) = services.vector_index.write() {
                        idx.add(&chunk_id, embedding);
                    }
                }
                Err(e) => {
                    warn!("Failed to generate embedding for chunk {}: {}", chunk_id, e);
                }
            }
        }

        Ok(chunk_id)
    }

    fn memory_update(&self, id: &str, content: &str) -> HostResult<()> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!("memory_update: id={}, content_len={}", id, content.len());

        let updated = services
            .database
            .memory()
            .update(id, content)
            .map_err(|e| HostError {
                code: 100,
                message: format!("Memory update failed: {}", e),
            })?;

        if !updated {
            return Err(HostError {
                code: 404,
                message: format!("Memory chunk not found: {}", id),
            });
        }

        Ok(())
    }

    fn memory_delete(&self, id: &str) -> HostResult<()> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!("memory_delete: id={}", id);

        let deleted = services
            .database
            .memory()
            .delete(id)
            .map_err(|e| HostError {
                code: 100,
                message: format!("Memory delete failed: {}", e),
            })?;

        if !deleted {
            return Err(HostError {
                code: 404,
                message: format!("Memory chunk not found: {}", id),
            });
        }

        Ok(())
    }

    fn skill_list(&self) -> HostResult<Vec<SkillSummary>> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        // Load skills from filesystem if registry is empty (lazy loading)
        self.ensure_skills_loaded(services);

        // Now read the summaries
        let registry = services.skills.blocking_read();
        let summaries = registry.list();

        // Log skills found for injection into system prompt
        if summaries.is_empty() {
            info!("skill_list: no skills found, system prompt will not include skills section");
        } else {
            let skill_names: Vec<&str> = summaries.iter().map(|s| s.name.as_str()).collect();
            info!(
                "skill_list: found {} skills for system prompt injection: {:?}",
                summaries.len(),
                skill_names
            );
        }

        // Convert skills::SkillSummary to builtin_wasm::SkillSummary
        Ok(summaries
            .into_iter()
            .map(|s| SkillSummary {
                name: s.name,
                description: s.description,
                tags: s.tags,
            })
            .collect())
    }

    fn skill_load(&self, name: &str) -> HostResult<String> {
        let start = std::time::Instant::now();
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!("skill_load: name={}", name);

        // Load skills from filesystem if registry is empty (lazy loading)
        self.ensure_skills_loaded(services);

        let registry = services.skills.blocking_read();
        let result = registry.get(name).ok_or_else(|| HostError {
            code: 404,
            message: format!("Skill not found: {}", name),
        });

        let duration_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok(skill) => {
                self.record_tool(
                    "skill_load",
                    Some(format!("name={}", name)),
                    true,
                    None,
                    None,
                    duration_ms,
                    Some(serde_json::json!({ "name": name })),
                    None,
                );
                Ok(skill.content.clone())
            }
            Err(e) => {
                self.record_tool(
                    "skill_load",
                    Some(format!("name={}", name)),
                    false,
                    Some(e.code.to_string()),
                    Some(e.message.clone()),
                    duration_ms,
                    Some(serde_json::json!({ "name": name })),
                    None,
                );
                Err(e)
            }
        }
    }

    fn skill_read(&self, name: &str, path: &str) -> HostResult<String> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!("skill_read: name={}, path={}", name, path);

        // Load skills from filesystem if registry is empty (lazy loading)
        self.ensure_skills_loaded(services);

        let registry = services.skills.blocking_read();
        let content = registry
            .read_auxiliary_file(name, path)
            .map_err(|e| HostError {
                code: match e {
                    nevoflux_skills::SkillsError::NotFound(_) => 404,
                    _ => 100,
                },
                message: format!("Skill read failed: {}", e),
            })?;

        Ok(content)
    }

    fn skill_execute(
        &self,
        name: &str,
        script: &str,
        args: &serde_json::Value,
    ) -> HostResult<String> {
        let start = std::time::Instant::now();
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!(
            "skill_execute: name={}, script={}, args={}",
            name, script, args
        );

        // Load skills from filesystem if registry is empty (lazy loading)
        self.ensure_skills_loaded(services);

        let registry = services.skills.blocking_read();
        let result = registry
            .execute_script(name, script, args)
            .map_err(|e| HostError {
                code: match e {
                    nevoflux_skills::SkillsError::NotFound(_) => 404,
                    nevoflux_skills::SkillsError::ExecutionError(_) => 500,
                    _ => 100,
                },
                message: format!("Skill execute failed: {}", e),
            });

        let duration_ms = start.elapsed().as_millis() as u64;
        let params_summary = Some(format!("name={}, script={}", name, script));
        match result {
            Ok(output) => {
                self.record_tool(
                    "skill_execute",
                    params_summary,
                    true,
                    None,
                    None,
                    duration_ms,
                    Some(serde_json::json!({ "name": name, "script": script })),
                    None,
                );
                Ok(output)
            }
            Err(e) => {
                self.record_tool(
                    "skill_execute",
                    params_summary,
                    false,
                    Some(e.code.to_string()),
                    Some(e.message.clone()),
                    duration_ms,
                    Some(serde_json::json!({ "name": name, "script": script })),
                    None,
                );
                Err(e)
            }
        }
    }

    fn tool_read(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> HostResult<ReadResult> {
        use std::fs;
        use std::io::{BufRead, BufReader};

        let start = std::time::Instant::now();
        debug!(
            "tool_read: path={}, offset={:?}, limit={:?}",
            path, offset, limit
        );

        let resolved_path = self
            .resolve_skill_path(path, true)
            .unwrap_or_else(|| std::path::PathBuf::from(path));

        let result: HostResult<ReadResult> = (|| {
            let file = fs::File::open(resolved_path.as_path()).map_err(|e| HostError {
                code: 1,
                message: format!("Failed to open file: {}", e),
            })?;

            let metadata = file.metadata().map_err(|e| HostError {
                code: 1,
                message: format!("Failed to read metadata: {}", e),
            })?;
            let total_bytes = metadata.len();

            let reader = BufReader::new(file);
            let all_lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
            let total_lines = all_lines.len() as u64;

            let off = offset.unwrap_or(0) as usize;
            let lim = limit.unwrap_or(200) as usize;

            let selected: Vec<&String> = all_lines.iter().skip(off).take(lim).collect();
            let returned_lines = selected.len() as u64;

            // Truncate individual long lines
            let content: String = selected
                .iter()
                .map(|line| {
                    if line.len() > 2000 {
                        format!("{}\u{2026}[truncated]", &line[..2000])
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            let truncated = (off + lim) < all_lines.len();

            Ok(ReadResult {
                total_lines,
                total_bytes,
                returned_lines,
                offset: off as u64,
                content,
                truncated,
            })
        })();

        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => self.record_tool(
                "tool_read",
                Some(format!("path={}", path)),
                true,
                None,
                None,
                duration_ms,
                Some(serde_json::json!({ "path": path, "offset": offset, "limit": limit })),
                None,
            ),
            Err(e) => self.record_tool(
                "tool_read",
                Some(format!("path={}", path)),
                false,
                Some(e.code.to_string()),
                Some(e.message.clone()),
                duration_ms,
                Some(serde_json::json!({ "path": path, "offset": offset, "limit": limit })),
                None,
            ),
        }
        result
    }

    fn tool_write(&self, path: &str, content: &str) -> HostResult<()> {
        use std::fs;

        let start = std::time::Instant::now();
        self.validate_sandbox_path(path)?;

        // Resolve path using skill base if available (no absolute fallback for writes)
        let resolved_path = self
            .resolve_skill_path(path, false)
            .unwrap_or_else(|| std::path::PathBuf::from(path));

        let result = fs::write(resolved_path.as_path(), content).map_err(|e| HostError {
            code: 1,
            message: format!("Failed to write file: {}", e),
        });

        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(()) => self.record_tool(
                "tool_write",
                Some(format!("path={}", path)),
                true,
                None,
                None,
                duration_ms,
                Some(serde_json::json!({ "path": path })),
                None,
            ),
            Err(e) => self.record_tool(
                "tool_write",
                Some(format!("path={}", path)),
                false,
                Some(e.code.to_string()),
                Some(e.message.clone()),
                duration_ms,
                Some(serde_json::json!({ "path": path })),
                None,
            ),
        }
        result
    }

    fn tool_edit(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> HostResult<()> {
        use std::fs;

        let start = std::time::Instant::now();
        self.validate_sandbox_path(path)?;
        let result = (|| {
            let content = fs::read_to_string(path).map_err(|e| HostError {
                code: 1,
                message: format!("Failed to read file: {}", e),
            })?;

            let new_content = if replace_all {
                content.replace(old_string, new_string)
            } else {
                content.replacen(old_string, new_string, 1)
            };

            fs::write(path, new_content).map_err(|e| HostError {
                code: 1,
                message: format!("Failed to write file: {}", e),
            })
        })();

        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(()) => self.record_tool(
                "tool_edit",
                Some(format!("path={}", path)),
                true,
                None,
                None,
                duration_ms,
                Some(serde_json::json!({ "path": path })),
                None,
            ),
            Err(e) => self.record_tool(
                "tool_edit",
                Some(format!("path={}", path)),
                false,
                Some(e.code.to_string()),
                Some(e.message.clone()),
                duration_ms,
                Some(serde_json::json!({ "path": path })),
                None,
            ),
        }
        result
    }

    fn tool_bash(&self, command: &str, timeout_ms: Option<u64>) -> HostResult<BashResult> {
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));
        let cmd_summary: String = command.chars().take(100).collect();
        let max_lines: usize = 200;
        let max_bytes: usize = 50 * 1024; // 50KB

        let (shell, shell_flag) = if cfg!(target_os = "windows") {
            ("powershell", "-Command")
        } else {
            ("bash", "-c")
        };

        let mut cmd = Command::new(shell);
        cmd.arg(shell_flag)
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Set up process group on Unix for clean timeout kills
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::other(format!("setsid failed: {}", e)))
            });
        }

        // On Windows, hide the console window for shell subprocesses
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let child = cmd.spawn().map_err(|e| HostError {
            code: 1,
            message: format!("Failed to spawn command: {}", e),
        })?;

        #[cfg(unix)]
        let child_pid = child.id();

        let runtime = self.runtime.clone();
        let output_result: Result<std::process::Output, String> =
            tokio::task::block_in_place(|| {
                runtime.block_on(async {
                    let wait_future = tokio::task::spawn_blocking(move || child.wait_with_output());

                    match tokio::time::timeout(timeout, wait_future).await {
                        Ok(Ok(output)) => output.map_err(|e| format!("Command failed: {}", e)),
                        Ok(Err(e)) => Err(format!("Task join error: {}", e)),
                        Err(_) => {
                            // Kill the entire process group on timeout
                            #[cfg(unix)]
                            {
                                use nix::sys::signal::{killpg, Signal};
                                use nix::unistd::Pid;
                                let _ = killpg(Pid::from_raw(child_pid as i32), Signal::SIGKILL);
                            }
                            Err(format!("Command timed out after {}ms", timeout.as_millis()))
                        }
                    }
                })
            });

        let host_result: HostResult<BashResult> = match output_result {
            Ok(output) => {
                let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let raw_stderr = String::from_utf8_lossy(&output.stderr).to_string();

                let all_lines: Vec<&str> = raw_stdout.lines().collect();
                let total_lines = all_lines.len() as u64;
                let total_bytes = raw_stdout.len() as u64;

                // Truncate by byte count and line count
                let mut truncated = false;
                let stdout = if raw_stdout.len() > max_bytes {
                    truncated = true;
                    // Find the last newline within max_bytes to avoid cutting mid-line
                    let truncated_str = &raw_stdout[..max_bytes];
                    match truncated_str.rfind('\n') {
                        Some(pos) => truncated_str[..pos].to_string(),
                        None => truncated_str.to_string(),
                    }
                } else if all_lines.len() > max_lines {
                    truncated = true;
                    all_lines[..max_lines].join("\n")
                } else {
                    raw_stdout.clone()
                };

                let returned_lines = stdout.lines().count() as u64;

                let status = if output.status.success() {
                    BashStatus::Success
                } else {
                    BashStatus::Error
                };

                let stderr = if !output.status.success() && !raw_stderr.is_empty() {
                    Some(raw_stderr)
                } else {
                    None
                };

                Ok(BashResult {
                    exit_code: output.status.code(),
                    status,
                    total_lines,
                    total_bytes,
                    returned_lines,
                    stdout,
                    stderr,
                    truncated,
                    hint: None,
                })
            }
            Err(e) => Ok(BashResult {
                exit_code: None,
                status: BashStatus::Timeout,
                total_lines: 0,
                total_bytes: 0,
                returned_lines: 0,
                stdout: String::new(),
                stderr: None,
                truncated: false,
                hint: Some(e),
            }),
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        match &host_result {
            Ok(_) => self.record_tool(
                "tool_bash",
                Some(cmd_summary),
                true,
                None,
                None,
                duration_ms,
                Some(
                    serde_json::json!({ "command": command.chars().take(500).collect::<String>() }),
                ),
                None,
            ),
            Err(e) => self.record_tool(
                "tool_bash",
                Some(cmd_summary),
                false,
                Some(e.code.to_string()),
                Some(e.message.clone()),
                duration_ms,
                Some(
                    serde_json::json!({ "command": command.chars().take(500).collect::<String>() }),
                ),
                None,
            ),
        }
        host_result
    }

    fn tool_glob(&self, pattern: &str, path: Option<&str>) -> HostResult<Vec<String>> {
        let start = std::time::Instant::now();
        let full_pattern = match path {
            Some(p) => format!("{}/{}", p, pattern),
            None => pattern.to_string(),
        };

        let result: HostResult<Vec<String>> = glob::glob(&full_pattern)
            .map_err(|e| HostError {
                code: 1,
                message: format!("Invalid glob pattern: {}", e),
            })
            .map(|paths| {
                paths
                    .filter_map(|r| r.ok())
                    .map(|p| p.display().to_string())
                    .collect()
            });

        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => self.record_tool(
                "tool_glob",
                Some(format!("pattern={}", pattern)),
                true,
                None,
                None,
                duration_ms,
                Some(serde_json::json!({ "pattern": pattern, "path": path })),
                None,
            ),
            Err(e) => self.record_tool(
                "tool_glob",
                Some(format!("pattern={}", pattern)),
                false,
                Some(e.code.to_string()),
                Some(e.message.clone()),
                duration_ms,
                Some(serde_json::json!({ "pattern": pattern, "path": path })),
                None,
            ),
        }
        result
    }

    fn tool_grep(
        &self,
        pattern: &str,
        path: Option<&str>,
        file_type: Option<&str>,
        case_insensitive: Option<bool>,
        max_results: Option<u64>,
    ) -> HostResult<GrepResult> {
        use grep_regex::RegexMatcherBuilder;
        use grep_searcher::{sinks::UTF8, SearcherBuilder};

        let start = std::time::Instant::now();
        let search_path = path.unwrap_or(".");
        let max = max_results.unwrap_or(50) as usize;

        let result: HostResult<GrepResult> = (|| {
            let matcher = RegexMatcherBuilder::new()
                .case_insensitive(case_insensitive.unwrap_or(false))
                .build(pattern)
                .map_err(|e| HostError {
                    code: 1,
                    message: format!("Invalid regex pattern: {}", e),
                })?;

            // Set up file type filtering
            let mut types_builder = ignore::types::TypesBuilder::new();
            types_builder.add_defaults();
            if let Some(ft) = file_type {
                types_builder.select(ft);
            }
            let types = types_builder.build().map_err(|e| HostError {
                code: 1,
                message: format!("Invalid file type: {}", e),
            })?;

            let mut all_matches: Vec<GrepMatch> = Vec::new();
            let mut file_set: std::collections::HashSet<String> = std::collections::HashSet::new();

            // Walk directories respecting .gitignore
            let walker = ignore::WalkBuilder::new(search_path).types(types).build();

            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let file_path = entry.path().to_string_lossy().to_string();

                let mut searcher = SearcherBuilder::new().build();

                let _ = searcher.search_path(
                    &matcher,
                    entry.path(),
                    UTF8(|line_num, line| {
                        file_set.insert(file_path.clone());
                        all_matches.push(GrepMatch {
                            file: file_path.clone(),
                            line: line_num,
                            content: line.trim_end().to_string(),
                        });
                        Ok(true)
                    }),
                );
            }

            let total_matches = all_matches.len() as u64;
            let total_files = file_set.len() as u64;
            let returned = std::cmp::min(all_matches.len(), max);
            let truncated = all_matches.len() > max;

            Ok(GrepResult {
                total_matches,
                total_files,
                returned: returned as u64,
                results: all_matches.into_iter().take(max).collect(),
                truncated,
            })
        })();

        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => self.record_tool(
                "tool_grep",
                Some(format!("pattern={}", pattern)),
                true,
                None,
                None,
                duration_ms,
                Some(serde_json::json!({ "pattern": pattern, "path": path })),
                None,
            ),
            Err(e) => self.record_tool(
                "tool_grep",
                Some(format!("pattern={}", pattern)),
                false,
                Some(e.code.to_string()),
                Some(e.message.clone()),
                duration_ms,
                Some(serde_json::json!({ "pattern": pattern, "path": path })),
                None,
            ),
        }
        result
    }

    fn tool_web_search(&self, query: &str) -> HostResult<String> {
        debug!("tool_web_search: query={}", query);

        // Request sidebar to perform web search
        let browser_result = self.execute_browser_action(
            BrowserToolAction::WebSearch,
            serde_json::json!({
                "query": query,
                "max_results": 10,
                "timeout_ms": 30000
            }),
            None, // WebSearch doesn't need tab_id
        )?;

        if !browser_result.success {
            let error_msg = browser_result
                .error
                .unwrap_or_else(|| "Failed to perform web search".into());
            return Err(HostError {
                code: 7001,
                message: error_msg,
            });
        }

        // Extract search results from response
        let result_data = browser_result.data.ok_or_else(|| HostError {
            code: 7001,
            message: "No result data from web search".into(),
        })?;

        let results = result_data["results"].as_array().ok_or_else(|| HostError {
            code: 7001,
            message: "No results array in response".into(),
        })?;

        // Format results as markdown
        let mut output = format!("# Search Results for: {}\n\n", query);

        if results.is_empty() {
            output.push_str("No results found.\n");
        } else {
            for (i, result) in results.iter().enumerate() {
                let title = result["title"].as_str().unwrap_or("Untitled");
                let url = result["url"].as_str().unwrap_or("");
                let snippet = result["snippet"].as_str().unwrap_or("");

                output.push_str(&format!(
                    "{}. **[{}]({})**\n   {}\n\n",
                    i + 1,
                    title,
                    url,
                    snippet
                ));
            }

            // Add total count if available
            if let Some(total) = result_data["total_results"].as_u64() {
                output.push_str(&format!("---\n*Total results: {}*\n", total));
            }
        }

        debug!("tool_web_search: found {} results", results.len());
        Ok(output)
    }

    fn tool_web_fetch(&self, url: &str, prompt: &str) -> HostResult<String> {
        debug!("tool_web_fetch: url={}, prompt={}", url, prompt);

        // Step 1: Request sidebar to fetch URL and save to cache file
        let browser_result = self.execute_browser_action(
            BrowserToolAction::WebFetch,
            serde_json::json!({
                "url": url,
                "timeout_ms": 30000,
                "include_images": false,
                "max_length": 100000
            }),
            None, // WebFetch doesn't need tab_id
        )?;

        if !browser_result.success {
            let error_msg = browser_result
                .error
                .unwrap_or_else(|| "Failed to fetch URL".into());
            return Err(HostError {
                code: 6001,
                message: error_msg,
            });
        }

        // Step 2: Extract result data from response
        let result_data = browser_result.data.ok_or_else(|| HostError {
            code: 6001,
            message: "No result data from web fetch".into(),
        })?;

        let page_title = result_data["title"].as_str().unwrap_or("Untitled");

        // Step 3: Get markdown content — prefer inline _markdown from sidebar,
        // fall back to reading cache file on disk.
        // The browser extension includes content directly in _markdown since
        // it cannot write to the local filesystem (sandbox restriction).
        let markdown_content = if let Some(inline) = result_data["_markdown"].as_str() {
            debug!(
                "tool_web_fetch: got inline markdown, title={}, len={}",
                page_title,
                inline.len()
            );
            // Opportunistically save to cache for future use
            if let Some(file_path) = result_data["file_path"].as_str() {
                let expanded = expand_tilde(file_path);
                if let Some(parent) = expanded.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&expanded, inline);
            }
            inline.to_string()
        } else if let Some(file_path) = result_data["file_path"].as_str() {
            debug!(
                "tool_web_fetch: reading cache file={}, title={}",
                file_path, page_title
            );
            let expanded = expand_tilde(file_path);
            std::fs::read_to_string(&expanded).map_err(|e| HostError {
                code: 6005,
                message: format!("Failed to read cache file '{}': {}", expanded.display(), e),
            })?
        } else {
            return Err(HostError {
                code: 6001,
                message: "No content in web fetch response (no _markdown or file_path)".into(),
            });
        };

        // Step 4: If prompt is empty, return raw content
        if prompt.trim().is_empty() {
            return Ok(format!(
                "# {}\n\nSource: {}\n\n{}",
                page_title, url, markdown_content
            ));
        }

        // Step 5: Process content with LLM using the prompt
        let processed_result =
            Self::process_web_content_with_llm(self, &markdown_content, prompt, url, page_title)?;

        Ok(processed_result)
    }

    fn tool_ask_user(&self, question: &str, options: &[String]) -> HostResult<String> {
        debug!(
            "tool_ask_user: question='{}', options={:?}",
            question, options
        );

        // Request sidebar to show user prompt and wait for response
        let browser_result = self.execute_browser_action(
            BrowserToolAction::AskUser,
            serde_json::json!({
                "question": question,
                "options": options,
                "allow_custom": true,
                "timeout_ms": 60000  // 60 second timeout for user response
            }),
            None, // AskUser doesn't need tab_id
        )?;

        if !browser_result.success {
            let error_msg = browser_result
                .error
                .unwrap_or_else(|| "Failed to get user response".into());
            return Err(HostError {
                code: 8001,
                message: error_msg,
            });
        }

        // Extract answer from response
        let result_data = browser_result.data.ok_or_else(|| HostError {
            code: 8001,
            message: "No result data from ask user".into(),
        })?;

        let answer = result_data["answer"].as_str().ok_or_else(|| HostError {
            code: 8001,
            message: "No answer in response".into(),
        })?;

        debug!("tool_ask_user: received answer='{}'", answer);
        Ok(answer.to_string())
    }

    fn permission_request(
        &self,
        resource_type: &str,
        action: &str,
        resource: &str,
    ) -> HostResult<bool> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!(
            "permission_request: resource_type={}, action={}, resource={}",
            resource_type, action, resource
        );

        // Check if permission already exists
        let check_params =
            nevoflux_storage::CheckPermissionParams::new(resource_type, action, resource);

        let existing = services
            .database
            .permissions()
            .check(check_params)
            .map_err(|e| HostError {
                code: 100,
                message: format!("Permission check failed: {}", e),
            })?;

        // If permission already exists, return the existing result
        if let Some(granted) = existing {
            debug!(
                "permission_request: existing permission found, granted={}",
                granted
            );
            return Ok(granted);
        }

        // No existing permission - create a new granted permission
        // In a full implementation, this could prompt the user via browser extension
        let create_params =
            nevoflux_storage::CreatePermissionParams::new(resource_type, action, resource)
                .with_scope(nevoflux_storage::PermissionScope::Session)
                .with_granted(true);

        services
            .database
            .permissions()
            .create(create_params)
            .map_err(|e| HostError {
                code: 100,
                message: format!("Permission create failed: {}", e),
            })?;

        debug!("permission_request: new permission created and granted");
        Ok(true)
    }

    fn permission_check(
        &self,
        resource_type: &str,
        action: &str,
        resource: &str,
    ) -> HostResult<bool> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!(
            "permission_check: resource_type={}, action={}, resource={}",
            resource_type, action, resource
        );

        let check_params =
            nevoflux_storage::CheckPermissionParams::new(resource_type, action, resource);

        let result = services
            .database
            .permissions()
            .check(check_params)
            .map_err(|e| HostError {
                code: 100,
                message: format!("Permission check failed: {}", e),
            })?;

        // Return the permission result, or true if no explicit permission exists
        // (default allow for permissions not explicitly defined)
        let granted = result.unwrap_or(true);
        debug!(
            "permission_check: result={:?}, returning={}",
            result, granted
        );
        Ok(granted)
    }

    fn tool_search(&self, query: &str, max_results: usize) -> HostResult<Vec<ToolSearchResult>> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        let tool_search = services.tool_search.as_ref().ok_or_else(|| HostError {
            code: 2,
            message: "Tool search not configured".into(),
        })?;

        // Use blocking_read for synchronous context
        let index = tool_search.blocking_read();
        let results = index.search_limit(query, max_results);

        debug!(
            "tool_search: query='{}', found {} results",
            query,
            results.len()
        );

        Ok(results
            .into_iter()
            .map(|r| ToolSearchResult {
                name: r.tool.name,
                description: r.tool.description,
                score: r.score,
                input_schema: r.tool.input_schema,
                source: None,
            })
            .collect())
    }

    fn tool_call_dynamic(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> HostResult<String> {
        let start = std::time::Instant::now();
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        let mcp_manager = services.mcp_manager.as_ref().ok_or_else(|| HostError {
            code: 2,
            message: "MCP manager not configured".into(),
        })?;

        debug!(
            "tool_call_dynamic: tool='{}', arguments={}",
            tool_name, arguments
        );

        // Capture params for trace before arguments is consumed
        let trace_params = if self.trace_collector.is_some() {
            Some(arguments.clone())
        } else {
            None
        };
        let params_summary = serde_json::to_string(arguments).ok();

        // Execute MCP call using block_in_place + block_on pattern
        let runtime = self.runtime.clone();
        let manager = mcp_manager.clone();
        let tool = tool_name.to_string();
        let args = arguments.clone();

        let result = tokio::task::block_in_place(|| {
            runtime.block_on(async move { manager.call_tool_any(&tool, args).await })
        });

        let host_result = match result {
            Ok(tool_result) => {
                // Extract text content from the result
                let content: String = tool_result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text { text } => Some(text.clone()),
                        ToolResultContent::Image { .. } => Some("[Image content]".to_string()),
                        ToolResultContent::Resource { text, uri, .. } => text
                            .clone()
                            .or_else(|| Some(format!("[Resource: {}]", uri))),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                if tool_result.is_error {
                    Err(HostError {
                        code: 100,
                        message: content,
                    })
                } else {
                    Ok(content)
                }
            }
            Err(e) => {
                error!("tool_call_dynamic failed: {}", e);
                Err(HostError {
                    code: 101,
                    message: format!("Tool call failed: {}", e),
                })
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let traced_name = format!("dynamic:{}", tool_name);
        match &host_result {
            Ok(_) => self.record_tool(
                &traced_name,
                params_summary,
                true,
                None,
                None,
                duration_ms,
                trace_params,
                None,
            ),
            Err(e) => self.record_tool(
                &traced_name,
                params_summary,
                false,
                Some(e.code.to_string()),
                Some(e.message.clone()),
                duration_ms,
                trace_params,
                None,
            ),
        }
        host_result
    }

    fn computer_screenshot(&self, monitor: Option<i64>) -> HostResult<String> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;
        let controller = services
            .computer_controller
            .as_ref()
            .ok_or_else(|| HostError {
                code: 2,
                message: "Computer controller not configured".into(),
            })?;

        let controller = controller.clone();
        let result = tokio::task::block_in_place(|| {
            self.runtime.block_on(async {
                if let Some(display_id) = monitor {
                    controller.capture_display(display_id as u32).await
                } else {
                    controller.capture_screen().await
                }
            })
        })
        .map_err(|e| HostError {
            code: 3,
            message: format!("Screenshot failed: {}", e),
        })?;

        serde_json::to_string(&result).map_err(|e| HostError {
            code: 4,
            message: format!("Serialization failed: {}", e),
        })
    }

    fn computer_mouse_move(&self, x: i64, y: i64, click: Option<&str>) -> HostResult<String> {
        use nevoflux_computer::{ClickType, MouseButton, Point};

        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;
        let controller = services
            .computer_controller
            .as_ref()
            .ok_or_else(|| HostError {
                code: 2,
                message: "Computer controller not configured".into(),
            })?;

        let controller = controller.clone();
        let point = Point::new(x as i32, y as i32);

        tokio::task::block_in_place(|| {
            self.runtime.block_on(async {
                controller.move_to(point).await.map_err(|e| HostError {
                    code: 3,
                    message: format!("Mouse move failed: {}", e),
                })?;

                if let Some(click_action) = click {
                    let (button, click_type) = match click_action {
                        "right" => (MouseButton::Right, ClickType::Single),
                        "middle" => (MouseButton::Middle, ClickType::Single),
                        "double" => (MouseButton::Left, ClickType::Double),
                        _ => (MouseButton::Left, ClickType::Single),
                    };
                    controller
                        .click(button, click_type)
                        .await
                        .map_err(|e| HostError {
                            code: 3,
                            message: format!("Click failed: {}", e),
                        })?;
                }

                Ok::<_, HostError>(())
            })
        })?;

        Ok(format!(r#"{{"moved_to":{{"x":{},"y":{}}}}}"#, x, y))
    }

    fn computer_click(
        &self,
        x: i64,
        y: i64,
        button: Option<&str>,
        click_type: Option<&str>,
    ) -> HostResult<String> {
        use nevoflux_computer::{ClickType, MouseButton, Point};

        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;
        let controller = services
            .computer_controller
            .as_ref()
            .ok_or_else(|| HostError {
                code: 2,
                message: "Computer controller not configured".into(),
            })?;

        let btn = match button {
            Some("right") => MouseButton::Right,
            Some("middle") => MouseButton::Middle,
            _ => MouseButton::Left,
        };
        let ct = match click_type {
            Some("double") => ClickType::Double,
            Some("triple") => ClickType::Triple,
            _ => ClickType::Single,
        };
        let point = Point::new(x as i32, y as i32);
        let controller = controller.clone();

        tokio::task::block_in_place(|| {
            self.runtime.block_on(async {
                controller
                    .click_at(point, btn, ct)
                    .await
                    .map_err(|e| HostError {
                        code: 3,
                        message: format!("Click failed: {}", e),
                    })
            })
        })?;

        Ok(format!(
            r#"{{"clicked":{{"x":{},"y":{},"button":"{}","click_type":"{}"}}}}"#,
            x,
            y,
            button.unwrap_or("left"),
            click_type.unwrap_or("single")
        ))
    }

    fn computer_type_text(&self, text: &str, _delay_ms: Option<u64>) -> HostResult<String> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;
        let controller = services
            .computer_controller
            .as_ref()
            .ok_or_else(|| HostError {
                code: 2,
                message: "Computer controller not configured".into(),
            })?;

        let controller = controller.clone();
        let text_owned = text.to_string();

        tokio::task::block_in_place(|| {
            self.runtime.block_on(async {
                controller
                    .type_text(&text_owned)
                    .await
                    .map_err(|e| HostError {
                        code: 3,
                        message: format!("Type text failed: {}", e),
                    })
            })
        })?;

        Ok(format!(r#"{{"typed_chars":{}}}"#, text.len()))
    }

    fn computer_key(
        &self,
        key: &str,
        modifiers: &[String],
        repeat: Option<u64>,
    ) -> HostResult<String> {
        use nevoflux_computer::KeyCombination;

        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;
        let controller = services
            .computer_controller
            .as_ref()
            .ok_or_else(|| HostError {
                code: 2,
                message: "Computer controller not configured".into(),
            })?;

        // Parse key
        let key_or_char = parse_key_str(key).map_err(|msg| HostError {
            code: 4,
            message: msg,
        })?;

        let mut combination = KeyCombination {
            key: key_or_char,
            modifiers: Vec::new(),
        };

        for m in modifiers {
            match m.to_lowercase().as_str() {
                "shift" => combination = combination.with_shift(),
                "ctrl" | "control" => combination = combination.with_ctrl(),
                "alt" => combination = combination.with_alt(),
                "meta" | "cmd" | "command" | "win" | "windows" | "super" => {
                    combination = combination.with_meta()
                }
                _ => {}
            }
        }

        let repeat_count = repeat.unwrap_or(1).max(1).min(100);
        let controller = controller.clone();

        tokio::task::block_in_place(|| {
            self.runtime.block_on(async {
                for _ in 0..repeat_count {
                    controller
                        .press_key(combination.clone())
                        .await
                        .map_err(|e| HostError {
                            code: 3,
                            message: format!("Key press failed: {}", e),
                        })?;
                }
                Ok::<_, HostError>(())
            })
        })?;

        Ok(format!(
            r#"{{"pressed":"{}","repeat":{}}}"#,
            key, repeat_count
        ))
    }

    fn computer_scroll(
        &self,
        x: i64,
        y: i64,
        direction: &str,
        amount: Option<u64>,
    ) -> HostResult<String> {
        use nevoflux_computer::{Point, ScrollDirection};

        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;
        let controller = services
            .computer_controller
            .as_ref()
            .ok_or_else(|| HostError {
                code: 2,
                message: "Computer controller not configured".into(),
            })?;

        let dir = match direction {
            "up" => ScrollDirection::Up,
            "left" => ScrollDirection::Left,
            "right" => ScrollDirection::Right,
            _ => ScrollDirection::Down,
        };
        let scroll_amount = amount.unwrap_or(3) as u32;
        let point = Point::new(x as i32, y as i32);
        let controller = controller.clone();

        tokio::task::block_in_place(|| {
            self.runtime.block_on(async {
                controller.move_to(point).await.map_err(|e| HostError {
                    code: 3,
                    message: format!("Mouse move failed: {}", e),
                })?;
                controller
                    .scroll(dir, scroll_amount)
                    .await
                    .map_err(|e| HostError {
                        code: 3,
                        message: format!("Scroll failed: {}", e),
                    })
            })
        })?;

        Ok(format!(
            r#"{{"scrolled":"{}","amount":{},"at":{{"x":{},"y":{}}}}}"#,
            direction, scroll_amount, x, y
        ))
    }

    fn builtin_chat(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        // Create a new agent and run chat mode
        let agent = nevoflux_builtin_wasm::Agent::new(self.clone_for_builtin());
        agent.run(&AgentInput {
            mode: nevoflux_builtin_wasm::AgentMode::Chat,
            ..input.clone()
        })
    }

    fn builtin_browser(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        let agent = nevoflux_builtin_wasm::Agent::new(self.clone_for_builtin());
        agent.run(&AgentInput {
            mode: nevoflux_builtin_wasm::AgentMode::Browser,
            ..input.clone()
        })
    }

    fn builtin_agent(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        let agent = nevoflux_builtin_wasm::Agent::new(self.clone_for_builtin());
        agent.run(&AgentInput {
            mode: nevoflux_builtin_wasm::AgentMode::Agent,
            ..input.clone()
        })
    }

    fn browser_navigate(&self, url: &str, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_navigate: url={}", url);
        self.execute_browser_action(
            BrowserToolAction::Navigate,
            serde_json::json!({"url": url}),
            tab_id,
        )
    }

    fn browser_go_back(&self, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_go_back");
        self.execute_browser_action(BrowserToolAction::GoBack, serde_json::json!({}), tab_id)
    }

    fn browser_go_forward(&self, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_go_forward");
        self.execute_browser_action(BrowserToolAction::GoForward, serde_json::json!({}), tab_id)
    }

    fn browser_click(&self, selector: &str, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_click: selector={}", selector);
        self.execute_browser_action(
            BrowserToolAction::Click,
            serde_json::json!({"selector": selector}),
            tab_id,
        )
    }

    fn browser_click_by_id(
        &self,
        element_id: &str,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_click_by_id: element_id={}", element_id);
        self.execute_browser_action(
            BrowserToolAction::ClickById,
            serde_json::json!({"element_id": element_id}),
            tab_id,
        )
    }

    fn browser_type(
        &self,
        selector: &str,
        text: &str,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_type: selector={}, text={}", selector, text);
        self.execute_browser_action(
            BrowserToolAction::Type,
            serde_json::json!({"selector": selector, "text": text}),
            tab_id,
        )
    }

    fn browser_type_by_id(
        &self,
        element_id: &str,
        text: &str,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!(
            "browser_type_by_id: element_id={}, text={}",
            element_id, text
        );
        self.execute_browser_action(
            BrowserToolAction::TypeById,
            serde_json::json!({"element_id": element_id, "text": text}),
            tab_id,
        )
    }

    fn browser_fill(
        &self,
        selector: &str,
        value: &str,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_fill: selector={}, value={}", selector, value);
        self.execute_browser_action(
            BrowserToolAction::Fill,
            serde_json::json!({"selector": selector, "value": value}),
            tab_id,
        )
    }

    fn browser_fill_by_id(
        &self,
        element_id: &str,
        value: &str,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!(
            "browser_fill_by_id: element_id={}, value={}",
            element_id, value
        );
        self.execute_browser_action(
            BrowserToolAction::FillById,
            serde_json::json!({"element_id": element_id, "value": value}),
            tab_id,
        )
    }

    fn browser_get_content(&self, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_get_content");
        self.execute_browser_action(BrowserToolAction::GetContent, serde_json::json!({}), tab_id)
    }

    fn browser_get_markdown(&self, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_get_markdown");
        self.execute_browser_action(
            BrowserToolAction::GetMarkdown,
            serde_json::json!({}),
            tab_id,
        )
    }

    fn browser_screenshot(
        &self,
        full_page: bool,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_screenshot: full_page={}", full_page);
        self.execute_browser_action(
            BrowserToolAction::Screenshot,
            serde_json::json!({"full_page": full_page}),
            tab_id,
        )
    }

    fn browser_eval_js(&self, script: &str, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_eval_js: script={}", script);
        self.execute_browser_action(
            BrowserToolAction::EvalJs,
            serde_json::json!({"script": script}),
            tab_id,
        )
    }

    fn browser_scroll(
        &self,
        direction: &str,
        amount: &str,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_scroll: direction={}, amount={}", direction, amount);
        self.execute_browser_action(
            BrowserToolAction::Scroll,
            serde_json::json!({"direction": direction, "amount": amount}),
            tab_id,
        )
    }

    fn browser_wait_for(
        &self,
        selector: &str,
        timeout_ms: u64,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!(
            "browser_wait_for: selector={}, timeout_ms={}",
            selector, timeout_ms
        );
        self.execute_browser_action(
            BrowserToolAction::WaitFor,
            serde_json::json!({"selector": selector, "timeout_ms": timeout_ms}),
            tab_id,
        )
    }

    fn browser_get_elements(&self, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_get_elements: getting accessibility tree");
        self.execute_browser_action(BrowserToolAction::Snapshot, serde_json::json!({}), tab_id)
    }

    fn browser_list_tabs(&self, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_list_tabs: listing all open tabs");
        self.execute_browser_action(BrowserToolAction::ListTabs, serde_json::json!({}), tab_id)
    }

    fn browser_query_tabs(
        &self,
        params: &serde_json::Value,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_query_tabs: querying tabs with filter");
        self.execute_browser_action(BrowserToolAction::QueryTabs, params.clone(), tab_id)
    }

    fn browser_read_artifact(
        &self,
        params: &serde_json::Value,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_read_artifact: reading artifact source");
        self.execute_browser_action(BrowserToolAction::ReadArtifact, params.clone(), tab_id)
    }

    fn browser_edit_artifact(
        &self,
        params: &serde_json::Value,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_edit_artifact: editing artifact");
        self.execute_browser_action(BrowserToolAction::EditArtifact, params.clone(), tab_id)
    }

    fn browser_wait_for_stable(
        &self,
        strategy: &str,
        max_wait: u64,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!(
            "browser_wait_for_stable: strategy={}, max_wait={}",
            strategy, max_wait
        );
        self.execute_browser_action(
            BrowserToolAction::WaitForStable,
            serde_json::json!({"strategy": strategy, "maxWait": max_wait}),
            tab_id,
        )
    }

    fn browser_viewport_snapshot(&self, tab_id: Option<i64>) -> HostResult<BrowserToolResult> {
        debug!("browser_viewport_snapshot: taking viewport-only snapshot");
        self.execute_browser_action(
            BrowserToolAction::Snapshot,
            serde_json::json!({"viewport_only": true}),
            tab_id,
        )
    }

    fn browser_key_press(
        &self,
        key: &str,
        modifiers: &[String],
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        debug!("browser_key_press: key={}, modifiers={:?}", key, modifiers);
        self.execute_browser_action(
            BrowserToolAction::KeyPress,
            serde_json::json!({"key": key, "modifiers": modifiers}),
            tab_id,
        )
    }

    fn is_interrupted(&self) -> HostResult<bool> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        Ok(services.is_interrupted())
    }

    fn subagent_spawn(&self, task: &str, mode: &str, tab_id: Option<i64>) -> HostResult<u64> {
        debug!(
            "subagent_spawn: task='{}', mode={}, tab_id={:?}",
            task, mode, tab_id
        );

        // Parse the mode
        let agent_mode = match mode {
            "chat" => AgentMode::Chat,
            "browser" => AgentMode::Browser,
            _ => AgentMode::Agent,
        };

        // Try to use WASM sandboxed executor if available (preferred)
        if let Some(services) = &self.services {
            if let Some(executor) = &services.subagent_executor {
                debug!("Using WASM sandboxed executor for subagent");

                let custom_prompt = Some(
                    Agent::<DaemonHostFunctions>::subagent_prompt_for_mode(agent_mode).to_string(),
                );

                let handle = executor
                    .spawn(task.to_string(), agent_mode, custom_prompt, tab_id)
                    .map_err(|e| HostError {
                        code: 500,
                        message: format!("Failed to spawn subagent: {}", e),
                    })?;

                return Ok(handle.id);
            }
        }

        // Fall back to legacy implementation (no sandboxing)
        debug!("Using legacy Tokio-based subagent execution (no sandbox)");
        Self::spawn_legacy_subagent_impl(
            &self.subagent_registry,
            &self.config,
            &self.runtime,
            &self.services,
            task,
            mode,
            agent_mode,
            tab_id,
        )
    }

    fn subagent_status(&self, id: u64) -> HostResult<String> {
        debug!("subagent_status: id={}", id);

        // Try WASM executor first
        if let Some(services) = &self.services {
            if let Some(executor) = &services.subagent_executor {
                if let Some(status) = executor.status(id) {
                    return Ok(status.as_str().to_string());
                }
            }
        }

        // Fall back to legacy registry
        let entries = self
            .subagent_registry
            .entries
            .read()
            .map_err(|_| HostError {
                code: 500,
                message: "Failed to lock subagent registry".into(),
            })?;

        entries
            .get(&id)
            .map(|e| e.status.as_str().to_string())
            .ok_or_else(|| HostError {
                code: 404,
                message: format!("Subagent not found: {}", id),
            })
    }

    fn subagent_wait(&self, id: u64) -> HostResult<String> {
        debug!("subagent_wait: id={}", id);

        // Try WASM executor first
        if let Some(services) = &self.services {
            if let Some(executor) = &services.subagent_executor {
                if executor.get(id).is_some() {
                    let runtime = self.runtime.clone();
                    return tokio::task::block_in_place(|| {
                        runtime.block_on(async { executor.wait(id).await })
                    })
                    .map_err(|e| HostError {
                        code: 500,
                        message: e,
                    });
                }
            }
        }

        // Fall back to legacy registry
        Self::wait_legacy_subagent_impl(&self.subagent_registry, &self.runtime, id)
    }

    fn subagent_wait_all(&self, ids: &[u64]) -> HostResult<String> {
        debug!("subagent_wait_all: ids={:?}", ids);

        let results: Vec<serde_json::Value> = ids
            .iter()
            .map(|&id| match self.subagent_wait(id) {
                Ok(result) => serde_json::json!({
                    "id": id,
                    "status": "completed",
                    "result": result,
                }),
                Err(e) => serde_json::json!({
                    "id": id,
                    "status": "failed",
                    "error": e.message,
                }),
            })
            .collect();

        Ok(serde_json::to_string_pretty(&results).unwrap_or_default())
    }

    fn subagent_kill(&self, id: u64) -> HostResult<bool> {
        debug!("subagent_kill: id={}", id);

        // Try WASM executor first
        if let Some(services) = &self.services {
            if let Some(executor) = &services.subagent_executor {
                if executor.get(id).is_some() {
                    return executor.kill(id).map_err(|e| HostError {
                        code: 500,
                        message: e,
                    });
                }
            }
        }

        // Fall back to legacy registry
        let mut entries = self
            .subagent_registry
            .entries
            .write()
            .map_err(|_| HostError {
                code: 500,
                message: "Failed to lock subagent registry".into(),
            })?;

        if let Some(entry) = entries.get_mut(&id) {
            if entry.status == SubagentStatus::Running {
                entry.status = SubagentStatus::Killed;
                entry.result = Some("Killed by user".to_string());
                entry.completion_rx = None;
                Ok(true)
            } else {
                Ok(false) // Already completed
            }
        } else {
            Err(HostError {
                code: 404,
                message: format!("Subagent not found: {}", id),
            })
        }
    }

    fn subagent_list(&self) -> HostResult<Vec<SubagentInfo>> {
        debug!("subagent_list");

        let mut all_subagents = Vec::new();

        // Get subagents from WASM executor if available
        if let Some(services) = &self.services {
            if let Some(executor) = &services.subagent_executor {
                for handle in executor.list() {
                    all_subagents.push(SubagentInfo {
                        id: handle.id,
                        task: handle.task().to_string(),
                        mode: handle.mode().to_string(),
                        status: handle.status().as_str().to_string(),
                    });
                }
            }
        }

        // Also get subagents from legacy registry
        let entries = self
            .subagent_registry
            .entries
            .read()
            .map_err(|_| HostError {
                code: 500,
                message: "Failed to lock subagent registry".into(),
            })?;

        for (id, entry) in entries.iter() {
            // Skip if already listed from executor (shouldn't happen, but be safe)
            if !all_subagents.iter().any(|s| s.id == *id) {
                all_subagents.push(SubagentInfo {
                    id: *id,
                    task: entry.task.clone(),
                    mode: entry.mode.clone(),
                    status: entry.status.as_str().to_string(),
                });
            }
        }

        Ok(all_subagents)
    }

    fn stream_emit(&self, text: &str) -> HostResult<()> {
        if let Some(tx) = &self.sidebar_stream_tx {
            let chunk = SidebarStreamChunk {
                text: text.to_string(),
                done: false,
                event: None,
                thinking_event: None,
            };
            tx.send(chunk).map_err(|_| HostError {
                code: 500,
                message: "Failed to send stream chunk: channel closed".into(),
            })?;
            debug!("stream_emit: sent {} bytes", text.len());
        } else {
            // If no stream sender is configured, just log and continue
            debug!("stream_emit: no sidebar stream configured, ignoring chunk");
        }
        Ok(())
    }

    fn stream_end(&self) -> HostResult<()> {
        if let Some(tx) = &self.sidebar_stream_tx {
            let chunk = SidebarStreamChunk {
                text: String::new(),
                done: true,
                event: None,
                thinking_event: None,
            };
            tx.send(chunk).map_err(|_| HostError {
                code: 500,
                message: "Failed to send stream end: channel closed".into(),
            })?;
            debug!("stream_end: sent end signal");
        } else {
            debug!("stream_end: no sidebar stream configured, ignoring");
        }
        Ok(())
    }

    fn set_iteration(&self, iteration: u32) -> HostResult<()> {
        self.current_iteration.store(iteration, Ordering::Relaxed);
        Ok(())
    }

    fn set_model_override(&self, provider: &str, model: &str) -> HostResult<()> {
        // Validate provider name
        let _provider_type = ProviderType::from_str(provider).map_err(|_| HostError {
            code: 10,
            message: format!("Invalid provider: {}", provider),
        })?;

        // Validate API key exists for this provider
        self.get_api_key_for_provider(provider)?;

        info!("Model override set: provider={}, model={}", provider, model);
        *self.model_override_provider.lock().unwrap() = Some(provider.to_string());
        *self.model_override_model.lock().unwrap() = Some(model.to_string());
        Ok(())
    }
}

impl DaemonHostFunctions {
    /// Send a tool event to the sidebar without any text content.
    pub fn stream_tool_event(&self, event: nevoflux_protocol::ToolEvent) -> HostResult<()> {
        if let Some(tx) = &self.sidebar_stream_tx {
            let chunk = SidebarStreamChunk {
                text: String::new(),
                done: false,
                event: Some(event),
                thinking_event: None,
            };
            tx.send(chunk).map_err(|_| HostError {
                code: 500,
                message: "stream closed".to_string(),
            })?;
        }
        Ok(())
    }

    /// Send a thinking event to the sidebar without any text content.
    pub fn stream_thinking_event(&self, event: nevoflux_protocol::ThinkingEvent) -> HostResult<()> {
        if let Some(tx) = &self.sidebar_stream_tx {
            let chunk = SidebarStreamChunk {
                text: String::new(),
                done: false,
                event: None,
                thinking_event: Some(event),
            };
            tx.send(chunk).map_err(|_| HostError {
                code: 500,
                message: "stream closed".to_string(),
            })?;
        }
        Ok(())
    }

    /// Create a clone for builtin proxy calls to avoid infinite recursion.
    fn clone_for_builtin(&self) -> Self {
        Self {
            config: self.config.clone(),
            runtime: self.runtime.clone(),
            services: self.services.clone(),
            subagent_registry: self.subagent_registry.clone(),
            stream_registry: self.stream_registry.clone(),
            sidebar_stream_tx: self.sidebar_stream_tx.clone(),
            session_id: self.session_id.clone(),
            trace_collector: self.trace_collector.clone(),
            current_iteration: AtomicU32::new(self.current_iteration.load(Ordering::Relaxed)),
            stream_trace_data: self.stream_trace_data.clone(),
            model_override_provider: self.model_override_provider.clone(),
            model_override_model: self.model_override_model.clone(),
            skill_base_path: self.skill_base_path.clone(),
            subagent_sandbox: self.subagent_sandbox.clone(),
            current_thinking_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Ensure skills are loaded from filesystem (lazy loading).
    fn ensure_skills_loaded(&self, services: &HostServices) {
        let mut registry = services.skills.blocking_write();
        if registry.is_empty() {
            info!("Loading skills from filesystem");
            match registry.load() {
                Ok(count) => {
                    if count > 0 {
                        let names = registry.names();
                        info!("Loaded {} skills from filesystem: {:?}", count, names);
                    } else {
                        info!("No skills found in configured directories");
                    }
                }
                Err(e) => {
                    warn!("Failed to load skills from filesystem: {}", e);
                }
            }
        }
    }

    /// Process web content with LLM using a small model.
    ///
    /// This takes the fetched web page content and uses the prompt to extract
    /// or summarize the relevant information.
    fn process_web_content_with_llm(
        &self,
        content: &str,
        prompt: &str,
        url: &str,
        title: &str,
    ) -> HostResult<String> {
        use nevoflux_builtin_wasm::{LlmRequest, Message};

        // Build system message for content extraction
        let system_message = format!(
            "You are a content extraction assistant. \
             Extract and summarize information from the provided web page content \
             based on the user's request.\n\n\
             Page URL: {}\n\
             Page Title: {}",
            url, title
        );

        // Truncate content if too long (keep first 50000 chars to leave room for response)
        let truncated_content = if content.len() > 50000 {
            format!("{}...\n\n[Content truncated]", &content[..50000])
        } else {
            content.to_string()
        };

        let user_message = format!(
            "Web page content:\n\n{}\n\n---\n\nUser request: {}",
            truncated_content, prompt
        );

        let request = LlmRequest {
            messages: vec![
                Message::system(&system_message),
                Message::user(&user_message),
            ],
            tools: vec![],
            stream: false,
        };

        // Call LLM (non-streaming)
        let response = self.llm_chat(&request)?;

        Ok(response.text)
    }

    /// Execute a browser action via the browser sender channel.
    /// Extract base64 screenshot data from a browser response result.
    ///
    /// Handles multiple response formats from the sidebar:
    /// - Plain base64 string: `"iVBORw0KGgo..."`
    /// - Data URL string: `"data:image/png;base64,iVBORw0KGgo..."`
    /// - Object with "screenshot" key: `{"screenshot": "..."}`
    /// - Object with "data" key: `{"data": "..."}`
    fn extract_screenshot_base64(result: &Option<serde_json::Value>) -> Option<String> {
        let value = result.as_ref()?;

        // Case 1: plain string (base64 or data URL)
        if let Some(s) = value.as_str() {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }

        // Case 2: JSON object with known keys
        if let Some(obj) = value.as_object() {
            for key in &["screenshot", "data", "data_url", "image", "base64"] {
                if let Some(serde_json::Value::String(s)) = obj.get(*key) {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }

        None
    }

    fn execute_browser_action(
        &self,
        action: BrowserToolAction,
        params: serde_json::Value,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        use crate::wasm::services::{BrowserRequest, BrowserResponse};

        let start = std::time::Instant::now();

        // Derive tool name from action (e.g. "browser_navigate", "browser_click")
        let tool_name = serde_json::to_value(action)
            .ok()
            .and_then(|v| v.as_str().map(|s| format!("browser_{}", s)))
            .unwrap_or_else(|| format!("browser_{:?}", action).to_lowercase());

        // Capture params summary for trace (guard clone with trace check)
        let params_summary = if self.trace_collector.is_some() {
            serde_json::to_string(&params).ok()
        } else {
            None
        };

        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        let browser_sender = services.browser_sender.as_ref().ok_or_else(|| HostError {
            code: 2,
            message: "Browser sender not configured".into(),
        })?;

        // Generate unique request ID
        let request_id = uuid::Uuid::new_v4().to_string();

        // Use configured session_id or fallback to "default"
        let session_id = self
            .session_id
            .clone()
            .unwrap_or_else(|| "default".to_string());

        let request = BrowserRequest {
            request_id: request_id.clone(),
            session_id,
            tab_id,
            action,
            params,
            timeout_ms: 30000, // 30 second default timeout
            client_identity: self
                .services
                .as_ref()
                .map(|s| s.client_identity.clone())
                .unwrap_or_default(),
            proxy_id: self
                .services
                .as_ref()
                .map(|s| s.proxy_id.clone())
                .unwrap_or_default(),
        };

        // Create oneshot channel for response
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        // Send request via browser_sender
        let sender = browser_sender.clone();
        let runtime = self.runtime.clone();

        let result: Result<BrowserResponse, String> = tokio::task::block_in_place(|| {
            runtime.block_on(async {
                // Send the request
                sender
                    .send((request, response_tx))
                    .await
                    .map_err(|_| "Failed to send browser request".to_string())?;

                // Wait for response with timeout
                tokio::time::timeout(std::time::Duration::from_millis(30000), response_rx)
                    .await
                    .map_err(|_| "Browser request timed out".to_string())?
                    .map_err(|_| "Response channel closed".to_string())
            })
        });

        let host_result = match result {
            Ok(response) => {
                if response.success {
                    // For Screenshot actions, put the base64 data in the screenshot field
                    // so that downstream extract_screenshot_from_tool_result can find it
                    if action == BrowserToolAction::Screenshot {
                        let screenshot_base64 = Self::extract_screenshot_base64(&response.result);
                        if screenshot_base64.is_none() {
                            warn!(
                                "browser_screenshot returned success but no screenshot data. result={:?}",
                                response.result.as_ref().map(|v| {
                                    let s = v.to_string();
                                    if s.len() > 200 {
                                        format!("{}...({}B)", &s[..200], s.len())
                                    } else {
                                        s
                                    }
                                })
                            );
                        }
                        Ok(BrowserToolResult {
                            success: true,
                            data: None,
                            error: None,
                            screenshot: screenshot_base64,
                        })
                    } else {
                        Ok(BrowserToolResult {
                            success: true,
                            data: response.result,
                            error: None,
                            screenshot: None,
                        })
                    }
                } else {
                    let error_msg = response
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "Unknown browser error".into());
                    Ok(BrowserToolResult::error(error_msg))
                }
            }
            Err(e) => {
                warn!("Browser action failed: {}", e);
                Ok(BrowserToolResult::error(e))
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let success = host_result.as_ref().map(|r| r.success).unwrap_or(false);
        let error_msg = host_result.as_ref().ok().and_then(|r| r.error.clone());
        self.record_tool(
            &tool_name,
            params_summary,
            success,
            if success {
                None
            } else {
                Some("BROWSER_ERROR".to_string())
            },
            error_msg,
            duration_ms,
            None,
            None,
        );

        host_result
    }

    /// Legacy subagent spawn implementation using Tokio tasks.
    ///
    /// This is used when no SubagentExecutor is available.
    /// WARNING: This provides no sandboxing - subagents have full access to HostServices.
    #[allow(clippy::too_many_arguments)]
    fn spawn_legacy_subagent_impl(
        subagent_registry: &Arc<SubagentRegistry>,
        config: &Arc<AgentConfig>,
        runtime: &Handle,
        services: &Option<HostServices>,
        task: &str,
        mode: &str,
        agent_mode: AgentMode,
        tab_id: Option<i64>,
    ) -> HostResult<u64> {
        let id = subagent_registry.allocate_id();
        let task_str = task.to_string();
        let mode_str = mode.to_string();

        // Create a oneshot channel for completion notification
        let (completion_tx, completion_rx) = oneshot::channel();

        // Register the entry
        {
            let mut entries = subagent_registry.entries.write().map_err(|_| HostError {
                code: 500,
                message: "Failed to lock subagent registry".into(),
            })?;

            entries.insert(
                id,
                SubagentEntry {
                    task: task_str.clone(),
                    mode: mode_str.clone(),
                    status: SubagentStatus::Running,
                    result: None,
                    completion_rx: Some(completion_rx),
                },
            );
        }

        // Spawn the task asynchronously
        let config = config.clone();
        let runtime_clone = runtime.clone();
        let services = services.clone();
        let registry = subagent_registry.clone();

        runtime.spawn(async move {
            // Create a new host functions instance for the subagent
            let mut host = DaemonHostFunctions::new(config, runtime_clone.clone());
            if let Some(svc) = services {
                host = host.with_services(svc);
            }

            // Create agent input with custom prompt for sub-agent
            let input = AgentInput {
                session_id: format!("subagent-{}", id),
                mode: agent_mode,
                user_message: task_str.clone(),
                history: vec![],
                attachments: vec![],
                local_files: vec![],
                custom_system_prompt: Some(
                    Agent::<DaemonHostFunctions>::subagent_prompt_for_mode(agent_mode).to_string(),
                ),
                tab_id,
                tab_ids: vec![],
                skill_context: None,
                available_models: vec![],
                mcp_servers: vec![],
                soul_context: None,
            };

            // Run the appropriate builtin mode
            let result = match agent_mode {
                AgentMode::Chat => host.builtin_chat(&input),
                AgentMode::Browser => host.builtin_browser(&input),
                AgentMode::Agent | AgentMode::Code => host.builtin_agent(&input),
            };

            // Update the registry with the result
            let (status, result_text) = match result {
                Ok(output) => (SubagentStatus::Completed, output.text),
                Err(e) => (SubagentStatus::Failed, format!("Error: {}", e)),
            };

            if let Ok(mut entries) = registry.entries.write() {
                if let Some(entry) = entries.get_mut(&id) {
                    entry.status = status;
                    entry.result = Some(result_text.clone());
                    entry.completion_rx = None; // Clear the receiver
                }
            }

            // Notify waiters
            let _ = completion_tx.send(result_text);
        });

        Ok(id)
    }

    /// Wait for a legacy subagent to complete.
    fn wait_legacy_subagent_impl(
        subagent_registry: &Arc<SubagentRegistry>,
        runtime: &Handle,
        id: u64,
    ) -> HostResult<String> {
        // First check if already completed
        {
            let entries = subagent_registry.entries.read().map_err(|_| HostError {
                code: 500,
                message: "Failed to lock subagent registry".into(),
            })?;

            if let Some(entry) = entries.get(&id) {
                if entry.status != SubagentStatus::Running {
                    return entry.result.clone().ok_or_else(|| HostError {
                        code: 500,
                        message: "No result available".into(),
                    });
                }
            } else {
                return Err(HostError {
                    code: 404,
                    message: format!("Subagent not found: {}", id),
                });
            }
        }

        // Take the completion receiver
        let completion_rx = {
            let mut entries = subagent_registry.entries.write().map_err(|_| HostError {
                code: 500,
                message: "Failed to lock subagent registry".into(),
            })?;

            entries.get_mut(&id).and_then(|e| e.completion_rx.take())
        };

        // Wait for completion
        if let Some(rx) = completion_rx {
            let runtime = runtime.clone();
            tokio::task::block_in_place(|| {
                runtime.block_on(async { rx.await.map_err(|_| "Subagent task was dropped") })
            })
            .map_err(|e| HostError {
                code: 500,
                message: e.to_string(),
            })
        } else {
            // Check result again (race condition handling)
            let entries = subagent_registry.entries.read().map_err(|_| HostError {
                code: 500,
                message: "Failed to lock subagent registry".into(),
            })?;

            entries
                .get(&id)
                .and_then(|e| e.result.clone())
                .ok_or_else(|| HostError {
                    code: 500,
                    message: "No result available".into(),
                })
        }
    }
}

/// Parse a key string into a KeyOrChar for computer_key host function.
fn parse_key_str(key_str: &str) -> Result<nevoflux_computer::KeyOrChar, String> {
    use nevoflux_computer::{Key, KeyOrChar};

    let key = match key_str.to_lowercase().as_str() {
        "shift" => Key::Shift,
        "ctrl" | "control" => Key::Control,
        "alt" => Key::Alt,
        "meta" | "cmd" | "command" | "win" | "windows" | "super" => Key::Meta,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "capslock" | "caps_lock" => Key::CapsLock,
        "space" => Key::Space,
        "enter" | "return" => Key::Enter,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        "up" | "arrowup" | "arrow_up" => Key::ArrowUp,
        "down" | "arrowdown" | "arrow_down" => Key::ArrowDown,
        "left" | "arrowleft" | "arrow_left" => Key::ArrowLeft,
        "right" | "arrowright" | "arrow_right" => Key::ArrowRight,
        "printscreen" | "print_screen" => Key::PrintScreen,
        "scrolllock" | "scroll_lock" => Key::ScrollLock,
        "pause" => Key::Pause,
        "numlock" | "num_lock" => Key::NumLock,
        s if s.len() == 1 => {
            return Ok(KeyOrChar::Char(s.chars().next().unwrap()));
        }
        _ => {
            return Err(format!("Unknown key: {}", key_str));
        }
    };

    Ok(KeyOrChar::Key(key))
}

/// Merge FTS keyword results with vector semantic results into a single ranked list.
///
/// Uses weighted scoring: FTS results get position-based scores (1.0 for first, decreasing),
/// semantic results use cosine similarity scores. The final score is a weighted combination.
///
/// For chunks that appear only in semantic results (not in FTS), the function looks them
/// up from the database to build the full `MemoryChunk`.
fn merge_search_results(
    fts_chunks: &[nevoflux_storage::MemoryChunk],
    semantic_results: &[VectorSearchResult],
    limit: usize,
    database: &nevoflux_storage::Database,
) -> Vec<MemoryChunk> {
    let fts_weight = 0.4;
    let semantic_weight = 0.6;

    // Normalize FTS scores: position-based (first result = 1.0, last ~ 0.1)
    let fts_count = fts_chunks.len().max(1) as f64;
    let fts_scores: HashMap<&str, f64> = fts_chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let score = 1.0 - (i as f64 / fts_count) * 0.9;
            (c.id.as_str(), score)
        })
        .collect();

    // Semantic scores are already 0..1 from cosine similarity
    let sem_scores: HashMap<&str, f64> = semantic_results
        .iter()
        .map(|r| (r.id.as_str(), r.score as f64))
        .collect();

    // Combine scores from both sources
    let mut combined: HashMap<&str, f64> = HashMap::new();
    for id in fts_scores.keys().chain(sem_scores.keys()) {
        if combined.contains_key(id) {
            continue;
        }
        let fts = fts_scores.get(id).copied().unwrap_or(0.0);
        let sem = sem_scores.get(id).copied().unwrap_or(0.0);
        combined.insert(id, fts_weight * fts + semantic_weight * sem);
    }

    // Sort by combined score descending
    let mut sorted: Vec<_> = combined.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    sorted.truncate(limit);

    // Build lookup from FTS results for fast access
    let fts_by_id: HashMap<&str, &nevoflux_storage::MemoryChunk> =
        fts_chunks.iter().map(|c| (c.id.as_str(), c)).collect();

    // Assemble final results
    sorted
        .into_iter()
        .filter_map(|(id, score)| {
            // Try FTS results first (already loaded)
            if let Some(chunk) = fts_by_id.get(id) {
                return Some(MemoryChunk {
                    id: chunk.id.clone(),
                    content: chunk.content.clone(),
                    session_id: chunk.session_id.clone(),
                    score: score as f32,
                });
            }
            // Semantic-only hit: look up from database
            match database.memory().get(id) {
                Ok(Some(chunk)) => Some(MemoryChunk {
                    id: chunk.id,
                    content: chunk.content,
                    session_id: chunk.session_id,
                    score: score as f32,
                }),
                Ok(None) => {
                    warn!(
                        "Memory chunk {} found in vector index but not in database",
                        id
                    );
                    None
                }
                Err(e) => {
                    warn!("Failed to look up memory chunk {}: {}", id, e);
                    None
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_skills::{LoaderConfig, Skill, SkillMetadata, SkillRegistry};
    use nevoflux_storage::Database;

    fn setup_host_with_services() -> (DaemonHostFunctions, tokio::runtime::Runtime) {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);
        (host, rt)
    }

    /// Setup host with an empty skills registry (no default directories).
    fn setup_host_with_empty_skills() -> (DaemonHostFunctions, tokio::runtime::Runtime) {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Use LoaderConfig with empty user_dirs to avoid loading from real filesystem
        let loader_config = LoaderConfig::new().with_user_dirs(vec![]);
        let skills = Arc::new(tokio::sync::RwLock::new(SkillRegistry::with_config(
            loader_config,
        )));
        let services = HostServices::with_skills(db, skills);

        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);
        (host, rt)
    }

    #[test]
    fn test_daemon_host_functions_creation() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _host = DaemonHostFunctions::new(config, rt.handle().clone());
    }

    // ==================== Memory Tests ====================

    #[test]
    fn test_memory_search_without_services() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.memory_search("test", 10);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 1);
    }

    #[test]
    fn test_memory_search_with_services() {
        let (host, _rt) = setup_host_with_services();

        // Search should return empty results (no data yet)
        let result = host.memory_search("test", 10);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_memory_create_and_search() {
        let (host, _rt) = setup_host_with_services();

        // Create a memory chunk
        let metadata = serde_json::json!({"source": "test"});
        let id = host
            .memory_create("hello world searchable content", &metadata)
            .unwrap();
        assert!(!id.is_empty());

        // Search for it
        let results = host.memory_search("searchable", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("searchable"));
    }

    #[test]
    fn test_memory_update() {
        let (host, _rt) = setup_host_with_services();

        // Create a memory chunk
        let id = host
            .memory_create("original content", &serde_json::json!({}))
            .unwrap();

        // Update it
        let result = host.memory_update(&id, "updated content");
        assert!(result.is_ok());

        // Search to verify update (FTS should find the new content)
        let results = host.memory_search("updated", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("updated"));
    }

    #[test]
    fn test_memory_update_not_found() {
        let (host, _rt) = setup_host_with_services();

        let result = host.memory_update("nonexistent-id", "content");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 404);
    }

    #[test]
    fn test_memory_delete() {
        let (host, _rt) = setup_host_with_services();

        // Create and then delete
        let id = host
            .memory_create("to be deleted", &serde_json::json!({}))
            .unwrap();
        let result = host.memory_delete(&id);
        assert!(result.is_ok());

        // Search should not find it anymore
        let results = host.memory_search("deleted", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_memory_delete_not_found() {
        let (host, _rt) = setup_host_with_services();

        let result = host.memory_delete("nonexistent-id");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 404);
    }

    // ==================== Skill Tests ====================

    #[test]
    fn test_skill_list_without_services() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.skill_list();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 1);
    }

    #[test]
    fn test_skill_list_empty() {
        // Use setup with empty skills registry to avoid loading from real filesystem
        let (host, _rt) = setup_host_with_empty_skills();

        let result = host.skill_list();
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_skill_list_with_skills() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Use empty skills registry to avoid loading from real filesystem
        let loader_config = LoaderConfig::new().with_user_dirs(vec![]);
        let skills = Arc::new(tokio::sync::RwLock::new(SkillRegistry::with_config(
            loader_config,
        )));
        let services = HostServices::with_skills(db, skills);

        // Register a test skill
        {
            let mut registry = services.skills.blocking_write();
            let skill = Skill::new(
                SkillMetadata::new("test-skill").with_description("A test skill"),
                "# Test Skill\n\nThis is test content.",
            );
            registry.register(skill).unwrap();
        }

        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let summaries = host.skill_list().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "test-skill");
        assert_eq!(summaries[0].description, "A test skill");
    }

    #[test]
    fn test_skill_load_not_found() {
        let (host, _rt) = setup_host_with_services();

        let result = host.skill_load("nonexistent");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 404);
    }

    #[test]
    fn test_skill_load_success() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        let services = HostServices::new(db);
        {
            let mut registry = services.skills.blocking_write();
            let skill = Skill::new(
                SkillMetadata::new("my-skill"),
                "# My Skill\n\nSkill content here.",
            );
            registry.register(skill).unwrap();
        }

        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let content = host.skill_load("my-skill").unwrap();
        assert!(content.contains("# My Skill"));
        assert!(content.contains("Skill content here"));
    }

    #[test]
    fn test_skill_read_not_found() {
        let (host, _rt) = setup_host_with_services();

        let result = host.skill_read("nonexistent", "file.txt");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 404);
    }

    #[test]
    fn test_skill_execute_not_found() {
        let (host, _rt) = setup_host_with_services();

        let result = host.skill_execute("nonexistent", "script.sh", &serde_json::json!({}));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 404);
    }

    // ==================== Permission Tests ====================

    #[test]
    fn test_permission_check_without_services() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.permission_check("file", "read", "/test");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 1);
    }

    #[test]
    fn test_permission_check_default_allow() {
        let (host, _rt) = setup_host_with_services();

        // No explicit permission - should return true (default allow)
        let result = host.permission_check("file", "read", "/home/user/file.txt");
        assert!(result.is_ok());
        assert!(result.unwrap()); // Default allow
    }

    #[test]
    fn test_permission_request_creates_permission() {
        let (host, _rt) = setup_host_with_services();

        // Request permission (should auto-grant)
        let granted = host.permission_request("tool", "execute", "bash").unwrap();
        assert!(granted);

        // Check the permission exists
        let result = host.permission_check("tool", "execute", "bash").unwrap();
        assert!(result);
    }

    #[test]
    fn test_permission_request_returns_existing() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Create a denied permission first
        db.permissions()
            .create(
                nevoflux_storage::CreatePermissionParams::new("file", "write", "/sensitive")
                    .with_scope(nevoflux_storage::PermissionScope::Global)
                    .with_granted(false),
            )
            .unwrap();

        let services = HostServices::new(db);
        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        // Request should return the existing denied permission
        let granted = host
            .permission_request("file", "write", "/sensitive")
            .unwrap();
        assert!(!granted);
    }

    #[test]
    fn test_daemon_host_functions_tool_search_without_services() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        // Should fail without services
        let result = host.tool_search("file", 5);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, 1);
        assert!(err.message.contains("Services not available"));
    }

    #[test]
    fn test_daemon_host_functions_tool_call_dynamic_without_services() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        // Should fail without services
        let result = host.tool_call_dynamic("read_file", &serde_json::json!({"path": "/test"}));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, 1);
        assert!(err.message.contains("Services not available"));
    }

    #[test]
    fn test_daemon_host_functions_with_services_tool_search() {
        use nevoflux_mcp::ToolSearchIndex;
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Create services with tool search
        let mut index = ToolSearchIndex::new();
        index.add(&nevoflux_mcp::ToolDefinition {
            name: "read_file".into(),
            description: "Read a file from disk".into(),
            input_schema: serde_json::json!({"type": "object"}),
        });

        let services = HostServices::new(db).with_tool_search(index);

        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        // Should find the tool
        let results = host.tool_search("file", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn test_daemon_host_functions_with_services_no_tool_search() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Create services without tool search
        let services = HostServices::new(db);

        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        // Should fail because tool_search is not configured
        let result = host.tool_search("file", 5);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("Tool search not configured"));
    }

    #[test]
    fn test_daemon_host_functions_with_services_no_mcp_manager() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Create services without MCP manager
        let services = HostServices::new(db);

        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        // Should fail because mcp_manager is not configured
        let result = host.tool_call_dynamic("read_file", &serde_json::json!({}));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("MCP manager not configured"));
    }

    #[test]
    fn test_daemon_host_functions_browser_navigate_without_services() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        // Should fail without services
        let result = host.browser_navigate("https://example.com", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, 1);
        assert!(err.message.contains("Services not available"));
    }

    #[test]
    fn test_daemon_host_functions_browser_navigate_without_browser_sender() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Create services without browser_sender
        let services = HostServices::new(db);

        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        // Should fail because browser_sender is not configured
        let result = host.browser_navigate("https://example.com", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("Browser sender not configured"));
    }

    #[test]
    fn test_daemon_host_functions_browser_click_without_browser_sender() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let result = host.browser_click("#button", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Browser sender"));
    }

    #[test]
    fn test_daemon_host_functions_browser_type_without_browser_sender() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let result = host.browser_type("#input", "hello", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_daemon_host_functions_browser_fill_without_browser_sender() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let result = host.browser_fill("#input", "value", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_daemon_host_functions_browser_screenshot_without_browser_sender() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let result = host.browser_screenshot(false, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_daemon_host_functions_browser_scroll_without_browser_sender() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let result = host.browser_scroll("down", "page", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_daemon_host_functions_browser_wait_for_without_browser_sender() {
        use nevoflux_storage::Database;

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let result = host.browser_wait_for("#element", 5000, None);
        assert!(result.is_err());
    }

    // ==================== Interrupt Tests ====================

    #[test]
    fn test_daemon_host_functions_is_interrupted_without_services() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.is_interrupted();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 1);
    }

    #[test]
    fn test_daemon_host_functions_is_interrupted_default() {
        let (host, _rt) = setup_host_with_services();

        // Default should be not interrupted
        let result = host.is_interrupted();
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_daemon_host_functions_is_interrupted_after_set() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);

        // Set interrupted before creating host
        services.set_interrupted(true);

        let host = DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services);

        let result = host.is_interrupted();
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_daemon_host_functions_interrupt_flag_shared_with_services() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);

        let host =
            DaemonHostFunctions::new(config, rt.handle().clone()).with_services(services.clone());

        // Initially not interrupted
        assert!(!host.is_interrupted().unwrap());

        // Set interrupted via services
        services.set_interrupted(true);

        // Host should see the change
        assert!(host.is_interrupted().unwrap());

        // Reset via services
        services.reset_interrupt();

        // Host should see the reset
        assert!(!host.is_interrupted().unwrap());
    }

    // ==================== Subagent Tests ====================

    #[test]
    fn test_daemon_host_functions_subagent_spawn() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        // Spawn a subagent (will fail to run without services but ID should be assigned)
        let id = host.subagent_spawn("Test task", "agent", None).unwrap();
        assert!(id > 0);

        // Spawn another - should get a different ID
        let id2 = host.subagent_spawn("Task 2", "chat", None).unwrap();
        assert!(id2 > id);
    }

    #[test]
    fn test_daemon_host_functions_subagent_status_not_found() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.subagent_status(999);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 404);
    }

    #[test]
    fn test_daemon_host_functions_subagent_status_after_spawn() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let id = host.subagent_spawn("Test task", "agent", None).unwrap();

        // Status should be "running" initially
        let status = host.subagent_status(id).unwrap();
        assert_eq!(status, "running");
    }

    #[test]
    fn test_daemon_host_functions_subagent_list_empty() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let list = host.subagent_list().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_daemon_host_functions_subagent_list_after_spawn() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        host.subagent_spawn("Task 1", "agent", None).unwrap();
        host.subagent_spawn("Task 2", "browser", None).unwrap();

        let list = host.subagent_list().unwrap();
        assert_eq!(list.len(), 2);

        // Verify the list contents
        assert!(list.iter().any(|s| s.task == "Task 1" && s.mode == "agent"));
        assert!(list
            .iter()
            .any(|s| s.task == "Task 2" && s.mode == "browser"));
    }

    #[test]
    fn test_daemon_host_functions_subagent_kill() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let id = host.subagent_spawn("Long task", "agent", None).unwrap();

        // Kill the running subagent
        let killed = host.subagent_kill(id).unwrap();
        assert!(killed);

        // Status should now be "killed"
        let status = host.subagent_status(id).unwrap();
        assert_eq!(status, "killed");

        // Killing again should return false
        let killed_again = host.subagent_kill(id).unwrap();
        assert!(!killed_again);
    }

    #[test]
    fn test_daemon_host_functions_subagent_kill_not_found() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.subagent_kill(999);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 404);
    }

    #[test]
    fn test_daemon_host_functions_subagent_wait_not_found() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.subagent_wait(999);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 404);
    }

    // ==================== Screenshot Extraction Tests ====================

    #[test]
    fn test_extract_screenshot_base64_none() {
        assert_eq!(DaemonHostFunctions::extract_screenshot_base64(&None), None);
    }

    #[test]
    fn test_extract_screenshot_base64_plain_string() {
        let result = Some(serde_json::Value::String("iVBORw0KGgo".into()));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            Some("iVBORw0KGgo".into())
        );
    }

    #[test]
    fn test_extract_screenshot_base64_data_url() {
        let data_url = "data:image/png;base64,iVBORw0KGgo";
        let result = Some(serde_json::Value::String(data_url.into()));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            Some(data_url.into())
        );
    }

    #[test]
    fn test_extract_screenshot_base64_empty_string() {
        let result = Some(serde_json::Value::String("".into()));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            None
        );
    }

    #[test]
    fn test_extract_screenshot_base64_object_screenshot_key() {
        let result = Some(serde_json::json!({"screenshot": "iVBORw0KGgo"}));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            Some("iVBORw0KGgo".into())
        );
    }

    #[test]
    fn test_extract_screenshot_base64_object_data_key() {
        let result = Some(serde_json::json!({"data": "iVBORw0KGgo"}));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            Some("iVBORw0KGgo".into())
        );
    }

    #[test]
    fn test_extract_screenshot_base64_object_image_key() {
        let result = Some(serde_json::json!({"image": "iVBORw0KGgo"}));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            Some("iVBORw0KGgo".into())
        );
    }

    #[test]
    fn test_extract_screenshot_base64_object_base64_key() {
        let result = Some(serde_json::json!({"base64": "iVBORw0KGgo"}));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            Some("iVBORw0KGgo".into())
        );
    }

    #[test]
    fn test_extract_screenshot_base64_object_unknown_keys() {
        let result = Some(serde_json::json!({"success": true, "url": "http://example.com"}));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            None
        );
    }

    #[test]
    fn test_extract_screenshot_base64_object_empty_value() {
        let result = Some(serde_json::json!({"screenshot": ""}));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            None
        );
    }

    #[test]
    fn test_extract_screenshot_base64_number() {
        let result = Some(serde_json::json!(42));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            None
        );
    }

    #[test]
    fn test_extract_screenshot_base64_bool() {
        let result = Some(serde_json::json!(true));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            None
        );
    }

    #[test]
    fn test_extract_screenshot_base64_object_priority() {
        // "screenshot" key should be found first due to iteration order
        let result = Some(serde_json::json!({
            "screenshot": "from_screenshot",
            "data": "from_data"
        }));
        assert_eq!(
            DaemonHostFunctions::extract_screenshot_base64(&result),
            Some("from_screenshot".into())
        );
    }

    #[test]
    fn test_model_override_fields_default_none() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        assert!(host.model_override_provider.lock().unwrap().is_none());
        assert!(host.model_override_model.lock().unwrap().is_none());
    }

    #[test]
    fn test_model_override_fields_set_and_read() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        *host.model_override_provider.lock().unwrap() = Some("openai".to_string());
        *host.model_override_model.lock().unwrap() = Some("gpt-4o".to_string());

        assert_eq!(
            host.model_override_provider.lock().unwrap().as_deref(),
            Some("openai")
        );
        assert_eq!(
            host.model_override_model.lock().unwrap().as_deref(),
            Some("gpt-4o")
        );
    }

    #[test]
    fn test_set_model_override_invalid_provider() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.set_model_override("nonexistent_provider", "some-model");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, 10);
        assert!(err.message.contains("Invalid provider"));
    }

    #[test]
    fn test_set_model_override_valid_provider_with_key() {
        let mut config = AgentConfig::default();
        config.llm.openai.api_key = Some("test-key-123".to_string());
        let config = Arc::new(config);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        let result = host.set_model_override("openai", "gpt-4o");
        assert!(result.is_ok());

        assert_eq!(
            host.model_override_provider.lock().unwrap().as_deref(),
            Some("openai")
        );
        assert_eq!(
            host.model_override_model.lock().unwrap().as_deref(),
            Some("gpt-4o")
        );
    }

    #[test]
    fn test_set_model_override_no_api_key() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        // openai provider exists but has no API key in default config
        let result = host.set_model_override("openai", "gpt-4o");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("No API key"));
    }

    #[test]
    fn test_resolve_provider_uses_override_when_set() {
        let mut config = AgentConfig::default();
        config.llm.provider = Some("anthropic".to_string());
        config.llm.anthropic.api_key = Some("anthropic-key".to_string());
        config.llm.anthropic.model = Some("claude-sonnet-4-20250514".to_string());
        config.llm.openai.api_key = Some("openai-key".to_string());
        let config = Arc::new(config);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        // Without override, should use anthropic from config
        let (provider, api_key, _model) = host.resolve_provider_and_model().unwrap();
        assert_eq!(provider, "anthropic");
        assert_eq!(api_key, "anthropic-key");

        // Set override to openai
        *host.model_override_provider.lock().unwrap() = Some("openai".to_string());
        *host.model_override_model.lock().unwrap() = Some("gpt-4o".to_string());

        let (provider, api_key, model) = host.resolve_provider_and_model().unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(api_key, "openai-key");
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn test_model_override_preserved_in_clone_for_builtin() {
        let mut config = AgentConfig::default();
        config.llm.openai.api_key = Some("test-key".to_string());
        let config = Arc::new(config);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());

        // Set override
        host.set_model_override("openai", "gpt-4o").unwrap();

        // Clone for builtin should share the same Arc<Mutex<...>>
        let cloned = host.clone_for_builtin();
        assert_eq!(
            cloned.model_override_provider.lock().unwrap().as_deref(),
            Some("openai")
        );
        assert_eq!(
            cloned.model_override_model.lock().unwrap().as_deref(),
            Some("gpt-4o")
        );
    }

    // ==================== tool_read Tests ====================

    #[test]
    fn test_tool_read_default_limit() {
        let (host, _rt) = setup_host_with_services();

        // Create a temp file with 300 lines
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test_300_lines.txt");
        let content: String = (1..=300)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, &content).unwrap();

        let result = host.tool_read(file_path.to_str().unwrap(), None, None);
        let read = result.unwrap();
        assert_eq!(read.total_lines, 300);
        assert_eq!(read.returned_lines, 200);
        assert_eq!(read.offset, 0);
        assert!(read.truncated);
        // First line should be "line 1"
        assert!(read.content.starts_with("line 1\n"));
        // Last returned line should be "line 200"
        assert!(read.content.ends_with("line 200"));
    }

    #[test]
    fn test_tool_read_with_offset_and_limit() {
        let (host, _rt) = setup_host_with_services();

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test_offset.txt");
        let content: String = (1..=300)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, &content).unwrap();

        let result = host.tool_read(file_path.to_str().unwrap(), Some(5), Some(3));
        let read = result.unwrap();
        assert_eq!(read.total_lines, 300);
        assert_eq!(read.returned_lines, 3);
        assert_eq!(read.offset, 5);
        assert!(read.truncated);
        // Lines at offsets 5, 6, 7 (0-indexed) = "line 6", "line 7", "line 8"
        assert_eq!(read.content, "line 6\nline 7\nline 8");
    }

    #[test]
    fn test_tool_read_no_truncation() {
        let (host, _rt) = setup_host_with_services();

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test_short.txt");
        let content: String = (1..=10)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, &content).unwrap();

        let result = host.tool_read(file_path.to_str().unwrap(), None, None);
        let read = result.unwrap();
        assert_eq!(read.total_lines, 10);
        assert_eq!(read.returned_lines, 10);
        assert_eq!(read.offset, 0);
        assert!(!read.truncated);
    }

    #[test]
    fn test_tool_read_long_line_truncation() {
        let (host, _rt) = setup_host_with_services();

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test_long_line.txt");
        // Create a file with one very long line (3000 chars)
        let long_line = "x".repeat(3000);
        std::fs::write(&file_path, &long_line).unwrap();

        let result = host.tool_read(file_path.to_str().unwrap(), None, None);
        let read = result.unwrap();
        assert_eq!(read.total_lines, 1);
        assert_eq!(read.returned_lines, 1);
        assert!(!read.truncated);
        // Line should be truncated at 2000 chars + "…[truncated]"
        assert!(read.content.contains("\u{2026}[truncated]"));
        assert!(read.content.len() < 3000);
    }

    // ==================== tool_grep Tests ====================

    #[test]
    fn test_tool_grep_with_grep_crate() {
        use std::io::Write;
        let (host, _rt) = setup_host_with_services();

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.rs");
        let mut file = std::fs::File::create(&file_path).unwrap();
        writeln!(file, "fn main() {{").unwrap();
        writeln!(file, "    println!(\"hello\");").unwrap();
        writeln!(file, "}}").unwrap();

        let result = host
            .tool_grep(
                "fn main",
                Some(dir.path().to_str().unwrap()),
                None,
                None,
                None,
            )
            .unwrap();
        assert!(result.total_matches >= 1);
        assert!(result.total_files >= 1);
        assert!(!result.results.is_empty());
        assert!(result.results[0].content.contains("fn main"));
    }

    #[test]
    fn test_tool_grep_case_insensitive() {
        use std::io::Write;
        let (host, _rt) = setup_host_with_services();

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        writeln!(file, "Hello World").unwrap();
        writeln!(file, "hello world").unwrap();
        writeln!(file, "HELLO WORLD").unwrap();

        let result = host
            .tool_grep(
                "hello",
                Some(dir.path().to_str().unwrap()),
                None,
                Some(true),
                None,
            )
            .unwrap();
        assert_eq!(result.total_matches, 3);
    }

    #[test]
    fn test_tool_grep_max_results() {
        use std::io::Write;
        let (host, _rt) = setup_host_with_services();

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        for i in 1..=20 {
            writeln!(file, "match line {}", i).unwrap();
        }

        let result = host
            .tool_grep(
                "match",
                Some(dir.path().to_str().unwrap()),
                None,
                None,
                Some(5),
            )
            .unwrap();
        assert_eq!(result.total_matches, 20);
        assert_eq!(result.returned, 5);
        assert_eq!(result.results.len(), 5);
        assert!(result.truncated);
    }

    #[test]
    fn test_tool_grep_invalid_regex() {
        let (host, _rt) = setup_host_with_services();
        let result = host.tool_grep("[invalid", Some("."), None, None, None);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, 1);
    }

    // ==================== tool_bash Tests ====================

    #[test]
    fn test_tool_bash_success_with_result() {
        let (host, _rt) = setup_host_with_services();
        let result = host.tool_bash("echo hello", None).unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(matches!(result.status, BashStatus::Success));
        assert!(result.stdout.contains("hello"));
        assert!(result.stderr.is_none());
        assert!(!result.truncated);
    }

    #[test]
    fn test_tool_bash_error_status() {
        let (host, _rt) = setup_host_with_services();
        let result = host.tool_bash("exit 42", None).unwrap();
        assert_eq!(result.exit_code, Some(42));
        assert!(matches!(result.status, BashStatus::Error));
    }

    #[test]
    fn test_tool_bash_output_truncation() {
        let (host, _rt) = setup_host_with_services();
        // Generate 300 lines
        let result = host.tool_bash("seq 1 300", None).unwrap();
        assert_eq!(result.total_lines, 300);
        assert!(result.returned_lines <= 200);
        assert!(result.truncated);
    }

    #[test]
    fn test_tool_bash_timeout_with_hint() {
        let (host, _rt) = setup_host_with_services();
        // Use a very short timeout
        let result = host.tool_bash("sleep 60", Some(500)).unwrap();
        assert!(matches!(result.status, BashStatus::Timeout));
        assert!(result.exit_code.is_none());
        assert!(result.hint.is_some());
        assert!(result.hint.unwrap().contains("timed out"));
    }

    // ==================== Sandbox Validation Tests ====================

    #[test]
    fn test_validate_sandbox_path_allows_sandbox_writes() {
        let sandbox_dir = tempfile::tempdir().unwrap();
        let sandbox_path = sandbox_dir.path().to_string_lossy().to_string();

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone())
            .with_subagent_sandbox(sandbox_path.clone());

        let file_in_sandbox = format!("{}/output.txt", sandbox_path);
        assert!(host.validate_sandbox_path(&file_in_sandbox).is_ok());
    }

    #[test]
    fn test_validate_sandbox_path_blocks_escape() {
        let sandbox_dir = tempfile::tempdir().unwrap();
        let sandbox_path = sandbox_dir.path().to_string_lossy().to_string();

        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone())
            .with_subagent_sandbox(sandbox_path);

        assert!(host.validate_sandbox_path("/etc/passwd").is_err());
        assert!(host.validate_sandbox_path("/tmp/other/file.txt").is_err());
    }

    #[test]
    fn test_validate_sandbox_path_skipped_for_main_agent() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = DaemonHostFunctions::new(config, rt.handle().clone());
        // No sandbox set — should allow any path
        assert!(host.validate_sandbox_path("/any/path/file.txt").is_ok());
    }
}
