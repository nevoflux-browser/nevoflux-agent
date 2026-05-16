//! Registry of daemon-emitted events observable by eval clients.
//!
//! See docs/superpowers/specs/2026-05-15-browser-use-eval-design.md §6.2.2.
//!
//! **Rename = breaking change.** PR must be tagged `[eval-breaking]`,
//! all references in `eval/nevoflux-suite/` updated, and an alias kept
//! for at least one release.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonEventName {
    /// A Canvas micro-app was successfully created and rendered.
    CanvasAppCreated,
    /// A micro-app called back into the agent via NevoFluxSDK.agent.chat().
    CanvasSdkChatInvoked,
    /// Daemon's permission gate blocked a tool call.
    ToolCallBlocked,
    /// MCP server-mode received a call from a remote client.
    McpServerCallReceived,
    /// MCP client-mode successfully invoked an external server.
    McpClientCallSucceeded,
}

impl DaemonEventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CanvasAppCreated => "canvas_app_created",
            Self::CanvasSdkChatInvoked => "canvas_sdk_chat_invoked",
            Self::ToolCallBlocked => "tool_call_blocked",
            Self::McpServerCallReceived => "mcp_server_call_received",
            Self::McpClientCallSucceeded => "mcp_client_call_succeeded",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_serde() {
        let json = serde_json::to_string(&DaemonEventName::CanvasAppCreated).unwrap();
        assert_eq!(json, "\"canvas_app_created\"");
        let back: DaemonEventName = serde_json::from_str("\"canvas_sdk_chat_invoked\"").unwrap();
        assert_eq!(back, DaemonEventName::CanvasSdkChatInvoked);
    }

    #[test]
    fn as_str_matches_serde() {
        for v in [
            DaemonEventName::CanvasAppCreated,
            DaemonEventName::CanvasSdkChatInvoked,
            DaemonEventName::ToolCallBlocked,
            DaemonEventName::McpServerCallReceived,
            DaemonEventName::McpClientCallSucceeded,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()));
        }
    }
}
