// crates/protocol/src/common.rs

//! Common types shared across protocol messages.

use serde::{Deserialize, Serialize};

/// Permission scope for authorization
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    /// Only this operation
    Once,
    /// This session only
    Session,
    /// Permanently authorized
    Always,
}

/// Resource type for permission requests
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    File,
    Script,
    Network,
    Mcp,
    Plugin,
}

/// Resource action for permission requests
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAction {
    Read,
    Write,
    Execute,
    Connect,
}

/// Requester type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequesterType {
    Agent,
    Plugin,
    Skill,
}

/// Stream format for responses
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StreamFormat {
    #[default]
    Markdown,
    Plain,
    Html,
}

/// Content type for content blocks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    Text,
    Markdown,
    Code,
    A2ui,
    Image,
}

/// Agent execution state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    #[default]
    Idle,
    Thinking,
    Executing,
    ExecutingTool,
    Waiting,
    WaitingResult,
    WaitingConfirmation,
    Complete,
    Error,
}

/// Tool execution status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Success,
    Failed,
}

/// Error severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorLevel {
    Warning,
    Error,
    Fatal,
}

/// Plan type for subscriptions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanType {
    Free,
    Pro,
    Team,
}

/// Plugin action
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginAction {
    Start,
    Stop,
    Restart,
}

/// Browser tool action
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserToolAction {
    Navigate,
    Click,
    Type,
    Fill,
    GetContent,
    Screenshot,
    EvalJs,
    WaitFor,
    Scroll,
    GetElement,
    QueryAll,
    Snapshot,
    ClickById,
    FillById,
    TypeById,
    GetMarkdown,
    /// Fetch URL content and save to cache file.
    ///
    /// Params:
    /// - `url`: URL to fetch
    /// - `timeout_ms`: Optional timeout (default 30000)
    /// - `include_images`: Optional, include images (default false)
    /// - `max_length`: Optional, max content length
    ///
    /// Returns:
    /// - `file_path`: Path to cached markdown file
    /// - `url`: Original URL
    /// - `title`: Page title
    /// - `content_length`: Content size in bytes
    /// - `cached`: Whether result was from cache
    WebFetch,
    /// Perform web search and return results.
    ///
    /// Params:
    /// - `query`: Search query string
    /// - `max_results`: Optional, max number of results (default 10)
    /// - `timeout_ms`: Optional timeout (default 30000)
    ///
    /// Returns:
    /// - `results`: Array of {title, url, snippet}
    /// - `query`: Original query
    /// - `total_results`: Total number of results found
    WebSearch,
    /// Ask the user a question and wait for response.
    ///
    /// Params:
    /// - `question`: Question text to display
    /// - `options`: Array of option strings (can be empty for free text)
    /// - `allow_custom`: Optional, allow custom text input (default true)
    /// - `timeout_ms`: Optional timeout for user response (default 60000)
    ///
    /// Returns:
    /// - `answer`: User's response text
    /// - `is_custom`: Whether the answer was custom input
    /// - `selected_index`: Index of selected option (-1 if custom)
    AskUser,
}

/// File attachment
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Filename
    pub name: String,
    /// MIME type
    pub mime_type: String,
    /// Base64 encoded data
    pub data: String,
}

/// Entity that requested a permission
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requester {
    /// Type of requester
    #[serde(rename = "type")]
    pub requester_type: RequesterType,
    /// Requester ID
    pub id: String,
    /// Requester display name
    pub name: String,
}

/// Step information for multi-step operations
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepInfo {
    /// Current step number
    pub current: u32,
    /// Total steps
    pub total: u32,
    /// Step description
    pub description: Option<String>,
}

/// Tool execution information
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Tool name
    pub name: String,
    /// Execution status
    pub status: ToolStatus,
    /// Target resource
    pub target: Option<String>,
}

/// Stream metadata
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamMetadata {
    /// Total tokens used
    pub total_tokens: Option<u32>,
    /// Duration in milliseconds
    pub duration_ms: Option<u64>,
    /// Model used
    pub model: Option<String>,
}

/// Account information
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
}

/// Plan information
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanInfo {
    #[serde(rename = "type")]
    pub plan_type: PlanType,
    pub name: String,
    pub expires_at: Option<String>,
}

/// Usage quota
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageQuota {
    pub used: u32,
    pub limit: u32,
    pub resets_at: Option<String>,
}

/// Quota information
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaInfo {
    pub llm_calls: Option<UsageQuota>,
}

/// Browser tool error
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserToolError {
    pub code: i32,
    pub message: String,
    pub recoverable: bool,
}

/// System error
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemError {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_scope_serialization() {
        assert_eq!(
            serde_json::to_string(&PermissionScope::Once).unwrap(),
            "\"once\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionScope::Session).unwrap(),
            "\"session\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionScope::Always).unwrap(),
            "\"always\""
        );
    }

    #[test]
    fn test_resource_type_serialization() {
        assert_eq!(
            serde_json::to_string(&ResourceType::File).unwrap(),
            "\"file\""
        );
        assert_eq!(
            serde_json::to_string(&ResourceType::Script).unwrap(),
            "\"script\""
        );
    }

    #[test]
    fn test_agent_state_serialization() {
        assert_eq!(
            serde_json::to_string(&AgentState::Idle).unwrap(),
            "\"idle\""
        );
        assert_eq!(
            serde_json::to_string(&AgentState::Thinking).unwrap(),
            "\"thinking\""
        );
        assert_eq!(
            serde_json::to_string(&AgentState::ExecutingTool).unwrap(),
            "\"executing_tool\""
        );
    }

    #[test]
    fn test_error_level_serialization() {
        assert_eq!(
            serde_json::to_string(&ErrorLevel::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorLevel::Error).unwrap(),
            "\"error\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorLevel::Fatal).unwrap(),
            "\"fatal\""
        );
    }

    #[test]
    fn test_attachment_roundtrip() {
        let attachment = Attachment {
            name: "test.txt".into(),
            mime_type: "text/plain".into(),
            data: "SGVsbG8gV29ybGQ=".into(),
        };
        let json = serde_json::to_string(&attachment).unwrap();
        let decoded: Attachment = serde_json::from_str(&json).unwrap();
        assert_eq!(attachment, decoded);
    }

    #[test]
    fn test_requester_roundtrip() {
        let requester = Requester {
            requester_type: RequesterType::Agent,
            id: "nevoflux-agent".into(),
            name: "NevoFlux Agent".into(),
        };
        let json = serde_json::to_string(&requester).unwrap();
        let decoded: Requester = serde_json::from_str(&json).unwrap();
        assert_eq!(requester, decoded);
    }
}
