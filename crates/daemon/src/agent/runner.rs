//! Agent runner for executing the Wasm agent.

use crate::error::{DaemonError, Result};
use crate::wasm::{HostServices, WasmInstance, WasmRuntime};
use nevoflux_protocol::ChatMessage;
use serde::{Deserialize, Serialize};

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
}

/// A tool call made by the agent.
#[derive(Debug, Clone)]
pub struct ToolCall {
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
    #[allow(dead_code)]
    services: Option<HostServices>,
}

impl AgentRunner {
    /// Create a new agent runner from Wasm bytes.
    pub fn new(wasm_bytes: &[u8]) -> Result<Self> {
        let runtime = WasmRuntime::from_bytes(wasm_bytes)?;
        Ok(Self {
            runtime,
            config: AgentRunnerConfig::default(),
            services: None,
        })
    }

    /// Create with custom config.
    pub fn with_config(wasm_bytes: &[u8], config: AgentRunnerConfig) -> Result<Self> {
        let runtime = WasmRuntime::from_bytes(wasm_bytes)?;
        Ok(Self {
            runtime,
            config,
            services: None,
        })
    }

    /// Set services for host functions.
    pub fn with_services(mut self, services: HostServices) -> Self {
        self.services = Some(services);
        self
    }

    /// Run the agent with the given input.
    pub async fn run(&self, input: AgentInput) -> Result<AgentOutput> {
        // Create instance
        let mut instance = WasmInstance::new(&self.runtime)?;

        // Check ABI version
        let abi_version = instance.get_abi_version()?;
        if abi_version != 1 {
            return Err(DaemonError::InternalError(format!(
                "Unsupported ABI version: {}",
                abi_version
            )));
        }

        // For now, return a placeholder response
        // Full implementation would:
        // 1. Serialize input to MessagePack
        // 2. Call Wasm entry point
        // 3. Handle tool calls
        // 4. Loop until done or max iterations

        Ok(AgentOutput {
            text: format!("Agent processed: {}", input.user_message),
            continue_loop: false,
            tool_calls: vec![],
        })
    }

    /// Get the configuration.
    pub fn config(&self) -> &AgentRunnerConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(output.unwrap().text.contains("Hello"));
    }
}
