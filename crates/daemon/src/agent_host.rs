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
use crate::wasm::llm::{execute_llm_chat, LlmChatRequest, LlmMessage as DaemonLlmMessage};
use crate::wasm::HostServices;
use nevoflux_builtin_wasm::{
    AgentInput, AgentMode, AgentOutput, BrowserToolResult, HostError, HostFunctions, HostResult,
    LlmRequest, LlmResponse, MemoryChunk, SkillSummary, SubagentInfo, ToolSearchResult,
};
use nevoflux_llm::ProviderType;
use nevoflux_mcp::ToolResultContent;
use nevoflux_protocol::BrowserToolAction;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tracing::{debug, error, warn};

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
}

impl DaemonHostFunctions {
    /// Create a new DaemonHostFunctions with the given configuration.
    pub fn new(config: Arc<AgentConfig>, runtime: Handle) -> Self {
        Self {
            config,
            runtime,
            services: None,
            subagent_registry: Arc::new(SubagentRegistry::new()),
        }
    }

    /// Add services to enable tool search and dynamic tool calls.
    pub fn with_services(mut self, services: HostServices) -> Self {
        self.services = Some(services);
        self
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
}

impl HostFunctions for DaemonHostFunctions {
    fn llm_chat(&self, request: &LlmRequest) -> HostResult<LlmResponse> {
        // Get provider configuration
        let provider_name = self.config.llm.active_provider().ok_or_else(|| HostError {
            code: 1,
            message: "No LLM provider configured".into(),
        })?;

        let api_key = self
            .config
            .llm
            .active_api_key()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| HostError {
                code: 2,
                message: "No API key configured".into(),
            })?;

        let model = self.config.llm.active_model().unwrap_or("gpt-4o-mini");

        let provider = ProviderType::from_str(provider_name).map_err(|_| HostError {
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
            128_000, // Default context window (TODO: make configurable per model)
            self.config.llm.max_tokens,
            &self.config.daemon.context,
        );

        // Attempt compression if needed
        let compressor = ContextCompressor::new(self.config.clone(), self.runtime.clone());
        let final_messages = match compressor.compress_if_needed(
            &context_messages,
            estimated_tokens,
            token_budget.for_history,
        ) {
            CompressionResult::Compressed {
                summary,
                recent,
                saved,
            } => {
                debug!("Compressed context, saved {} tokens", saved);
                // Prepend summary to recent messages
                let mut result = vec![ContextMessage {
                    role: "system".into(),
                    content: format!("[Conversation summary]\n{}", summary),
                }];
                result.extend(recent);
                result
            }
            CompressionResult::NotNeeded => context_messages,
            CompressionResult::Skipped { reason } => {
                debug!("Context compression skipped: {}", reason);
                context_messages
            }
        };

        // Convert to daemon request with potentially compressed messages
        let daemon_request = self.convert_request_with_messages(request, &final_messages);

        // Execute LLM call synchronously using block_in_place
        // (allows blocking in async context by moving to blocking thread pool)
        let runtime = self.runtime.clone();
        let result = tokio::task::block_in_place(|| {
            runtime.block_on(async {
                execute_llm_chat(provider, api_key, model, daemon_request).await
            })
        });

        match result {
            Ok(response) => {
                // Convert tool calls
                let tool_calls = response
                    .tool_calls
                    .unwrap_or_default()
                    .into_iter()
                    .map(|tc| nevoflux_builtin_wasm::ToolCall {
                        id: tc.id,
                        name: tc.name,
                        arguments: tc.arguments,
                    })
                    .collect();

                Ok(LlmResponse {
                    text: response.content,
                    tool_calls,
                })
            }
            Err(e) => {
                error!("LLM chat failed: {}", e);
                Err(HostError {
                    code: 100,
                    message: format!("LLM error: {}", e),
                })
            }
        }
    }

    fn llm_stream_start(&self, _request: &LlmRequest) -> HostResult<u64> {
        // Streaming not yet implemented
        warn!("llm_stream_start not yet implemented");
        Err(HostError {
            code: 501,
            message: "Streaming not yet implemented".into(),
        })
    }

    fn llm_stream_next(
        &self,
        _stream_id: u64,
    ) -> HostResult<Option<nevoflux_builtin_wasm::LlmChunk>> {
        Err(HostError {
            code: 501,
            message: "Streaming not yet implemented".into(),
        })
    }

    fn llm_stream_close(&self, _stream_id: u64) -> HostResult<()> {
        Ok(())
    }

    fn memory_search(&self, query: &str, limit: usize) -> HostResult<Vec<MemoryChunk>> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!("memory_search: query='{}', limit={}", query, limit);

        // Use FTS search from the storage crate
        let results = services
            .database
            .memory()
            .search_fts(query, limit)
            .map_err(|e| HostError {
                code: 100,
                message: format!("Memory search failed: {}", e),
            })?;

        // Convert storage::MemoryChunk to builtin_wasm::MemoryChunk
        Ok(results
            .into_iter()
            .map(|chunk| MemoryChunk {
                id: chunk.id,
                content: chunk.content,
                session_id: chunk.session_id,
                score: 1.0, // FTS doesn't provide a score, use 1.0 as default
            })
            .collect())
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

        debug!("skill_list");

        // Load skills from filesystem if registry is empty (lazy loading)
        self.ensure_skills_loaded(services);

        // Now read the summaries
        let registry = services.skills.blocking_read();
        let summaries = registry.list();

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
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        debug!("skill_load: name={}", name);

        // Load skills from filesystem if registry is empty (lazy loading)
        self.ensure_skills_loaded(services);

        let registry = services.skills.blocking_read();
        let skill = registry.get(name).ok_or_else(|| HostError {
            code: 404,
            message: format!("Skill not found: {}", name),
        })?;

        // Return the skill content (Level 2 loading)
        Ok(skill.content.clone())
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
        let output = registry
            .execute_script(name, script, args)
            .map_err(|e| HostError {
                code: match e {
                    nevoflux_skills::SkillsError::NotFound(_) => 404,
                    nevoflux_skills::SkillsError::ExecutionError(_) => 500,
                    _ => 100,
                },
                message: format!("Skill execute failed: {}", e),
            })?;

        Ok(output)
    }

    fn tool_read(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> HostResult<String> {
        use std::fs;
        use std::io::{BufRead, BufReader};

        let file = fs::File::open(path).map_err(|e| HostError {
            code: 1,
            message: format!("Failed to open file: {}", e),
        })?;

        let reader = BufReader::new(file);
        let offset = offset.unwrap_or(0) as usize;
        let limit = limit.unwrap_or(2000) as usize;

        let lines: Vec<String> = reader
            .lines()
            .skip(offset)
            .take(limit)
            .filter_map(|l| l.ok())
            .collect();

        Ok(lines.join("\n"))
    }

    fn tool_write(&self, path: &str, content: &str) -> HostResult<()> {
        use std::fs;

        fs::write(path, content).map_err(|e| HostError {
            code: 1,
            message: format!("Failed to write file: {}", e),
        })
    }

    fn tool_edit(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> HostResult<()> {
        use std::fs;

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
    }

    fn tool_bash(&self, command: &str, _timeout_ms: Option<u64>) -> HostResult<String> {
        use std::process::Command;

        // TODO: Implement timeout for bash commands

        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .output()
            .map_err(|e| HostError {
                code: 1,
                message: format!("Failed to execute command: {}", e),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            Ok(format!("STDOUT:\n{}\nSTDERR:\n{}", stdout, stderr))
        }
    }

    fn tool_glob(&self, pattern: &str, path: Option<&str>) -> HostResult<Vec<String>> {
        let full_pattern = match path {
            Some(p) => format!("{}/{}", p, pattern),
            None => pattern.to_string(),
        };

        let entries: Vec<String> = glob::glob(&full_pattern)
            .map_err(|e| HostError {
                code: 1,
                message: format!("Invalid glob pattern: {}", e),
            })?
            .filter_map(|r| r.ok())
            .map(|p| p.display().to_string())
            .collect();

        Ok(entries)
    }

    fn tool_grep(
        &self,
        pattern: &str,
        path: Option<&str>,
        _file_type: Option<&str>,
    ) -> HostResult<Vec<String>> {
        use std::process::Command;

        let mut cmd = Command::new("grep");
        cmd.arg("-r").arg("-n").arg(pattern);

        if let Some(p) = path {
            cmd.arg(p);
        } else {
            cmd.arg(".");
        }

        let output = cmd.output().map_err(|e| HostError {
            code: 1,
            message: format!("Failed to run grep: {}", e),
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.lines().map(|s| s.to_string()).collect())
    }

    fn tool_web_search(&self, query: &str) -> HostResult<String> {
        // TODO: Integrate with web search API
        debug!("tool_web_search not yet implemented for query: {}", query);
        Ok(format!("Web search for '{}' not yet implemented", query))
    }

    fn tool_web_fetch(&self, url: &str, prompt: &str) -> HostResult<String> {
        // TODO: Integrate with web fetch
        debug!("tool_web_fetch not yet implemented for url: {}", url);
        Ok(format!(
            "Web fetch for '{}' with prompt '{}' not yet implemented",
            url, prompt
        ))
    }

    fn tool_ask_user(&self, question: &str, _options: &[String]) -> HostResult<String> {
        // TODO: Integrate with browser extension for user interaction
        debug!(
            "tool_ask_user not yet implemented for question: {}",
            question
        );
        Err(HostError {
            code: 501,
            message: "User interaction not yet implemented".into(),
        })
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

        // Execute MCP call using block_in_place + block_on pattern
        let runtime = self.runtime.clone();
        let manager = mcp_manager.clone();
        let tool = tool_name.to_string();
        let args = arguments.clone();

        let result = tokio::task::block_in_place(|| {
            runtime.block_on(async move { manager.call_tool_any(&tool, args).await })
        });

        match result {
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
        }
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
        amount: i32,
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

    fn is_interrupted(&self) -> HostResult<bool> {
        let services = self.services.as_ref().ok_or_else(|| HostError {
            code: 1,
            message: "Services not available".into(),
        })?;

        Ok(services.is_interrupted())
    }

    fn subagent_spawn(&self, task: &str, mode: &str) -> HostResult<u64> {
        debug!("subagent_spawn: task='{}', mode={}", task, mode);

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

                // Build custom prompt for sub-agent
                let custom_prompt = Some(format!(
                    "You are a sub-agent executing a specific task.\n\
                     Task: {}\n\n\
                     Focus on completing this task efficiently. \
                     Return your findings when complete.",
                    task
                ));

                let handle = executor
                    .spawn(task.to_string(), agent_mode, custom_prompt)
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
}

impl DaemonHostFunctions {
    /// Create a clone for builtin proxy calls to avoid infinite recursion.
    fn clone_for_builtin(&self) -> Self {
        Self {
            config: self.config.clone(),
            runtime: self.runtime.clone(),
            services: self.services.clone(),
            subagent_registry: self.subagent_registry.clone(),
        }
    }

    /// Ensure skills are loaded from filesystem (lazy loading).
    fn ensure_skills_loaded(&self, services: &HostServices) {
        let mut registry = services.skills.blocking_write();
        if registry.is_empty() {
            debug!("ensure_skills_loaded: loading skills from filesystem");
            if let Err(e) = registry.load() {
                warn!("Failed to load skills from filesystem: {}", e);
            }
        }
    }

    /// Execute a browser action via the browser sender channel.
    fn execute_browser_action(
        &self,
        action: BrowserToolAction,
        params: serde_json::Value,
        tab_id: Option<i64>,
    ) -> HostResult<BrowserToolResult> {
        use crate::wasm::services::{BrowserRequest, BrowserResponse};

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

        let request = BrowserRequest {
            request_id: request_id.clone(),
            session_id: "default".into(), // TODO: get from context
            tab_id,
            action,
            params,
            timeout_ms: 30000, // 30 second default timeout
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

        match result {
            Ok(response) => {
                if response.success {
                    Ok(BrowserToolResult {
                        success: true,
                        data: response.result,
                        error: None,
                        screenshot: None,
                    })
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
        }
    }

    /// Legacy subagent spawn implementation using Tokio tasks.
    ///
    /// This is used when no SubagentExecutor is available.
    /// WARNING: This provides no sandboxing - subagents have full access to HostServices.
    fn spawn_legacy_subagent_impl(
        subagent_registry: &Arc<SubagentRegistry>,
        config: &Arc<AgentConfig>,
        runtime: &Handle,
        services: &Option<HostServices>,
        task: &str,
        mode: &str,
        agent_mode: AgentMode,
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
                custom_system_prompt: Some(format!(
                    "You are a sub-agent executing a specific task.\n\
                     Task: {}\n\n\
                     Focus on completing this task efficiently. \
                     Return your findings when complete.",
                    task_str
                )),
            };

            // Run the appropriate builtin mode
            let result = match agent_mode {
                AgentMode::Chat => host.builtin_chat(&input),
                AgentMode::Browser => host.builtin_browser(&input),
                AgentMode::Agent => host.builtin_agent(&input),
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

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_skills::{Skill, SkillMetadata};
    use nevoflux_storage::Database;

    fn setup_host_with_services() -> (DaemonHostFunctions, tokio::runtime::Runtime) {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
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
        let (host, _rt) = setup_host_with_services();

        let result = host.skill_list();
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_skill_list_with_skills() {
        let config = Arc::new(AgentConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(Database::open_in_memory().unwrap());

        // Create services and register a skill
        let services = HostServices::new(db);
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

        let result = host.browser_scroll("down", 500, None);
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
        let id = host.subagent_spawn("Test task", "agent").unwrap();
        assert!(id > 0);

        // Spawn another - should get a different ID
        let id2 = host.subagent_spawn("Task 2", "chat").unwrap();
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

        let id = host.subagent_spawn("Test task", "agent").unwrap();

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

        host.subagent_spawn("Task 1", "agent").unwrap();
        host.subagent_spawn("Task 2", "browser").unwrap();

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

        let id = host.subagent_spawn("Long task", "agent").unwrap();

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
}
