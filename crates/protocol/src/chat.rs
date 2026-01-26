// crates/protocol/src/chat.rs

//! Chat channel message definitions.
//!
//! Messages exchanged between Chat Sidebar and Agent via the Chat channel.

use crate::common::*;
use serde::{Deserialize, Serialize};

// ============================================================================
// Sidebar → Agent Messages
// ============================================================================

/// User chat message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Session ID
    pub session_id: String,
    /// Message ID (UUID)
    pub message_id: String,
    /// Message text
    pub text: String,
    /// Attachments
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Current tab ID
    pub tab_id: Option<i64>,
}

/// Skill command
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillCommand {
    /// Session ID
    pub session_id: String,
    /// Skill name
    pub skill_name: String,
    /// Skill arguments
    pub args: Option<serde_json::Value>,
}

/// Stop generation request
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopGeneration {
    /// Session ID
    pub session_id: String,
}

/// Permission response from user
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionResponse {
    /// Request ID
    pub request_id: String,
    /// Whether permission was granted
    pub granted: bool,
    /// Authorization scope
    pub scope: Option<PermissionScope>,
}

/// Plugin command
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommand {
    /// Plugin ID
    pub plugin_id: String,
    /// Action to perform
    pub action: PluginAction,
}

/// System command
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemCommand {
    /// Request ID
    pub request_id: String,
    /// Command name
    pub command: String,
    /// Command parameters
    pub params: Option<serde_json::Value>,
}

/// Browser tool response
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserToolResponse {
    /// Request ID
    pub request_id: String,
    /// Session ID
    pub session_id: String,
    /// Whether the operation succeeded
    pub success: bool,
    /// Result data
    pub result: Option<serde_json::Value>,
    /// Error information
    pub error: Option<BrowserToolError>,
}

// ============================================================================
// Agent → Sidebar Messages
// ============================================================================

/// Streaming response chunk
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamChunk {
    /// Session ID
    pub session_id: String,
    /// Stream ID
    pub stream_id: String,
    /// Incremental content
    pub delta: String,
    /// Content format
    pub format: StreamFormat,
}

/// Streaming response end marker
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamEnd {
    /// Session ID
    pub session_id: String,
    /// Stream ID
    pub stream_id: String,
    /// Stream metadata
    pub metadata: Option<StreamMetadata>,
}

/// Content block (code, image, etc.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlock {
    /// Session ID
    pub session_id: String,
    /// Block ID
    pub block_id: String,
    /// Content type
    pub content_type: ContentType,
    /// Content data
    pub content: serde_json::Value,
    /// Additional metadata
    pub metadata: Option<serde_json::Value>,
}

/// Permission request to user
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Request ID
    pub request_id: String,
    /// Session ID
    pub session_id: String,
    /// Resource type
    pub resource_type: ResourceType,
    /// Action requested
    pub action: ResourceAction,
    /// Resource identifier
    pub resource: String,
    /// Who is requesting
    pub requester: Requester,
    /// Reason for the request
    pub reason: String,
    /// Suggested scope
    pub scope: PermissionScope,
    /// Timeout in milliseconds
    pub timeout_ms: u64,
}

/// Agent state update
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStateMessage {
    /// Session ID
    pub session_id: String,
    /// Current state
    pub state: AgentState,
    /// Step information
    pub step: Option<StepInfo>,
    /// Tool information
    pub tool: Option<ToolInfo>,
    /// Progress (0.0 - 1.0)
    pub progress: Option<f32>,
}

/// Error notification
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorMessage {
    /// Session ID
    pub session_id: String,
    /// Error ID
    pub error_id: String,
    /// Error level
    pub level: ErrorLevel,
    /// Error code
    pub code: String,
    /// Error message
    pub message: String,
    /// Additional details
    pub details: Option<serde_json::Value>,
    /// Whether the error is recoverable
    pub recoverable: bool,
    /// Suggested retry action
    pub retry_action: Option<String>,
    /// Related request ID
    pub related_request_id: Option<String>,
}

/// Account status
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountStatus {
    /// Whether user is logged in
    pub logged_in: bool,
    /// Account information
    pub account: Option<AccountInfo>,
    /// Plan information
    pub plan: Option<PlanInfo>,
    /// Quota information
    pub quota: Option<QuotaInfo>,
}

/// System command response
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemResponse {
    /// Request ID
    pub request_id: String,
    /// Command that was executed
    pub command: String,
    /// Whether the command succeeded
    pub success: bool,
    /// Response data
    pub data: Option<serde_json::Value>,
    /// Error information
    pub error: Option<SystemError>,
}

/// Browser tool request
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserToolRequest {
    /// Request ID
    pub request_id: String,
    /// Session ID
    pub session_id: String,
    /// Tab ID
    pub tab_id: Option<i64>,
    /// Browser action
    pub action: BrowserToolAction,
    /// Action parameters
    pub params: serde_json::Value,
    /// Timeout in milliseconds
    pub timeout_ms: u64,
}

// ============================================================================
// Tagged Message Enums (for serialization with type field)
// ============================================================================

/// All messages from Sidebar to Agent
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum SidebarMessage {
    ChatMessage(ChatMessage),
    SkillCommand(SkillCommand),
    StopGeneration(StopGeneration),
    PermissionResponse(PermissionResponse),
    PluginCommand(PluginCommand),
    SystemCommand(SystemCommand),
    BrowserToolResponse(BrowserToolResponse),
}

/// All messages from Agent to Sidebar
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum AgentMessage {
    StreamChunk(StreamChunk),
    StreamEnd(StreamEnd),
    ContentBlock(ContentBlock),
    PermissionRequest(PermissionRequest),
    AgentState(AgentStateMessage),
    Error(ErrorMessage),
    AccountStatus(AccountStatus),
    SystemResponse(SystemResponse),
    BrowserToolRequest(BrowserToolRequest),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_message_serialization() {
        let msg = ChatMessage {
            session_id: "sess-001".into(),
            message_id: "msg-001".into(),
            text: "Hello, Agent!".into(),
            attachments: vec![],
            tab_id: Some(123),
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"session_id\":\"sess-001\""));

        let decoded: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_skill_command_serialization() {
        let cmd = SkillCommand {
            session_id: "sess-001".into(),
            skill_name: "web_search".into(),
            args: Some(serde_json::json!({"query": "rust async"})),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let decoded: SkillCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, decoded);
    }

    #[test]
    fn test_permission_response_serialization() {
        let resp = PermissionResponse {
            request_id: "perm-001".into(),
            granted: true,
            scope: Some(PermissionScope::Session),
        };

        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"granted\":true"));
        assert!(json.contains("\"scope\":\"session\""));
    }

    #[test]
    fn test_sidebar_message_tagged_serialization() {
        let msg = SidebarMessage::ChatMessage(ChatMessage {
            session_id: "sess-001".into(),
            message_id: "msg-001".into(),
            text: "Hello".into(),
            attachments: vec![],
            tab_id: None,
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"chat_message\""));
        assert!(json.contains("\"payload\""));

        let decoded: SidebarMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, SidebarMessage::ChatMessage(_)));
    }

    #[test]
    fn test_system_command_serialization() {
        let cmd = SystemCommand {
            request_id: "sys-001".into(),
            command: "mode.switch".into(),
            params: Some(serde_json::json!({"mode": "browser"})),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let decoded: SystemCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, decoded);
    }

    #[test]
    fn test_agent_message_tagged_serialization() {
        let msg = AgentMessage::StreamChunk(StreamChunk {
            session_id: "sess-001".into(),
            stream_id: "stream-001".into(),
            delta: "Hello".into(),
            format: StreamFormat::Markdown,
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"stream_chunk\""));

        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, AgentMessage::StreamChunk(_)));
    }

    #[test]
    fn test_permission_request_serialization() {
        let req = PermissionRequest {
            request_id: "perm-001".into(),
            session_id: "sess-001".into(),
            resource_type: ResourceType::File,
            action: ResourceAction::Read,
            resource: "/home/user/file.txt".into(),
            requester: Requester {
                requester_type: RequesterType::Agent,
                id: "agent-001".into(),
                name: "NevoFlux Agent".into(),
            },
            reason: "Reading configuration".into(),
            scope: PermissionScope::Session,
            timeout_ms: 60000,
        };

        let json = serde_json::to_string(&req).unwrap();
        let decoded: PermissionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_browser_tool_request_serialization() {
        let req = BrowserToolRequest {
            request_id: "bt-001".into(),
            session_id: "sess-001".into(),
            tab_id: Some(123),
            action: BrowserToolAction::Navigate,
            params: serde_json::json!({"url": "https://github.com"}),
            timeout_ms: 30000,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"action\":\"navigate\""));

        let decoded: BrowserToolRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_content_block_a2ui_serialization() {
        // Test A2UI content block for structured UI components
        let block = ContentBlock {
            session_id: "sess-001".into(),
            block_id: "block-001".into(),
            content_type: ContentType::A2ui,
            content: serde_json::json!({
                "component": "file_tree",
                "props": {
                    "root": "/home/user/project",
                    "files": [
                        {"name": "src", "type": "directory"},
                        {"name": "Cargo.toml", "type": "file"}
                    ]
                }
            }),
            metadata: Some(serde_json::json!({
                "interactive": true,
                "expandable": true
            })),
        };

        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"content_type\":\"a2ui\""));
        assert!(json.contains("\"component\":\"file_tree\""));

        let decoded: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, decoded);
    }

    #[test]
    fn test_content_block_a2ui_button_component() {
        // Test A2UI button component
        let block = ContentBlock {
            session_id: "sess-001".into(),
            block_id: "block-002".into(),
            content_type: ContentType::A2ui,
            content: serde_json::json!({
                "component": "action_buttons",
                "props": {
                    "buttons": [
                        {"label": "Approve", "action": "approve", "style": "primary"},
                        {"label": "Reject", "action": "reject", "style": "danger"}
                    ]
                }
            }),
            metadata: None,
        };

        let json = serde_json::to_string(&block).unwrap();
        let decoded: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, decoded);
        assert_eq!(decoded.content_type, ContentType::A2ui);
    }

    #[test]
    fn test_content_block_a2ui_progress_component() {
        // Test A2UI progress component
        let block = ContentBlock {
            session_id: "sess-001".into(),
            block_id: "block-003".into(),
            content_type: ContentType::A2ui,
            content: serde_json::json!({
                "component": "progress_indicator",
                "props": {
                    "current": 3,
                    "total": 10,
                    "label": "Processing files...",
                    "items": [
                        {"name": "file1.rs", "status": "complete"},
                        {"name": "file2.rs", "status": "complete"},
                        {"name": "file3.rs", "status": "in_progress"},
                        {"name": "file4.rs", "status": "pending"}
                    ]
                }
            }),
            metadata: Some(serde_json::json!({
                "auto_update": true,
                "update_interval_ms": 500
            })),
        };

        let json = serde_json::to_string(&block).unwrap();
        let decoded: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block.block_id, decoded.block_id);
        assert!(json.contains("\"progress_indicator\""));
    }

    #[test]
    fn test_agent_message_content_block_a2ui() {
        // Test A2UI content in AgentMessage envelope
        let msg = AgentMessage::ContentBlock(ContentBlock {
            session_id: "sess-001".into(),
            block_id: "block-004".into(),
            content_type: ContentType::A2ui,
            content: serde_json::json!({
                "component": "code_diff",
                "props": {
                    "language": "rust",
                    "original": "fn old() {}",
                    "modified": "fn new() {}"
                }
            }),
            metadata: None,
        });

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"content_block\""));
        assert!(json.contains("\"content_type\":\"a2ui\""));

        let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
        if let AgentMessage::ContentBlock(block) = decoded {
            assert_eq!(block.content_type, ContentType::A2ui);
        } else {
            panic!("Expected ContentBlock variant");
        }
    }
}
