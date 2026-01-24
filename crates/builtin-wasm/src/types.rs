//! Types for host-guest communication.
//!
//! These types are serialized via MessagePack for efficient transfer.

use serde::{Deserialize, Serialize};

/// Agent execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Chat mode - dialogue + current page understanding.
    Chat,
    /// Browser mode - active browser control.
    Browser,
    /// Agent mode - full capabilities including file/bash/computer use.
    Agent,
}

impl Default for AgentMode {
    fn default() -> Self {
        Self::Chat
    }
}

/// Message role in conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// System message (instructions).
    System,
    /// User message.
    User,
    /// Assistant (AI) message.
    Assistant,
    /// Tool use message.
    Tool,
}

/// A conversation message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Message role.
    pub role: MessageRole,
    /// Message content.
    pub content: String,
    /// Optional tool call ID (for tool responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// Create a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tool_call_id: None,
        }
    }

    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_call_id: None,
        }
    }

    /// Create an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_call_id: None,
        }
    }

    /// Create a tool response message.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Tool definition for LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON schema for input parameters.
    pub input_schema: serde_json::Value,
}

/// Tool call request from LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique ID for this tool call.
    pub id: String,
    /// Tool name to invoke.
    pub name: String,
    /// Tool arguments as JSON.
    pub arguments: serde_json::Value,
}

/// Tool execution result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Tool call ID.
    pub tool_call_id: String,
    /// Result content.
    pub content: String,
    /// Whether the execution was successful.
    pub success: bool,
}

/// LLM request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Messages to send.
    pub messages: Vec<Message>,
    /// Available tools.
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    /// Whether to stream the response.
    #[serde(default)]
    pub stream: bool,
}

/// LLM response chunk (for streaming).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmChunk {
    /// Text content delta.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Tool calls in this chunk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Whether this is the final chunk.
    #[serde(default)]
    pub done: bool,
}

/// LLM response (non-streaming).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// Full response text.
    pub text: String,
    /// Tool calls from the response.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// Agent input from host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInput {
    /// Session ID.
    pub session_id: String,
    /// Current mode.
    pub mode: AgentMode,
    /// User message.
    pub user_message: String,
    /// Conversation history.
    #[serde(default)]
    pub history: Vec<Message>,
}

/// Agent output to host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    /// Response text.
    pub text: String,
    /// Tool calls made during execution.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Whether the agent loop should continue.
    pub continue_loop: bool,
}

/// Skill summary (from skill_list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    /// Skill name.
    pub name: String,
    /// Brief description.
    pub description: String,
    /// Tags for categorization.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Memory chunk for search results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryChunk {
    /// Chunk ID.
    pub id: String,
    /// Content text.
    pub content: String,
    /// Source session ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Relevance score.
    pub score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_mode_default() {
        assert_eq!(AgentMode::default(), AgentMode::Chat);
    }

    #[test]
    fn test_message_constructors() {
        let sys = Message::system("You are helpful");
        assert_eq!(sys.role, MessageRole::System);
        assert_eq!(sys.content, "You are helpful");

        let user = Message::user("Hello");
        assert_eq!(user.role, MessageRole::User);

        let assistant = Message::assistant("Hi there");
        assert_eq!(assistant.role, MessageRole::Assistant);

        let tool = Message::tool("call-123", "Result");
        assert_eq!(tool.role, MessageRole::Tool);
        assert_eq!(tool.tool_call_id, Some("call-123".into()));
    }

    #[test]
    fn test_message_serialization() {
        let msg = Message::user("Test message");
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.role, MessageRole::User);
        assert_eq!(decoded.content, "Test message");
    }

    #[test]
    fn test_tool_definition() {
        let tool = ToolDefinition {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            }),
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("read_file"));
    }

    #[test]
    fn test_tool_call() {
        let call = ToolCall {
            id: "call-001".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls -la"}),
        };
        assert_eq!(call.name, "bash");
    }

    #[test]
    fn test_tool_result() {
        let result = ToolResult {
            tool_call_id: "call-001".into(),
            content: "file1.txt\nfile2.txt".into(),
            success: true,
        };
        assert!(result.success);
    }

    #[test]
    fn test_agent_input() {
        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Agent,
            user_message: "Help me".into(),
            history: vec![Message::system("You are helpful")],
        };
        assert_eq!(input.mode, AgentMode::Agent);
        assert_eq!(input.history.len(), 1);
    }

    #[test]
    fn test_agent_output() {
        let output = AgentOutput {
            text: "Here's my response".into(),
            tool_calls: vec![],
            continue_loop: false,
        };
        assert!(!output.continue_loop);
    }

    #[test]
    fn test_llm_request() {
        let req = LlmRequest {
            messages: vec![Message::user("Hello")],
            tools: vec![],
            stream: false,
        };
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn test_llm_response() {
        let resp = LlmResponse {
            text: "Hello there".into(),
            tool_calls: vec![],
        };
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn test_llm_chunk() {
        let chunk = LlmChunk {
            text: Some("Hello".into()),
            tool_calls: vec![],
            done: false,
        };
        assert!(!chunk.done);
    }

    #[test]
    fn test_skill_summary() {
        let skill = SkillSummary {
            name: "code-review".into(),
            description: "Review code".into(),
            tags: vec!["code".into()],
        };
        assert_eq!(skill.tags.len(), 1);
    }

    #[test]
    fn test_memory_chunk() {
        let chunk = MemoryChunk {
            id: "mem-001".into(),
            content: "Some memory".into(),
            session_id: Some("sess-001".into()),
            score: 0.95,
        };
        assert!(chunk.score > 0.9);
    }
}
