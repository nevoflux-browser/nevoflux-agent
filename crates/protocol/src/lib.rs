// crates/protocol/src/lib.rs

//! NevoFlux Protocol - IPC message definitions for Agent communication.

pub mod channel;
pub mod chat;
pub mod common;
pub mod envelope;
mod error;

pub use channel::Channel;
pub use envelope::{AuthInfo, DaemonEnvelope, ProxyEnvelope};
pub use common::{
    AccountInfo, AgentState, Attachment, BrowserToolAction, BrowserToolError, ContentType,
    ErrorLevel, PermissionScope, PlanInfo, PlanType, PluginAction, QuotaInfo, Requester,
    RequesterType, ResourceAction, ResourceType, StepInfo, StreamFormat, StreamMetadata,
    SystemError, ToolInfo, ToolStatus, UsageQuota,
};
pub use chat::{
    // Sidebar → Agent messages
    ChatMessage, SkillCommand, StopGeneration, PermissionResponse, PluginCommand,
    SystemCommand, BrowserToolResponse,
    // Agent → Sidebar messages
    StreamChunk, StreamEnd, ContentBlock, PermissionRequest, AgentStateMessage,
    ErrorMessage, AccountStatus, SystemResponse, BrowserToolRequest,
    // Tagged enums
    SidebarMessage, AgentMessage,
};
pub use error::{ProtocolError, Result};

/// Protocol version
pub const PROTOCOL_VERSION: &str = "5.0.0";

/// Get the protocol version
pub fn get_protocol_version() -> &'static str {
    PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version() {
        assert_eq!(get_protocol_version(), "5.0.0");
    }
}
