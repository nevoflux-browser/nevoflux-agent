//! Agent runner for executing the Wasm agent.
//!
//! This module implements the complete agent execution loop, handling:
//! - Wasm instance creation and ABI version verification
//! - Iteration loop with timeout handling
//! - Tool call execution and result passing
//! - Response accumulation and completion detection
//! - Streaming responses in real-time

use crate::agent::abi::{
    AgentContent, AgentProcessInput, AgentProcessOutput, HistoryEntry, PendingToolCall, ToolResult,
    ABI_VERSION,
};
use crate::agent::streaming::StreamHandle;
use crate::agent::tools::ToolRegistry;
use crate::error::{DaemonError, Result};
use crate::trace::collector::TraceCollector;
use crate::trace::detection::{DetectionContext, PatternEngine};
use crate::wasm::{HostServices, WasmInstance, WasmRuntime};
use nevoflux_protocol::{ChatMessage, StreamFormat, StreamMetadata};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Agent execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Normal chat mode.
    #[default]
    Chat,
    /// Planning mode.
    Plan,
    /// Code execution mode.
    Code,
    /// Browser automation mode.
    Browser,
}

/// Configuration for the agent runner.
#[derive(Debug, Clone)]
pub struct AgentRunnerConfig {
    /// Maximum iterations per request.
    pub max_iterations: u32,
    /// Timeout per iteration in milliseconds.
    pub iteration_timeout_ms: u64,
}

impl Default for AgentRunnerConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            iteration_timeout_ms: 30_000,
        }
    }
}

/// Input to the agent runner.
#[derive(Debug, Clone)]
pub struct AgentInput {
    /// Session ID.
    pub session_id: String,
    /// Current mode.
    pub mode: AgentMode,
    /// User message.
    pub user_message: String,
    /// Conversation history.
    pub history: Vec<ChatMessage>,
}

/// Output from the agent runner.
#[derive(Debug, Clone)]
pub struct AgentOutput {
    /// Response text.
    pub text: String,
    /// Whether to continue the loop.
    pub continue_loop: bool,
    /// Tool calls made.
    pub tool_calls: Vec<ToolCall>,
    /// Number of iterations executed.
    pub iterations: u32,
}

/// A tool call made by the agent.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Tool call ID.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Tool arguments as JSON.
    pub arguments: serde_json::Value,
    /// Tool result.
    pub result: Option<String>,
}

/// The agent runner.
pub struct AgentRunner {
    runtime: WasmRuntime,
    config: AgentRunnerConfig,
    tools: ToolRegistry,
    #[allow(dead_code)]
    services: Option<HostServices>,
    trace_collector: Option<Arc<TraceCollector>>,
    pattern_engine: Option<Mutex<PatternEngine>>,
}

impl AgentRunner {
    /// Create a new agent runner from Wasm bytes.
    pub fn new(wasm_bytes: &[u8]) -> Result<Self> {
        let runtime = WasmRuntime::from_bytes(wasm_bytes)?;
        Ok(Self {
            runtime,
            config: AgentRunnerConfig::default(),
            tools: ToolRegistry::new(),
            services: None,
            trace_collector: None,
            pattern_engine: None,
        })
    }

    /// Create with custom config.
    pub fn with_config(wasm_bytes: &[u8], config: AgentRunnerConfig) -> Result<Self> {
        let runtime = WasmRuntime::from_bytes(wasm_bytes)?;
        Ok(Self {
            runtime,
            config,
            tools: ToolRegistry::new(),
            services: None,
            trace_collector: None,
            pattern_engine: None,
        })
    }

    /// Set services for host functions.
    pub fn with_services(mut self, services: HostServices) -> Self {
        self.services = Some(services);
        self
    }

    /// Enable trace collection and pattern detection.
    pub fn with_trace(mut self, collector: Arc<TraceCollector>) -> Self {
        self.trace_collector = Some(collector);
        self.pattern_engine = Some(Mutex::new(PatternEngine::default_engine()));
        self
    }

    /// Get a mutable reference to the tool registry.
    pub fn tools_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tools
    }

    /// Get a reference to the tool registry.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Run the agent with the given input.
    ///
    /// This implements the complete agent execution loop:
    /// 1. Create a WasmInstance and verify ABI version
    /// 2. Convert AgentInput to AgentProcessInput
    /// 3. Loop until complete or max_iterations reached
    /// 4. For each iteration:
    ///    - Build AgentProcessInput with current content (UserMessage or ToolResults)
    ///    - Call the Wasm agent (simulated for now)
    ///    - Accumulate response text
    ///    - If complete, return AgentOutput
    ///    - If tool calls pending, execute them and continue with ToolResults
    /// 5. Track iterations and return in AgentOutput
    pub async fn run(&self, input: AgentInput) -> Result<AgentOutput> {
        // Create instance
        let mut instance = WasmInstance::new(&self.runtime)?;

        // Check ABI version
        let abi_version = instance.get_abi_version()?;
        if abi_version as i32 != ABI_VERSION {
            return Err(DaemonError::InternalError(format!(
                "Unsupported ABI version: {}, expected: {}",
                abi_version, ABI_VERSION
            )));
        }

        // Convert history from ChatMessage to HistoryEntry
        let history: Vec<HistoryEntry> = input
            .history
            .iter()
            .map(|msg| HistoryEntry {
                role: "user".to_string(), // ChatMessage doesn't have role, default to user
                content: msg.text.clone(),
            })
            .collect();

        // Initialize loop state
        let mut iteration: u32 = 0;
        let mut accumulated_text = String::new();
        let mut all_tool_calls: Vec<ToolCall> = Vec::new();
        let mut current_content = AgentContent::UserMessage {
            text: input.user_message.clone(),
        };
        let mut trace_summary: Option<String> = None;

        // Main execution loop
        loop {
            // Check if we've exceeded max iterations
            if iteration >= self.config.max_iterations {
                return Ok(AgentOutput {
                    text: accumulated_text,
                    continue_loop: true, // Indicate we stopped due to max iterations
                    tool_calls: all_tool_calls,
                    iterations: iteration,
                });
            }

            // Build the process input for this iteration
            let process_input = AgentProcessInput {
                session_id: input.session_id.clone(),
                iteration,
                content: current_content.clone(),
                history: history.clone(),
                trace_summary: trace_summary.take(),
            };

            // Call the Wasm agent (simulated for now since we don't have a real agent_process export)
            // In a full implementation, this would:
            // 1. Serialize process_input to MessagePack
            // 2. Allocate memory in Wasm and copy the data
            // 3. Call agent_process with timeout
            // 4. Read and deserialize the response
            let output = self.call_agent(&mut instance, &process_input).await?;

            // Accumulate response text
            if !output.text.is_empty() {
                if !accumulated_text.is_empty() {
                    accumulated_text.push('\n');
                }
                accumulated_text.push_str(&output.text);
            }

            // Convert pending tool calls to ToolCall structs
            let tool_calls: Vec<ToolCall> = output
                .tool_calls
                .iter()
                .map(|tc| ToolCall {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                    result: None,
                })
                .collect();

            // Check if complete
            if output.complete {
                // Add any final tool calls (shouldn't happen if complete, but for safety)
                all_tool_calls.extend(tool_calls);

                return Ok(AgentOutput {
                    text: accumulated_text,
                    continue_loop: false,
                    tool_calls: all_tool_calls,
                    iterations: iteration + 1,
                });
            }

            // If there are pending tool calls, execute them
            if !output.tool_calls.is_empty() {
                let mut tool_results: Vec<ToolResult> = Vec::new();

                for pending in &output.tool_calls {
                    // Execute the tool (simulated for now)
                    let result = self.execute_tool(pending).await;

                    // Record tool execution in trace
                    if let Some(tc) = &self.trace_collector {
                        let params_summary =
                            extract_tool_params_summary(&pending.name, &pending.arguments);
                        let (success, err_code, err_msg) = match &result.error {
                            Some(err) => {
                                (false, Some("TOOL_ERROR".to_string()), Some(err.clone()))
                            }
                            None => (true, None, None),
                        };
                        tc.record_tool_exec(
                            &input.session_id,
                            iteration,
                            &pending.name,
                            params_summary,
                            success,
                            err_code,
                            err_msg,
                            0, // duration_ms - not tracked at this level currently
                            None,
                            None,
                        );
                    }

                    // Track the tool call with its result
                    all_tool_calls.push(ToolCall {
                        id: pending.id.clone(),
                        name: pending.name.clone(),
                        arguments: pending.arguments.clone(),
                        result: result.content.clone(),
                    });

                    tool_results.push(result);
                }

                // Pattern detection - check for anomalous patterns
                if let (Some(tc), Some(engine)) =
                    (&self.trace_collector, &self.pattern_engine)
                {
                    let recent = tc.recent_tool_spans(&input.session_id, 10);
                    let ctx = DetectionContext {
                        session_id: &input.session_id,
                        iteration,
                        max_iterations: self.config.max_iterations,
                        recent_tool_spans: &recent,
                    };
                    trace_summary = engine.lock().unwrap().check(&ctx);
                }

                // Set up next iteration with tool results
                current_content = AgentContent::ToolResults {
                    results: tool_results,
                };
            }

            iteration += 1;
        }
    }

    /// Call the Wasm agent with the given input.
    ///
    /// This is a simulated implementation that returns appropriate responses
    /// based on the input. A full implementation would serialize the input,
    /// call the actual Wasm entry point, and deserialize the response.
    async fn call_agent(
        &self,
        _instance: &mut WasmInstance,
        input: &AgentProcessInput,
    ) -> Result<AgentProcessOutput> {
        // Simulated implementation for testing
        // In production, this would call the actual Wasm agent_process function

        match &input.content {
            AgentContent::UserMessage { text } => {
                // First iteration: respond to user message
                // For testing, just echo the message
                Ok(AgentProcessOutput {
                    text: format!("Agent processed: {}", text),
                    tool_calls: vec![],
                    complete: true,
                })
            }
            AgentContent::ToolResults { results } => {
                // Subsequent iteration: process tool results
                let result_summary: Vec<String> = results
                    .iter()
                    .map(|r| {
                        if let Some(content) = &r.content {
                            format!("{}: {}", r.name, content)
                        } else if let Some(error) = &r.error {
                            format!("{}: error - {}", r.name, error)
                        } else {
                            format!("{}: no result", r.name)
                        }
                    })
                    .collect();

                Ok(AgentProcessOutput {
                    text: format!("Tool results: {}", result_summary.join(", ")),
                    tool_calls: vec![],
                    complete: true,
                })
            }
        }
    }

    /// Execute a tool call using the tool registry.
    ///
    /// This method dispatches the tool call to the appropriate executor
    /// in the tool registry and returns the result.
    async fn execute_tool(&self, tool_call: &PendingToolCall) -> ToolResult {
        self.tools.execute(tool_call).await
    }

    /// Get the configuration.
    pub fn config(&self) -> &AgentRunnerConfig {
        &self.config
    }

    /// Run the agent with streaming output.
    ///
    /// This method is similar to `run`, but streams response chunks back
    /// through the provided `StreamHandle` as they are generated.
    ///
    /// # Arguments
    ///
    /// * `input` - The agent input containing session, mode, and user message
    /// * `stream_handle` - Handle for sending streaming chunks back to the client
    ///
    /// # Returns
    ///
    /// Returns `AgentOutput` containing the complete response and execution metadata.
    /// Note that the response text in the output will contain the complete accumulated
    /// text, even though it was already streamed.
    pub async fn run_streaming(
        &self,
        input: AgentInput,
        stream_handle: StreamHandle,
    ) -> Result<AgentOutput> {
        // Create instance
        let mut instance = WasmInstance::new(&self.runtime)?;

        // Check ABI version
        let abi_version = instance.get_abi_version()?;
        if abi_version as i32 != ABI_VERSION {
            return Err(DaemonError::InternalError(format!(
                "Unsupported ABI version: {}, expected: {}",
                abi_version, ABI_VERSION
            )));
        }

        // Convert history from ChatMessage to HistoryEntry
        let history: Vec<HistoryEntry> = input
            .history
            .iter()
            .map(|msg| HistoryEntry {
                role: "user".to_string(),
                content: msg.text.clone(),
            })
            .collect();

        // Initialize loop state
        let mut iteration: u32 = 0;
        let mut accumulated_text = String::new();
        let mut all_tool_calls: Vec<ToolCall> = Vec::new();
        let mut current_content = AgentContent::UserMessage {
            text: input.user_message.clone(),
        };
        let mut trace_summary: Option<String> = None;

        // Track timing for metadata
        let start_time = std::time::Instant::now();

        // Main execution loop
        loop {
            // Check if we've exceeded max iterations
            if iteration >= self.config.max_iterations {
                // End stream with metadata
                let metadata = StreamMetadata {
                    total_tokens: None,
                    duration_ms: Some(start_time.elapsed().as_millis() as u64),
                    model: None,
                };
                let _ = stream_handle.end(Some(metadata)).await;

                return Ok(AgentOutput {
                    text: accumulated_text,
                    continue_loop: true,
                    tool_calls: all_tool_calls,
                    iterations: iteration,
                });
            }

            // Build the process input for this iteration
            let process_input = AgentProcessInput {
                session_id: input.session_id.clone(),
                iteration,
                content: current_content.clone(),
                history: history.clone(),
                trace_summary: trace_summary.take(),
            };

            // Call the Wasm agent
            let output = self.call_agent(&mut instance, &process_input).await?;

            // Stream the response text as it comes
            if !output.text.is_empty() {
                // Send the chunk via the stream handle
                if let Err(e) = stream_handle
                    .send_chunk(output.text.clone(), StreamFormat::Markdown)
                    .await
                {
                    tracing::warn!("Failed to send stream chunk: {}", e);
                    // Continue processing even if streaming fails
                }

                // Accumulate the text
                if !accumulated_text.is_empty() {
                    accumulated_text.push('\n');
                }
                accumulated_text.push_str(&output.text);
            }

            // Convert pending tool calls to ToolCall structs
            let tool_calls: Vec<ToolCall> = output
                .tool_calls
                .iter()
                .map(|tc| ToolCall {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                    result: None,
                })
                .collect();

            // Check if complete
            if output.complete {
                // Add any final tool calls
                all_tool_calls.extend(tool_calls);

                // End the stream with metadata
                let metadata = StreamMetadata {
                    total_tokens: None,
                    duration_ms: Some(start_time.elapsed().as_millis() as u64),
                    model: None,
                };
                let _ = stream_handle.end(Some(metadata)).await;

                return Ok(AgentOutput {
                    text: accumulated_text,
                    continue_loop: false,
                    tool_calls: all_tool_calls,
                    iterations: iteration + 1,
                });
            }

            // If there are pending tool calls, execute them
            if !output.tool_calls.is_empty() {
                let mut tool_results: Vec<ToolResult> = Vec::new();

                for pending in &output.tool_calls {
                    // Execute the tool
                    let result = self.execute_tool(pending).await;

                    // Record tool execution in trace
                    if let Some(tc) = &self.trace_collector {
                        let params_summary =
                            extract_tool_params_summary(&pending.name, &pending.arguments);
                        let (success, err_code, err_msg) = match &result.error {
                            Some(err) => {
                                (false, Some("TOOL_ERROR".to_string()), Some(err.clone()))
                            }
                            None => (true, None, None),
                        };
                        tc.record_tool_exec(
                            &input.session_id,
                            iteration,
                            &pending.name,
                            params_summary,
                            success,
                            err_code,
                            err_msg,
                            0, // duration_ms - not tracked at this level currently
                            None,
                            None,
                        );
                    }

                    // Track the tool call with its result
                    all_tool_calls.push(ToolCall {
                        id: pending.id.clone(),
                        name: pending.name.clone(),
                        arguments: pending.arguments.clone(),
                        result: result.content.clone(),
                    });

                    tool_results.push(result);
                }

                // Pattern detection - check for anomalous patterns
                if let (Some(tc), Some(engine)) =
                    (&self.trace_collector, &self.pattern_engine)
                {
                    let recent = tc.recent_tool_spans(&input.session_id, 10);
                    let ctx = DetectionContext {
                        session_id: &input.session_id,
                        iteration,
                        max_iterations: self.config.max_iterations,
                        recent_tool_spans: &recent,
                    };
                    trace_summary = engine.lock().unwrap().check(&ctx);
                }

                // Set up next iteration with tool results
                current_content = AgentContent::ToolResults {
                    results: tool_results,
                };
            }

            iteration += 1;
        }
    }
}

/// Extract key identifying fields from tool arguments for pattern detection.
fn extract_tool_params_summary(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    let key = match tool_name {
        "write_file" | "read_file" | "tool_read" | "tool_write" => "path",
        "web_fetch" | "tool_web_fetch" => "url",
        "web_search" | "tool_web_search" => "query",
        "tool_glob" => "pattern",
        "tool_bash" => "command",
        _ => return Some(tool_name.to_string()),
    };
    args.get(key)
        .map(|v| serde_json::json!({ key: v }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::streaming::{create_stream_channel, StreamEvent};

    fn create_test_wasm() -> Vec<u8> {
        wat::parse_str(
            r#"
            (module
                (func (export "get_abi_version") (result i32) i32.const 1)
                (memory (export "memory") 1)
            )
            "#,
        )
        .unwrap()
    }

    fn create_wrong_abi_wasm() -> Vec<u8> {
        wat::parse_str(
            r#"
            (module
                (func (export "get_abi_version") (result i32) i32.const 99)
                (memory (export "memory") 1)
            )
            "#,
        )
        .unwrap()
    }

    #[test]
    fn test_agent_runner_creation() {
        let wasm = create_test_wasm();
        let runner = AgentRunner::new(&wasm);
        assert!(runner.is_ok());
    }

    #[test]
    fn test_agent_runner_config() {
        let wasm = create_test_wasm();
        let config = AgentRunnerConfig {
            max_iterations: 10,
            iteration_timeout_ms: 5000,
        };
        let runner = AgentRunner::with_config(&wasm, config).unwrap();
        assert_eq!(runner.config().max_iterations, 10);
        assert_eq!(runner.config().iteration_timeout_ms, 5000);
    }

    #[test]
    fn test_agent_runner_config_default() {
        let config = AgentRunnerConfig::default();
        assert_eq!(config.max_iterations, 50);
        assert_eq!(config.iteration_timeout_ms, 30_000);
    }

    #[tokio::test]
    async fn test_agent_runner_run() {
        let wasm = create_test_wasm();
        let runner = AgentRunner::new(&wasm).unwrap();

        let input = AgentInput {
            session_id: "sess-001".to_string(),
            mode: AgentMode::Chat,
            user_message: "Hello".to_string(),
            history: vec![],
        };

        let output = runner.run(input).await;
        assert!(output.is_ok());
        let result = output.unwrap();
        assert!(result.text.contains("Hello"));
        assert!(!result.continue_loop);
        assert_eq!(result.iterations, 1);
        assert!(result.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn test_agent_runner_tracks_iterations() {
        let wasm = create_test_wasm();
        let runner = AgentRunner::new(&wasm).unwrap();

        let input = AgentInput {
            session_id: "sess-002".to_string(),
            mode: AgentMode::Code,
            user_message: "Run a test".to_string(),
            history: vec![],
        };

        let output = runner.run(input).await.unwrap();
        assert_eq!(output.iterations, 1, "Should complete in one iteration");
        assert!(!output.continue_loop);
    }

    #[tokio::test]
    async fn test_agent_runner_wrong_abi_version() {
        let wasm = create_wrong_abi_wasm();
        let runner = AgentRunner::new(&wasm).unwrap();

        let input = AgentInput {
            session_id: "sess-003".to_string(),
            mode: AgentMode::Chat,
            user_message: "Hello".to_string(),
            history: vec![],
        };

        let result = runner.run(input).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unsupported ABI version"));
    }

    #[test]
    fn test_agent_mode_default() {
        let mode = AgentMode::default();
        assert_eq!(mode, AgentMode::Chat);
    }

    #[test]
    fn test_agent_mode_serialization() {
        let mode = AgentMode::Browser;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, r#""browser""#);

        let deserialized: AgentMode = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, AgentMode::Browser);
    }

    #[test]
    fn test_agent_input_creation() {
        let input = AgentInput {
            session_id: "sess-001".to_string(),
            mode: AgentMode::Plan,
            user_message: "Create a plan".to_string(),
            history: vec![],
        };

        assert_eq!(input.session_id, "sess-001");
        assert_eq!(input.mode, AgentMode::Plan);
        assert_eq!(input.user_message, "Create a plan");
    }

    #[test]
    fn test_tool_call_structure() {
        let tool_call = ToolCall {
            id: "tc-001".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "/tmp/test.txt"}),
            result: Some("file contents".to_string()),
        };

        assert_eq!(tool_call.id, "tc-001");
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.arguments["path"], "/tmp/test.txt");
        assert_eq!(tool_call.result, Some("file contents".to_string()));
    }

    #[test]
    fn test_agent_output_structure() {
        let output = AgentOutput {
            text: "Response text".to_string(),
            continue_loop: false,
            tool_calls: vec![ToolCall {
                id: "tc-001".to_string(),
                name: "test_tool".to_string(),
                arguments: serde_json::json!({}),
                result: None,
            }],
            iterations: 3,
        };

        assert_eq!(output.text, "Response text");
        assert!(!output.continue_loop);
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.iterations, 3);
    }

    // Streaming tests

    #[tokio::test]
    async fn test_agent_runner_run_streaming() {
        let wasm = create_test_wasm();
        let runner = AgentRunner::new(&wasm).unwrap();

        let (tx, mut rx) = create_stream_channel(16);
        let stream_handle = StreamHandle::new("sess-001".to_string(), tx);

        let input = AgentInput {
            session_id: "sess-001".to_string(),
            mode: AgentMode::Chat,
            user_message: "Hello".to_string(),
            history: vec![],
        };

        // Run in a separate task so we can receive events
        let runner_task =
            tokio::spawn(async move { runner.run_streaming(input, stream_handle).await });

        // Collect stream events
        let mut chunks = Vec::new();
        let mut end_event = None;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Chunk(chunk) => chunks.push(chunk),
                StreamEvent::End(end) => {
                    end_event = Some(end);
                    break;
                }
            }
        }

        // Wait for the runner to complete
        let result = runner_task.await.unwrap();
        assert!(result.is_ok());

        let output = result.unwrap();
        assert!(output.text.contains("Hello"));
        assert!(!output.continue_loop);
        assert_eq!(output.iterations, 1);

        // Verify we received stream events
        assert!(!chunks.is_empty());
        assert!(end_event.is_some());

        // Verify the end event has metadata with duration
        let end = end_event.unwrap();
        assert!(end.metadata.is_some());
        let metadata = end.metadata.unwrap();
        assert!(metadata.duration_ms.is_some());
    }

    #[tokio::test]
    async fn test_agent_runner_streaming_returns_same_as_run() {
        let wasm = create_test_wasm();

        // Run without streaming
        let runner1 = AgentRunner::new(&wasm).unwrap();
        let input1 = AgentInput {
            session_id: "sess-001".to_string(),
            mode: AgentMode::Chat,
            user_message: "Test message".to_string(),
            history: vec![],
        };
        let output1 = runner1.run(input1).await.unwrap();

        // Run with streaming
        let runner2 = AgentRunner::new(&wasm).unwrap();
        let (tx, mut rx) = create_stream_channel(16);
        let stream_handle = StreamHandle::new("sess-002".to_string(), tx);

        let input2 = AgentInput {
            session_id: "sess-002".to_string(),
            mode: AgentMode::Chat,
            user_message: "Test message".to_string(),
            history: vec![],
        };

        let runner_task =
            tokio::spawn(async move { runner2.run_streaming(input2, stream_handle).await });

        // Drain the stream
        while let Some(event) = rx.recv().await {
            if event.is_end() {
                break;
            }
        }

        let output2 = runner_task.await.unwrap().unwrap();

        // Both should produce the same text and iterations
        assert_eq!(output1.text, output2.text);
        assert_eq!(output1.iterations, output2.iterations);
        assert_eq!(output1.continue_loop, output2.continue_loop);
    }

    #[tokio::test]
    async fn test_agent_runner_streaming_wrong_abi() {
        let wasm = create_wrong_abi_wasm();
        let runner = AgentRunner::new(&wasm).unwrap();

        let (tx, _rx) = create_stream_channel(16);
        let stream_handle = StreamHandle::new("sess-001".to_string(), tx);

        let input = AgentInput {
            session_id: "sess-001".to_string(),
            mode: AgentMode::Chat,
            user_message: "Hello".to_string(),
            history: vec![],
        };

        let result = runner.run_streaming(input, stream_handle).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unsupported ABI version"));
    }

    #[tokio::test]
    async fn test_agent_runner_streaming_continues_after_channel_close() {
        let wasm = create_test_wasm();
        let runner = AgentRunner::new(&wasm).unwrap();

        let (tx, rx) = create_stream_channel(16);
        let stream_handle = StreamHandle::new("sess-001".to_string(), tx);

        // Drop the receiver immediately to simulate channel close
        drop(rx);

        let input = AgentInput {
            session_id: "sess-001".to_string(),
            mode: AgentMode::Chat,
            user_message: "Hello".to_string(),
            history: vec![],
        };

        // The runner should still complete even if streaming fails
        let result = runner.run_streaming(input, stream_handle).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        assert!(output.text.contains("Hello"));
    }
}
