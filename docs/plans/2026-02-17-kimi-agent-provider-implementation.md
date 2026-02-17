# Kimi-Agent LLM Provider Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add kimi-agent as a new CLI-based LLM provider using its JSON-RPC 2.0 wire protocol over stdin/stdout.

**Architecture:** Per-outer-turn subprocess lifecycle with persistent connection within a turn for mid-turn tool handling via wire protocol. Stateful wire client with explicit state machine bridges the wire protocol's `ToolCallRequest`/`ToolResult` pattern to rig's CompletionModel trait.

**Tech Stack:** Rust, tokio (async subprocess), serde_json (JSON-RPC), rig-core (CompletionModel trait), tokio-stream (streaming)

**Reference design doc:** `docs/plans/2026-02-17-kimi-agent-provider-design.md`

---

### Task 1: Wire Protocol Types (`types.rs`)

**Files:**
- Create: `crates/llm/src/providers/kimi_agent/types.rs`

**Step 1: Write the failing test**

Add the test at the bottom of `types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wire_message_envelope_deserialize_content_part() {
        let json = r#"{"type":"ContentPart","payload":{"type":"text","text":"hello"}}"#;
        let envelope: WireMessageEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.msg_type, "ContentPart");
        assert_eq!(envelope.payload["type"], "text");
        assert_eq!(envelope.payload["text"], "hello");
    }

    #[test]
    fn test_wire_message_envelope_deserialize_tool_call_request() {
        let json = r#"{"type":"ToolCallRequest","payload":{"id":"call_1","name":"read_file","arguments":"{\"path\":\"foo.txt\"}"}}"#;
        let envelope: WireMessageEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.msg_type, "ToolCallRequest");
        assert_eq!(envelope.payload["id"], "call_1");
        assert_eq!(envelope.payload["name"], "read_file");
    }

    #[test]
    fn test_wire_message_envelope_deserialize_turn_end() {
        let json = r#"{"type":"TurnEnd","payload":{}}"#;
        let envelope: WireMessageEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.msg_type, "TurnEnd");
    }

    #[test]
    fn test_wire_message_envelope_deserialize_status_update() {
        let json = r#"{"type":"StatusUpdate","payload":{"context_usage":0.5,"token_usage":{"input_other":100,"output":50,"input_cache_read":0,"input_cache_creation":0}}}"#;
        let envelope: WireMessageEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.msg_type, "StatusUpdate");
    }

    #[test]
    fn test_jsonrpc_event_deserialize() {
        let json = r#"{"jsonrpc":"2.0","method":"event","params":{"type":"ContentPart","payload":{"type":"text","text":"hi"}}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.method.as_deref(), Some("event"));
        assert!(msg.id.is_none());
    }

    #[test]
    fn test_jsonrpc_response_deserialize() {
        let json = r#"{"jsonrpc":"2.0","id":"1","result":{"protocol_version":"1.2","server":{"name":"kimi-agent","version":"0.1.0"}}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id.as_deref(), Some("1"));
        assert!(msg.result.is_some());
    }

    #[test]
    fn test_jsonrpc_request_from_server() {
        let json = r#"{"jsonrpc":"2.0","method":"request","id":"req_1","params":{"type":"ToolCallRequest","payload":{"id":"call_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.method.as_deref(), Some("request"));
        assert_eq!(msg.id.as_deref(), Some("req_1"));
    }

    #[test]
    fn test_jsonrpc_error_deserialize() {
        let json = r#"{"jsonrpc":"2.0","id":"1","error":{"code":-32000,"message":"Turn in progress"}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert!(msg.error.is_some());
        assert_eq!(msg.error.as_ref().unwrap().code, -32000);
    }

    #[test]
    fn test_initialize_request_serialize() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "initialize",
            id: "1".to_string(),
            params: serde_json::json!({
                "protocol_version": "1.2",
                "client": { "name": "nevoflux", "version": "0.1.0" },
                "external_tools": []
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"initialize\""));
        assert!(json.contains("\"id\":\"1\""));
    }

    #[test]
    fn test_prompt_request_serialize() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "prompt",
            id: "2".to_string(),
            params: serde_json::json!({
                "user_input": "Hello"
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"prompt\""));
        assert!(json.contains("\"user_input\":\"Hello\""));
    }

    #[test]
    fn test_tool_result_response_serialize() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0",
            id: "req_1".to_string(),
            result: serde_json::json!({
                "type": "ToolResult",
                "payload": {
                    "tool_call_id": "call_1",
                    "return_value": {
                        "is_error": false,
                        "output": "file contents",
                        "message": ""
                    }
                }
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"tool_call_id\":\"call_1\""));
    }

    #[test]
    fn test_kimi_completion_response_try_from_text_only() {
        let resp = KimiAgentCompletionResponse {
            content: "Hello world".to_string(),
            tool_calls: vec![],
            usage: KimiUsage { input_tokens: 10, output_tokens: 5 },
        };
        let completion: CompletionResponse<KimiAgentCompletionResponse> = resp.try_into().unwrap();
        assert_eq!(completion.usage.input_tokens, 10);
        assert_eq!(completion.usage.output_tokens, 5);
    }

    #[test]
    fn test_kimi_completion_response_try_from_with_tool_calls() {
        let resp = KimiAgentCompletionResponse {
            content: "I'll read the file.".to_string(),
            tool_calls: vec![
                ExtractedToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "foo.txt"}),
                },
            ],
            usage: KimiUsage { input_tokens: 20, output_tokens: 10 },
        };
        let completion: CompletionResponse<KimiAgentCompletionResponse> = resp.try_into().unwrap();
        // Should contain both text and tool call
        assert!(completion.choice.len() >= 2);
    }

    #[test]
    fn test_kimi_completion_response_empty_error() {
        let resp = KimiAgentCompletionResponse {
            content: "".to_string(),
            tool_calls: vec![],
            usage: KimiUsage::default(),
        };
        let result: Result<CompletionResponse<KimiAgentCompletionResponse>, _> = resp.try_into();
        assert!(result.is_err());
    }
}
```

**Step 2: Write the implementation**

```rust
//! Wire protocol types for the kimi-agent provider.
//!
//! Defines JSON-RPC 2.0 message types and wire protocol envelope structures
//! for communicating with the kimi-agent subprocess.

use rig::completion::{
    AssistantContent, CompletionError, CompletionResponse, ToolDefinition, Usage,
};
use rig::message::{ToolCall, ToolFunction};
use rig::OneOrMany;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Wire message envelope — the `{type, payload}` structure inside JSON-RPC params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessageEnvelope {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub payload: Map<String, Value>,
}

/// A JSON-RPC 2.0 message (union of request, notification, response, error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcMessage {
    pub jsonrpc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Outbound JSON-RPC request (client → server).
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub id: String,
    pub params: Value,
}

/// Outbound JSON-RPC response (client → server, for ToolCallRequest responses).
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: String,
    pub result: Value,
}

/// External tool definition for the initialize handshake.
#[derive(Debug, Serialize)]
pub struct WireExternalTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl From<&ToolDefinition> for WireExternalTool {
    fn from(td: &ToolDefinition) -> Self {
        Self {
            name: td.name.clone(),
            description: td.description.clone(),
            parameters: td.parameters.clone(),
        }
    }
}

/// A tool call extracted from wire protocol events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Token usage from kimi-agent StatusUpdate events.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KimiUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// Wrapper response for rig CompletionResponse conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiAgentCompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ExtractedToolCall>,
    pub usage: KimiUsage,
}

impl TryFrom<KimiAgentCompletionResponse> for CompletionResponse<KimiAgentCompletionResponse> {
    type Error = CompletionError;

    fn try_from(value: KimiAgentCompletionResponse) -> Result<Self, Self::Error> {
        let usage = Usage {
            input_tokens: value.usage.input_tokens,
            output_tokens: value.usage.output_tokens,
            total_tokens: value.usage.input_tokens + value.usage.output_tokens,
        };

        if value.content.is_empty() && value.tool_calls.is_empty() {
            return Err(CompletionError::ResponseError(
                "Empty response from kimi-agent".into(),
            ));
        }

        let mut contents: Vec<AssistantContent> = Vec::new();
        if !value.content.is_empty() {
            contents.push(AssistantContent::text(&value.content));
        }
        for tc in &value.tool_calls {
            contents.push(AssistantContent::ToolCall(ToolCall::new(
                tc.id.clone(),
                ToolFunction::new(tc.name.clone(), tc.arguments.clone()),
            )));
        }

        let choice = OneOrMany::many(contents).map_err(|_| {
            CompletionError::ResponseError("Empty response from kimi-agent".into())
        })?;

        Ok(CompletionResponse {
            choice,
            usage,
            raw_response: value,
        })
    }
}
```

**Step 3: Run test to verify it passes**

Run: `cargo test -p nevoflux-llm kimi_agent::types`
Expected: All tests PASS

**Step 4: Commit**

```bash
git add crates/llm/src/providers/kimi_agent/types.rs
git commit -m "feat(llm): add kimi-agent wire protocol types"
```

---

### Task 2: Client Configuration (`client.rs`)

**Files:**
- Create: `crates/llm/src/providers/kimi_agent/client.rs`

**Step 1: Write the failing test**

Add tests at the bottom of `client.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_new() {
        let client = KimiAgentClient::new("kimi-agent");
        assert_eq!(client.command(), "kimi-agent");
    }

    #[test]
    fn test_client_with_api_key() {
        let client = KimiAgentClient::new("kimi-agent").with_api_key("sk-test");
        assert_eq!(client.api_key(), Some("sk-test"));
    }

    #[test]
    fn test_client_with_model() {
        let client = KimiAgentClient::new("kimi-agent").with_model("kimi-latest");
        assert_eq!(client.model(), Some("kimi-latest"));
    }

    #[test]
    fn test_client_with_working_dir() {
        let client = KimiAgentClient::new("kimi-agent").with_working_dir("/tmp/workspace");
        assert_eq!(client.working_dir(), Some("/tmp/workspace"));
    }

    #[test]
    fn test_client_with_thinking() {
        let client = KimiAgentClient::new("kimi-agent").with_thinking(true);
        assert_eq!(client.thinking(), Some(true));
    }

    #[test]
    fn test_client_debug_redacts_api_key() {
        let client = KimiAgentClient::new("kimi-agent").with_api_key("super-secret");
        let debug = format!("{:?}", client);
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("<REDACTED>"));
    }

    #[test]
    fn test_client_defaults() {
        let client = KimiAgentClient::new("kimi-agent");
        assert!(client.api_key().is_none());
        assert!(client.model().is_none());
        assert!(client.working_dir().is_none());
        assert!(client.thinking().is_none());
    }

    #[test]
    fn test_client_clone() {
        let client = KimiAgentClient::new("kimi-agent").with_api_key("key");
        let cloned = client.clone();
        assert_eq!(cloned.command(), "kimi-agent");
        assert_eq!(cloned.api_key(), Some("key"));
    }

    #[test]
    fn test_completion_model_creation() {
        let client = KimiAgentClient::new("kimi-agent");
        let model = client.completion_model("kimi-latest");
        assert_eq!(model.model(), "kimi-latest");
    }
}
```

**Step 2: Write the implementation**

```rust
//! KimiAgentClient - configuration and builder for the kimi-agent subprocess.

use super::completion::KimiAgentCompletionModel;

/// Client for interacting with LLMs via the kimi-agent CLI (wire mode).
///
/// Manages configuration for spawning the kimi-agent subprocess.
/// The CLI communicates via JSON-RPC 2.0 over stdin/stdout.
#[derive(Clone)]
pub struct KimiAgentClient {
    command: String,
    api_key: Option<String>,
    model: Option<String>,
    working_dir: Option<String>,
    thinking: Option<bool>,
}

impl KimiAgentClient {
    /// Create a new KimiAgentClient with the given command path.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            api_key: None,
            model: None,
            working_dir: None,
            thinking: None,
        }
    }

    /// Set an API key (passed via environment variable to the subprocess).
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Set the default model name.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set the working directory for the subprocess.
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Set thinking mode (--thinking / --no-thinking).
    pub fn with_thinking(mut self, thinking: bool) -> Self {
        self.thinking = Some(thinking);
        self
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub(crate) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub(crate) fn working_dir(&self) -> Option<&str> {
        self.working_dir.as_deref()
    }

    pub(crate) fn thinking(&self) -> Option<bool> {
        self.thinking
    }

    /// Create a completion model for the specified model name.
    pub fn completion_model(&self, model: impl Into<String>) -> KimiAgentCompletionModel {
        KimiAgentCompletionModel::new(self.clone(), model)
    }
}

impl std::fmt::Debug for KimiAgentClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KimiAgentClient")
            .field("command", &self.command)
            .field("api_key", &self.api_key.as_ref().map(|_| "<REDACTED>"))
            .field("model", &self.model)
            .field("working_dir", &self.working_dir)
            .field("thinking", &self.thinking)
            .finish()
    }
}
```

**Step 3: Run test to verify it passes**

Run: `cargo test -p nevoflux-llm kimi_agent::client`
Expected: All tests PASS

**Step 4: Commit**

```bash
git add crates/llm/src/providers/kimi_agent/client.rs
git commit -m "feat(llm): add kimi-agent client configuration"
```

---

### Task 3: Wire Client (`wire.rs`)

**Files:**
- Create: `crates/llm/src/providers/kimi_agent/wire.rs`

This is the largest and most complex file. It manages the subprocess, JSON-RPC communication, and the state machine.

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wire_state_initial() {
        assert!(matches!(WireState::NoProcess, WireState::NoProcess));
    }

    #[test]
    fn test_build_initialize_request() {
        let tools = vec![ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let json = build_initialize_params(&tools);
        let params: serde_json::Value = serde_json::from_str(&serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(params["protocol_version"], "1.2");
        assert_eq!(params["external_tools"][0]["name"], "read_file");
    }

    #[test]
    fn test_build_prompt_params_text() {
        let params = build_prompt_params("Hello world");
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["user_input"], "Hello world");
    }

    #[test]
    fn test_build_tool_result_response() {
        let json = build_tool_result_envelope("call_1", false, "file contents", "");
        assert_eq!(json["type"], "ToolResult");
        assert_eq!(json["payload"]["tool_call_id"], "call_1");
        assert_eq!(json["payload"]["return_value"]["is_error"], false);
    }

    #[test]
    fn test_build_tool_result_error_response() {
        let json = build_tool_result_envelope("call_2", true, "", "tool not found");
        assert_eq!(json["payload"]["return_value"]["is_error"], true);
        assert_eq!(json["payload"]["return_value"]["message"], "tool not found");
    }

    #[test]
    fn test_parse_event_content_part() {
        let params = serde_json::json!({"type": "ContentPart", "payload": {"type": "text", "text": "hello"}});
        let event = parse_wire_event(&params);
        assert!(matches!(event, Some(WireEvent::ContentPart { text }) if text == "hello"));
    }

    #[test]
    fn test_parse_event_turn_end() {
        let params = serde_json::json!({"type": "TurnEnd", "payload": {}});
        let event = parse_wire_event(&params);
        assert!(matches!(event, Some(WireEvent::TurnEnd)));
    }

    #[test]
    fn test_parse_event_tool_call_request() {
        let params = serde_json::json!({"type": "ToolCallRequest", "payload": {"id": "call_1", "name": "bash", "arguments": "{\"cmd\":\"ls\"}"}});
        let event = parse_wire_event(&params);
        assert!(matches!(event, Some(WireEvent::ToolCallRequest { .. })));
    }

    #[test]
    fn test_parse_event_status_update() {
        let params = serde_json::json!({"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 100, "output": 50}}});
        let event = parse_wire_event(&params);
        assert!(matches!(event, Some(WireEvent::StatusUpdate { .. })));
    }

    #[test]
    fn test_parse_event_unknown_type() {
        let params = serde_json::json!({"type": "UnknownEvent", "payload": {}});
        let event = parse_wire_event(&params);
        assert!(matches!(event, Some(WireEvent::Unknown(_))));
    }
}
```

**Step 2: Write the implementation**

```rust
//! Wire client for the kimi-agent JSON-RPC 2.0 protocol.
//!
//! Manages the subprocess lifecycle, JSON-RPC communication, and state machine
//! for mid-turn tool handling.

use rig::completion::ToolDefinition;
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::client::KimiAgentClient;
use super::types::*;

/// Timeout for the initialize handshake.
const INIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Internal state of the wire client.
#[derive(Debug)]
pub(crate) enum WireState {
    NoProcess,
    Initializing,
    Ready,
    InTurn,
    WaitingForToolResults,
    Finished,
}

/// Parsed wire events from the kimi-agent subprocess.
#[derive(Debug)]
pub(crate) enum WireEvent {
    TurnBegin,
    TurnEnd,
    StepBegin { n: i64 },
    StepInterrupted,
    ContentPart { text: String },
    ThinkingPart { text: String },
    ToolCallRequest { id: String, name: String, arguments: Value },
    StatusUpdate { input_tokens: u64, output_tokens: u64 },
    Unknown(String),
}

/// The wire client that manages the kimi-agent subprocess.
pub(crate) struct WireClient {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    state: WireState,
    next_id: u64,
}

impl WireClient {
    /// Spawn a new kimi-agent subprocess and return a wire client.
    pub async fn spawn(config: &KimiAgentClient, model: &str) -> Result<Self, String> {
        let mut cmd = Command::new(config.command());
        cmd.arg("--yolo");

        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        } else if let Some(m) = config.model() {
            cmd.arg("--model").arg(m);
        }

        if let Some(dir) = config.working_dir() {
            cmd.arg("--work-dir").arg(dir);
        }

        match config.thinking() {
            Some(true) => { cmd.arg("--thinking"); }
            Some(false) => { cmd.arg("--no-thinking"); }
            None => {}
        }

        // Pass API key via env if set
        if let Some(api_key) = config.api_key() {
            cmd.env("MOONSHOT_API_KEY", api_key);
        }

        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn kimi-agent: {}", e))?;

        let stdin = child.stdin.take()
            .ok_or_else(|| "Failed to get kimi-agent stdin".to_string())?;
        let stdout = child.stdout.take()
            .ok_or_else(|| "Failed to get kimi-agent stdout".to_string())?;

        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            state: WireState::NoProcess,
            next_id: 1,
        })
    }

    /// Get the next request ID.
    fn next_request_id(&mut self) -> String {
        let id = self.next_id.to_string();
        self.next_id += 1;
        id
    }

    /// Send a JSON-RPC message (newline-delimited).
    async fn send_message(&mut self, value: &Value) -> Result<(), String> {
        // Check if process is still alive
        if let Ok(Some(status)) = self.child.try_wait() {
            return Err(format!("kimi-agent process exited with status: {}", status));
        }

        let json = serde_json::to_string(value)
            .map_err(|e| format!("Failed to serialize message: {}", e))?;

        self.stdin.write_all(json.as_bytes()).await
            .map_err(|e| format!("Failed to write to kimi-agent stdin: {}", e))?;
        self.stdin.write_all(b"\n").await
            .map_err(|e| format!("Failed to write newline: {}", e))?;
        self.stdin.flush().await
            .map_err(|e| format!("Failed to flush stdin: {}", e))?;

        Ok(())
    }

    /// Read one JSON-RPC line from stdout.
    async fn read_line(&mut self) -> Result<Option<JsonRpcMessage>, String> {
        let mut line = String::new();
        match self.stdout.read_line(&mut line).await {
            Ok(0) => {
                // EOF - process exited
                let status = self.child.try_wait()
                    .map_err(|e| format!("Failed to check process status: {}", e))?;
                Err(format!("kimi-agent process exited unexpectedly: {:?}", status))
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                let msg: JsonRpcMessage = serde_json::from_str(trimmed)
                    .map_err(|e| format!("Failed to parse JSON-RPC message: {} (line: {})", e, trimmed))?;
                Ok(Some(msg))
            }
            Err(e) => Err(format!("Failed to read from kimi-agent stdout: {}", e)),
        }
    }

    /// Send the initialize request and wait for response.
    pub async fn initialize(&mut self, tools: &[ToolDefinition]) -> Result<(), String> {
        self.state = WireState::Initializing;

        let id = self.next_request_id();
        let params = build_initialize_params(tools);
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "id": id,
            "params": params,
        });

        self.send_message(&request).await?;

        // Wait for response with timeout
        let result = tokio::time::timeout(INIT_TIMEOUT, async {
            loop {
                if let Some(msg) = self.read_line().await? {
                    if msg.id.as_deref() == Some(&id) {
                        if let Some(error) = msg.error {
                            return Err(format!("Initialize error ({}): {}", error.code, error.message));
                        }
                        return Ok(msg.result);
                    }
                    // Skip non-matching messages during init
                }
            }
        }).await;

        match result {
            Ok(Ok(_response)) => {
                self.state = WireState::Ready;
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = self.child.start_kill();
                Err(e)
            }
            Err(_) => {
                let _ = self.child.start_kill();
                Err("kimi-agent initialization timed out".to_string())
            }
        }
    }

    /// Send a prompt and start a turn.
    pub async fn send_prompt(&mut self, user_input: &str) -> Result<String, String> {
        let id = self.next_request_id();
        let params = build_prompt_params(user_input);
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "prompt",
            "id": id,
            "params": params,
        });

        self.send_message(&request).await?;
        self.state = WireState::InTurn;
        Ok(id)
    }

    /// Send a tool result response to a pending ToolCallRequest.
    pub async fn send_tool_result(
        &mut self,
        request_id: &str,
        tool_call_id: &str,
        is_error: bool,
        output: &str,
        error_message: &str,
    ) -> Result<(), String> {
        let envelope = build_tool_result_envelope(tool_call_id, is_error, output, error_message);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": envelope,
        });

        self.send_message(&response).await?;
        self.state = WireState::InTurn;
        Ok(())
    }

    /// Read events until the turn pauses (ToolCallRequest) or ends (TurnEnd).
    ///
    /// Returns accumulated (text, tool_calls, usage).
    pub async fn read_until_pause(
        &mut self,
    ) -> Result<(String, Vec<ExtractedToolCall>, KimiUsage), String> {
        let mut text = String::new();
        let mut tool_calls: Vec<ExtractedToolCall> = Vec::new();
        let mut usage = KimiUsage::default();

        loop {
            let msg = match self.read_line().await? {
                Some(msg) => msg,
                None => continue, // skip empty lines
            };

            // Handle event notifications
            if msg.method.as_deref() == Some("event") {
                if let Some(params) = &msg.params {
                    match parse_wire_event(params) {
                        Some(WireEvent::ContentPart { text: t }) => {
                            text.push_str(&t);
                        }
                        Some(WireEvent::ThinkingPart { .. }) => {
                            // Skip thinking content for now
                        }
                        Some(WireEvent::StatusUpdate { input_tokens, output_tokens }) => {
                            usage.input_tokens = input_tokens;
                            usage.output_tokens = output_tokens;
                        }
                        Some(WireEvent::TurnEnd) => {
                            self.state = WireState::Finished;
                            return Ok((text, tool_calls, usage));
                        }
                        Some(WireEvent::TurnBegin) | Some(WireEvent::StepBegin { .. }) | Some(WireEvent::StepInterrupted) => {
                            // Metadata events, skip
                        }
                        Some(WireEvent::ToolCallRequest { .. }) => {
                            // ToolCallRequest as event shouldn't happen, but handle gracefully
                        }
                        Some(WireEvent::Unknown(_)) | None => {
                            // Skip unknown events
                        }
                    }
                }
                continue;
            }

            // Handle request from server (ToolCallRequest)
            if msg.method.as_deref() == Some("request") {
                if let (Some(id), Some(params)) = (&msg.id, &msg.params) {
                    if let Some(WireEvent::ToolCallRequest { id: call_id, name, arguments }) = parse_wire_event(params) {
                        tool_calls.push(ExtractedToolCall {
                            id: call_id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        });
                        // Store the JSON-RPC request ID for later response
                        // The completion model will send the ToolResult when called again
                        self.state = WireState::WaitingForToolResults;
                        // We need to store pending request IDs
                        // For now, use the call_id as the request_id mapping
                        return Ok((text, tool_calls, usage));
                    }
                }
                continue;
            }

            // Handle prompt response (turn finished from JSON-RPC perspective)
            if msg.id.is_some() && msg.result.is_some() {
                // This is the prompt response; turn is done
                self.state = WireState::Finished;
                return Ok((text, tool_calls, usage));
            }

            // Handle errors
            if let Some(error) = &msg.error {
                return Err(format!("kimi-agent error ({}): {}", error.code, error.message));
            }
        }
    }

    /// Kill the subprocess.
    pub async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Get current state.
    pub fn state(&self) -> &WireState {
        &self.state
    }

    /// Take ownership of the child process (for ChildGuardStream).
    pub fn take_child(&mut self) -> Option<Child> {
        // Cannot take child while wire client is active
        None
    }
}

impl Drop for WireClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

// --- Helper functions ---

/// Build the params for an initialize request.
pub(crate) fn build_initialize_params(tools: &[ToolDefinition]) -> Value {
    let external_tools: Vec<WireExternalTool> = tools.iter().map(WireExternalTool::from).collect();
    serde_json::json!({
        "protocol_version": "1.2",
        "client": {
            "name": "nevoflux",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "external_tools": external_tools,
    })
}

/// Build the params for a prompt request.
pub(crate) fn build_prompt_params(user_input: &str) -> Value {
    serde_json::json!({
        "user_input": user_input,
    })
}

/// Build a ToolResult envelope for responding to a ToolCallRequest.
pub(crate) fn build_tool_result_envelope(
    tool_call_id: &str,
    is_error: bool,
    output: &str,
    message: &str,
) -> Value {
    serde_json::json!({
        "type": "ToolResult",
        "payload": {
            "tool_call_id": tool_call_id,
            "return_value": {
                "is_error": is_error,
                "output": output,
                "message": message,
            }
        }
    })
}

/// Parse a wire event from JSON-RPC params.
pub(crate) fn parse_wire_event(params: &Value) -> Option<WireEvent> {
    let msg_type = params.get("type")?.as_str()?;
    let payload = params.get("payload")?;

    match msg_type {
        "TurnBegin" => Some(WireEvent::TurnBegin),
        "TurnEnd" => Some(WireEvent::TurnEnd),
        "StepBegin" => {
            let n = payload.get("n").and_then(|v| v.as_i64()).unwrap_or(0);
            Some(WireEvent::StepBegin { n })
        }
        "StepInterrupted" => Some(WireEvent::StepInterrupted),
        "ContentPart" => {
            let content_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("text");
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            match content_type {
                "think" | "thinking" => Some(WireEvent::ThinkingPart { text }),
                _ => Some(WireEvent::ContentPart { text }),
            }
        }
        "ToolCallRequest" => {
            let id = payload.get("id").and_then(|v| v.as_str())?.to_string();
            let name = payload.get("name").and_then(|v| v.as_str())?.to_string();
            let arguments_str = payload.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
            let arguments: Value = serde_json::from_str(arguments_str).unwrap_or(Value::Object(Default::default()));
            Some(WireEvent::ToolCallRequest { id, name, arguments })
        }
        "StatusUpdate" => {
            let token_usage = payload.get("token_usage");
            let input_tokens = token_usage
                .and_then(|t| t.get("input_other"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output_tokens = token_usage
                .and_then(|t| t.get("output"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(WireEvent::StatusUpdate { input_tokens, output_tokens })
        }
        other => Some(WireEvent::Unknown(other.to_string())),
    }
}
```

**Step 3: Run test to verify it passes**

Run: `cargo test -p nevoflux-llm kimi_agent::wire`
Expected: All tests PASS

**Step 4: Commit**

```bash
git add crates/llm/src/providers/kimi_agent/wire.rs
git commit -m "feat(llm): add kimi-agent wire client with JSON-RPC protocol"
```

---

### Task 4: CompletionModel Implementation (`completion.rs`)

**Files:**
- Create: `crates/llm/src/providers/kimi_agent/completion.rs`

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completion_model_new() {
        let client = KimiAgentClient::new("kimi-agent");
        let model = KimiAgentCompletionModel::new(client, "kimi-latest");
        assert_eq!(model.model(), "kimi-latest");
    }

    #[test]
    fn test_build_prompt_from_messages() {
        let messages = vec![
            Message::user("Hello, read a file for me"),
        ];
        let prompt = build_prompt_text(&messages, None, &[]);
        assert!(prompt.contains("Hello, read a file for me"));
    }

    #[test]
    fn test_build_prompt_with_system() {
        let messages = vec![Message::user("Hi")];
        let prompt = build_prompt_text(&messages, Some("You are a helpful assistant"), &[]);
        assert!(prompt.contains("You are a helpful assistant"));
        assert!(prompt.contains("Hi"));
    }

    #[test]
    fn test_build_prompt_multiturn() {
        let messages = vec![
            Message::user("Hello"),
            Message::Assistant {
                content: OneOrMany::one(AssistantContent::text("Hi there")),
            },
            Message::user("How are you?"),
        ];
        let prompt = build_prompt_text(&messages, None, &[]);
        assert!(prompt.contains("Hello"));
        assert!(prompt.contains("Hi there"));
        assert!(prompt.contains("How are you?"));
    }
}
```

**Step 2: Write the implementation**

```rust
//! KimiAgentCompletionModel - implements rig's CompletionModel trait.
//!
//! Manages a per-turn kimi-agent subprocess with wire protocol communication.
//! Each outer turn spawns a new process. Within a turn, the process stays alive
//! for mid-turn tool handling via ToolCallRequest/ToolResult.

use futures::stream::StreamExt;
use rig::completion::{
    self, AssistantContent, CompletionError, CompletionRequest, CompletionResponse,
    Document, ToolDefinition, Usage,
};
use rig::message::{
    Message, ToolCall, ToolFunction, ToolResultContent, UserContent,
};
use rig::streaming::{RawStreamingChoice, RawStreamingToolCall, StreamingCompletionResponse};
use rig::OneOrMany;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::client::KimiAgentClient;
use super::types::*;
use super::wire::WireClient;

/// Completion model for kimi-agent CLI (wire mode).
#[derive(Clone)]
pub struct KimiAgentCompletionModel {
    client: KimiAgentClient,
    model: String,
    /// Persistent wire client for mid-turn tool handling.
    /// None = no active turn; Some = mid-turn, waiting for tool results.
    wire: Arc<Mutex<Option<WireClient>>>,
    /// Pending JSON-RPC request IDs for ToolCallRequest responses.
    pending_request_ids: Arc<Mutex<Vec<(String, String)>>>,  // (jsonrpc_id, tool_call_id)
}

impl KimiAgentCompletionModel {
    pub fn new(client: KimiAgentClient, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            wire: Arc::new(Mutex::new(None)),
            pending_request_ids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Build a text prompt from rig messages, system prompt, and documents.
pub(crate) fn build_prompt_text(
    messages: &[Message],
    system_prompt: Option<&str>,
    documents: &[Document],
) -> String {
    let mut parts = Vec::new();

    if let Some(sys) = system_prompt {
        if !sys.is_empty() {
            parts.push(format!("<system>\n{}\n</system>", sys));
        }
    }

    if !documents.is_empty() {
        let doc_text = documents.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n");
        parts.push(format!("<attachments>\n{}\n</attachments>", doc_text));
    }

    let has_assistant = messages.iter().any(|m| matches!(m, Message::Assistant { .. }));
    if has_assistant {
        parts.push("<conversation_history>".to_string());
        for msg in messages {
            match msg {
                Message::User { content } => {
                    let text = extract_user_text(content);
                    parts.push(format!("[user]: {}", text));
                }
                Message::Assistant { content, .. } => {
                    let text = extract_assistant_text(content);
                    parts.push(format!("[assistant]: {}", text));
                }
            }
        }
        parts.push("</conversation_history>".to_string());
        parts.push("Continue the conversation based on the history above.".to_string());
    } else {
        // Single turn - just the user message
        for msg in messages {
            if let Message::User { content } = msg {
                parts.push(extract_user_text(content));
            }
        }
    }

    parts.join("\n\n")
}

fn extract_user_text(content: &OneOrMany<UserContent>) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.clone()),
            UserContent::ToolResult(tr) => {
                let result_text: Vec<String> = tr
                    .content
                    .iter()
                    .filter_map(|rc| match rc {
                        ToolResultContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect();
                Some(format!("<tool_result call_id=\"{}\">\n{}\n</tool_result>", tr.id, result_text.join("")))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_assistant_text(content: &OneOrMany<AssistantContent>) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.clone()),
            AssistantContent::ToolCall(tc) => {
                Some(format!("<tool_call>\n{}\n</tool_call>",
                    serde_json::json!({"id": tc.id, "name": tc.function.name, "arguments": tc.function.arguments})
                ))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl completion::CompletionModel for KimiAgentCompletionModel {
    type Response = KimiAgentCompletionResponse;

    async fn completion(
        &self,
        completion_request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        let mut wire_guard = self.wire.lock().await;

        match wire_guard.as_mut() {
            None => {
                // New turn: spawn process, initialize, send prompt
                let mut wc = WireClient::spawn(&self.client, &self.model)
                    .await
                    .map_err(|e| CompletionError::ProviderError(e))?;

                // Build tool definitions
                let tools: Vec<ToolDefinition> = completion_request
                    .tools
                    .iter()
                    .cloned()
                    .collect();

                wc.initialize(&tools)
                    .await
                    .map_err(|e| CompletionError::ProviderError(e))?;

                // Build prompt from messages
                let prompt = build_prompt_text(
                    &completion_request.chat_history,
                    completion_request.preamble.as_deref(),
                    &completion_request.documents,
                );

                wc.send_prompt(&prompt)
                    .await
                    .map_err(|e| CompletionError::ProviderError(e))?;

                // Read events until pause or end
                let (text, tool_calls, usage) = wc.read_until_pause()
                    .await
                    .map_err(|e| CompletionError::ProviderError(e))?;

                if tool_calls.is_empty() {
                    wc.kill().await;
                    *wire_guard = None;
                } else {
                    *wire_guard = Some(wc);
                }

                let response = KimiAgentCompletionResponse {
                    content: text,
                    tool_calls,
                    usage,
                };
                response.try_into()
            }
            Some(wc) => {
                // Resuming: send tool results, continue reading
                let tool_results = extract_tool_results_from_request(&completion_request);
                for (call_id, content, is_error) in &tool_results {
                    // Use the call_id as the JSON-RPC response ID
                    // (the wire server uses the same ID for both)
                    wc.send_tool_result(call_id, call_id, *is_error, content, "")
                        .await
                        .map_err(|e| CompletionError::ProviderError(e))?;
                }

                let (text, tool_calls, usage) = wc.read_until_pause()
                    .await
                    .map_err(|e| CompletionError::ProviderError(e))?;

                if tool_calls.is_empty() {
                    wc.kill().await;
                    *wire_guard = None;
                } else {
                    // More tool calls, keep alive
                }

                let response = KimiAgentCompletionResponse {
                    content: text,
                    tool_calls,
                    usage,
                };
                response.try_into()
            }
        }
    }

    async fn stream(
        &self,
        _completion_request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::Response>, CompletionError> {
        // For now, streaming falls back to non-streaming completion
        // TODO: Implement true streaming with wire events
        Err(CompletionError::ProviderError(
            "Streaming not yet supported for kimi-agent provider".to_string(),
        ))
    }
}

/// Extract tool results from a CompletionRequest's chat history.
fn extract_tool_results_from_request(
    request: &CompletionRequest,
) -> Vec<(String, String, bool)> {
    let mut results = Vec::new();
    for msg in &request.chat_history {
        if let Message::User { content } = msg {
            for c in content.iter() {
                if let UserContent::ToolResult(tr) = c {
                    let text: String = tr
                        .content
                        .iter()
                        .filter_map(|rc| match rc {
                            ToolResultContent::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    results.push((tr.id.clone(), text, false));
                }
            }
        }
    }
    results
}
```

**Step 3: Run test to verify it passes**

Run: `cargo test -p nevoflux-llm kimi_agent::completion`
Expected: All tests PASS

**Step 4: Commit**

```bash
git add crates/llm/src/providers/kimi_agent/completion.rs
git commit -m "feat(llm): add kimi-agent CompletionModel implementation"
```

---

### Task 5: Module Setup and Provider Registration (`mod.rs` + `factory.rs`)

**Files:**
- Create: `crates/llm/src/providers/kimi_agent/mod.rs`
- Modify: `crates/llm/src/providers/mod.rs:8` — add `pub mod kimi_agent;`
- Modify: `crates/llm/src/factory.rs` — add `KimiAgent` variant to `ProviderType`

**Step 1: Create `mod.rs`**

```rust
//! Kimi-Agent CLI provider implementation (wire mode).
//!
//! Provides access to LLMs via the kimi-agent CLI subprocess,
//! communicating over JSON-RPC 2.0 stdin/stdout (wire protocol).

mod client;
mod completion;
mod types;
mod wire;

pub use client::KimiAgentClient;
pub use completion::KimiAgentCompletionModel;
```

**Step 2: Add module to providers/mod.rs**

Add `pub mod kimi_agent;` after line 9 in `crates/llm/src/providers/mod.rs`.

**Step 3: Add `KimiAgent` to `ProviderType` in `factory.rs`**

Add to the enum (after `GeminiCli` line 37):
```rust
    /// Kimi Agent CLI (subprocess, wire mode)
    KimiAgent,
```

Add to `FromStr` (after line 59):
```rust
            "kimi-agent" | "kimi_agent" | "kimi" => Ok(ProviderType::KimiAgent),
```

Add to `default_model_for` (after line 135):
```rust
        ProviderType::KimiAgent => "kimi-latest",
```

Add to `default_context_window_for` (after line 156):
```rust
        ProviderType::KimiAgent => 128_000,
```

Add to `api_key_env_var` (after line 177):
```rust
        ProviderType::KimiAgent => "MOONSHOT_API_KEY",
```

Update test arrays to include `ProviderType::KimiAgent`.

**Step 4: Run tests**

Run: `cargo test -p nevoflux-llm`
Expected: All tests PASS (including existing tests that iterate all providers)

**Step 5: Commit**

```bash
git add crates/llm/src/providers/kimi_agent/mod.rs crates/llm/src/providers/mod.rs crates/llm/src/factory.rs
git commit -m "feat(llm): register kimi-agent as ProviderType"
```

---

### Task 6: Daemon Config Integration

**Files:**
- Modify: `crates/daemon/src/config.rs` — add `kimi_agent` field + match arms

**Step 1: Add field to LlmConfig struct**

After the `gemini_cli` field (around line 249), add:
```rust
    /// Kimi Agent CLI-specific configuration.
    #[serde(default)]
    pub kimi_agent: ProviderConfig,
```

**Step 2: Add match arms**

In `active_api_key()` (around line 336), add:
```rust
            "kimi-agent" | "kimi_agent" => {
                self.kimi_agent.api_key.as_deref().or(Some("kimi-agent-cli"))
            }
```

In `active_model()` (around line 364), add:
```rust
            "kimi-agent" | "kimi_agent" => self.kimi_agent.model.as_deref(),
```

In `configured_providers()` (around line 390), add:
```rust
            ("kimi-agent", &self.kimi_agent),
```

In `context_window()` (around line 433), add:
```rust
            Some("kimi-agent") | Some("kimi_agent") => self.kimi_agent.context_window,
```

In `merge()` (around line 145), add:
```rust
        merge_provider(&mut self.llm.kimi_agent, &other.llm.kimi_agent);
```

In `Default` impl (around line 472), add:
```rust
            kimi_agent: ProviderConfig::default(),
```

**Step 3: Run tests**

Run: `cargo test -p nevoflux-daemon config`
Expected: All tests PASS

**Step 4: Commit**

```bash
git add crates/daemon/src/config.rs
git commit -m "feat(config): add kimi-agent provider configuration"
```

---

### Task 7: LLM Router Integration

**Files:**
- Modify: `crates/daemon/src/wasm/llm.rs` — add kimi-agent match arms

**Step 1: Add import**

At the top of the file (around line 9), add:
```rust
use nevoflux_llm::providers::kimi_agent::KimiAgentClient;
```

**Step 2: Add to `execute_llm_chat` match**

After the `GeminiCli` arm (around line 322), add:
```rust
        ProviderType::KimiAgent => {
            execute_kimi_agent_chat(api_key, model, request, provider).await
        }
```

**Step 3: Add the execution function**

After `execute_gemini_cli_chat` (around line 449), add:
```rust
/// Execute a chat request using the Kimi Agent CLI provider (wire mode).
async fn execute_kimi_agent_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    provider: ProviderType,
) -> Result<LlmChatResponse> {
    let client = if api_key == "kimi-agent-cli" {
        KimiAgentClient::new("kimi-agent")
    } else {
        KimiAgentClient::new("kimi-agent").with_api_key(api_key)
    };

    let workspace_dir = resolve_workspace_dir();
    let client = client.with_working_dir(workspace_dir);

    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request, provider).await
}
```

**Step 4: Add to `execute_llm_stream_inner` match**

After the `GeminiCli` arm (around line 1331), add:
```rust
        ProviderType::KimiAgent => stream_kimi_agent(api_key, model, request, tx, provider).await,
```

**Step 5: Add the streaming function**

After `stream_gemini_cli` (around line 1582), add:
```rust
/// Stream from Kimi Agent CLI provider.
async fn stream_kimi_agent(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
    _provider: ProviderType,
) -> Result<()> {
    // Streaming not yet supported for kimi-agent; fall back to non-streaming
    Err(DaemonError::InternalError(
        "Streaming not yet supported for kimi-agent provider".to_string(),
    ))
}
```

**Step 6: Run tests**

Run: `cargo test -p nevoflux-daemon wasm::llm`
Expected: All tests PASS

**Step 7: Commit**

```bash
git add crates/daemon/src/wasm/llm.rs
git commit -m "feat(daemon): add kimi-agent to LLM router"
```

---

### Task 8: Agent Host & Server Integration

**Files:**
- Modify: `crates/daemon/src/agent_host.rs` — add API key resolution
- Modify: `crates/daemon/src/server.rs` — add ProviderMeta and config lookups

**Step 1: Update agent_host.rs**

In `get_api_key_for_provider()` (around line 528), add:
```rust
            "kimi-agent" | "kimi_agent" => self.config.llm.kimi_agent.api_key.as_deref(),
```

After the `ClaudeCode` placeholder fallback (around line 546), add:
```rust
            Err(_) if pt == ProviderType::KimiAgent => {
                Ok("kimi-agent-cli".to_string())
            }
```

**Step 2: Update server.rs PROVIDER_METAS**

After the `gemini-cli` ProviderMeta entry (around line 4573), add:
```rust
    ProviderMeta {
        id: "kimi-agent",
        display_name: "Kimi Agent",
        provider_type: "cli",
        icon_bytes: include_bytes!("../../../assets/icons/providers/kimi-agent.webp"),
    },
```

Note: We'll need a `kimi-agent.webp` icon file. For now, use the moonshot/kimi logo or a placeholder.

**Step 3: Update `get_provider_config()` in server.rs**

Add match arm (around line 4596):
```rust
        "kimi-agent" | "kimi_agent" => Some(&llm.kimi_agent),
```

**Step 4: Update `get_provider_config_mut()` in server.rs**

Add match arm (around line 4621):
```rust
        "kimi-agent" | "kimi_agent" => Some(&mut llm.kimi_agent),
```

**Step 5: Update `is_active` check in server.rs**

Add to the alias check (around line 4648):
```rust
                || (meta.id == "kimi-agent" && active.as_deref() == Some("kimi_agent"))
```

And in the get endpoint (around line 4754):
```rust
        || (provider_id == "kimi-agent" && config.llm.active_provider() == Some("kimi_agent"))
```

**Step 6: Create placeholder icon**

Create an empty/placeholder icon at `assets/icons/providers/kimi-agent.webp`. If no icon available, temporarily reuse an existing one (like `deepseek.webp`).

**Step 7: Run tests**

Run: `cargo build --workspace`
Expected: BUILD succeeds

Run: `cargo test --workspace`
Expected: All tests PASS

**Step 8: Commit**

```bash
git add crates/daemon/src/agent_host.rs crates/daemon/src/server.rs assets/icons/providers/kimi-agent.webp
git commit -m "feat(daemon): integrate kimi-agent in agent host and server config API"
```

---

### Task 9: Build Verification & CI Check

**Files:** None (verification only)

**Step 1: Full build**

Run: `cargo build --workspace`
Expected: BUILD succeeds with no errors

**Step 2: Format check**

Run: `cargo fmt --check`
Expected: No formatting issues

**Step 3: Clippy check**

Run: `cargo clippy --workspace`
Expected: No warnings/errors

**Step 4: Full test suite**

Run: `cargo test --workspace`
Expected: All tests PASS

**Step 5: Fix any issues and commit**

If any issues found, fix them and commit:
```bash
git commit -m "fix: resolve build/lint issues in kimi-agent provider"
```

---

### Task 10: Integration Test (Optional, requires kimi-agent binary)

**Files:**
- Create: `crates/llm/tests/kimi_agent_provider.rs`

This test only runs if the `kimi-agent` binary is available on PATH.

**Step 1: Write the test**

```rust
//! Integration tests for kimi-agent provider.
//! These tests require the `kimi-agent` binary to be installed and configured.

use nevoflux_llm::providers::kimi_agent::KimiAgentClient;
use rig::completion::CompletionModel;

fn kimi_agent_available() -> bool {
    std::process::Command::new("kimi-agent")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn test_kimi_agent_simple_completion() {
    if !kimi_agent_available() {
        eprintln!("Skipping: kimi-agent not available");
        return;
    }

    let client = KimiAgentClient::new("kimi-agent");
    let model = client.completion_model("kimi-latest");

    let request = rig::completion::CompletionRequestBuilder::new(model.clone(), "What is 2+2? Answer with just the number.")
        .build();

    let response = model.completion(request).await;
    assert!(response.is_ok(), "Completion failed: {:?}", response.err());

    let resp = response.unwrap();
    let text = resp.choice.first().map(|c| match c {
        rig::completion::AssistantContent::Text(t) => t.text.clone(),
        _ => String::new(),
    }).unwrap_or_default();

    assert!(text.contains("4"), "Expected answer containing '4', got: {}", text);
}
```

**Step 2: Run test (if kimi-agent available)**

Run: `cargo test -p nevoflux-llm --test kimi_agent_provider -- --nocapture`
Expected: PASS if kimi-agent is installed, SKIP otherwise

**Step 3: Commit**

```bash
git add crates/llm/tests/kimi_agent_provider.rs
git commit -m "test(llm): add kimi-agent integration test"
```
