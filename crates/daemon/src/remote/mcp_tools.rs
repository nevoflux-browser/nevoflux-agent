//! What an agent channel may call, and who answers (design §6).
//!
//! The whitelist is the boundary: `tools/list` shows only these and
//! `tools/call` refuses everything else, whatever the backend could do. A new
//! daemon tool is therefore unreachable over this channel until somebody adds
//! it here on purpose.

use async_trait::async_trait;
use nevoflux_mcp::rmcp::model::{CallToolResult, ContentBlock, ErrorCode, Tool};
use nevoflux_mcp::rmcp::ErrorData;
use nevoflux_protocol::ExecutionTier;
use serde_json::{json, Value};

/// Every tool an agent channel can reach. Names are the executor's own
/// (`wasm::mcp_tool_executor`), not the design document's shorthand.
pub const WHITELIST: &[&str] = &[
    // observe
    "browser_snapshot",
    "browser_get_markdown",
    "browser_screenshot",
    "browser_get_tabs",
    // act
    "browser_navigate",
    "browser_activate_tab",
    "browser_click_by_id",
    "browser_fill_by_id",
    "browser_type_by_id",
    "browser_click",
    "browser_fill",
    "browser_type",
    "browser_key_press",
    "browser_scroll",
    "browser_wait_for",
    "browser_upload_file",
];

pub fn is_allowed(name: &str) -> bool {
    WHITELIST.contains(&name)
}

/// A JSON-RPC error whose `data.code` a client can branch on (design §12).
pub fn error_with_code(code: &'static str, message: impl Into<String>) -> ErrorData {
    ErrorData::new(
        ErrorCode::INVALID_PARAMS,
        message.into(),
        Some(json!({ "code": code })),
    )
}

/// `approval_denied` reasons (M2a §5). A closed set a client may branch on.
pub const REASON_DECLINED: &str = "declined";
pub const REASON_CLIENT_CANNOT_APPROVE: &str = "client_cannot_approve";
pub const REASON_RULE: &str = "rule";
pub const REASON_APPROVAL_FAILED: &str = "approval_failed";

/// Why an agent's tool call did not run, as the agent is told (M2a §5).
///
/// Every one is a normal tool result with `isError: true` - the call was
/// understood and answered - never a JSON-RPC error, which an MCP client
/// reports as the transport failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// Not on the whitelist.
    UnknownTool,
    /// Above this pairing's tier. Nobody is asked (decision A1).
    TierDenied {
        tier: ExecutionTier,
        required: ExecutionTier,
    },
    /// A person, a rule, or the client's inability to ask said no.
    ApprovalDenied {
        reason: &'static str,
        message: String,
    },
    /// Nobody answered the approval in time.
    ApprovalTimeout,
    /// No desktop browser to send the call to.
    BrowserUnavailable,
    /// No tab of the agent's own and none named.
    NoAgentTab,
    /// The tab is gone.
    TabClosed { tab_id: i64 },
    /// The browser ran the call and it failed.
    ToolFailed { detail: String },
}

impl AgentError {
    /// The machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            AgentError::UnknownTool => "unknown_tool",
            AgentError::TierDenied { .. } => "tier_denied",
            AgentError::ApprovalDenied { .. } => "approval_denied",
            AgentError::ApprovalTimeout => "approval_timeout",
            AgentError::BrowserUnavailable => "browser_unavailable",
            AgentError::NoAgentTab => "no_agent_tab",
            AgentError::TabClosed { .. } => "tab_closed",
            AgentError::ToolFailed { .. } => "tool_failed",
        }
    }

    /// `{"error": {"code", "message", ...extras}}` for a call to `tool`.
    pub fn body(&self, tool: &str) -> Value {
        let (message, extra): (String, Vec<(&str, Value)>) = match self {
            AgentError::UnknownTool => (
                format!("{tool} is not available over the agent channel"),
                vec![],
            ),
            AgentError::TierDenied { tier, required } => (
                format!(
                    "{tool} needs the {} tier; this agent is paired at {}. The person can raise it in NevoFlux with /agent-tier.",
                    required.as_setting(),
                    tier.as_setting()
                ),
                vec![
                    ("tier", json!(tier.as_setting())),
                    ("required", json!(required.as_setting())),
                ],
            ),
            AgentError::ApprovalDenied { reason, message } => {
                (message.clone(), vec![("reason", json!(reason))])
            }
            AgentError::ApprovalTimeout => (
                format!("nobody answered the approval for {tool} within 60 seconds"),
                vec![],
            ),
            AgentError::BrowserUnavailable => (
                "the NevoFlux desktop browser is not connected".to_string(),
                vec![],
            ),
            AgentError::NoAgentTab => (
                "you have no tab open yet: call browser_navigate first, or pass tab_id".to_string(),
                vec![],
            ),
            AgentError::TabClosed { tab_id } => {
                (format!("tab {tab_id} is closed"), vec![("tab_id", json!(tab_id))])
            }
            AgentError::ToolFailed { detail } => (
                format!("{tool} failed: {detail}"),
                vec![("detail", json!(detail))],
            ),
        };
        let mut error = serde_json::Map::new();
        error.insert("code".into(), json!(self.code()));
        error.insert("message".into(), json!(message));
        for (key, value) in extra {
            error.insert(key.to_string(), value);
        }
        json!({ "error": Value::Object(error) })
    }

    /// The tool result: `isError`, the object in `structuredContent`, and the
    /// same object as JSON text for clients that read only `content`.
    pub fn to_result(&self, tool: &str) -> CallToolResult {
        CallToolResult::structured_error(self.body(tool))
    }
}

/// Whatever actually runs the tools behind an agent channel.
#[async_trait]
pub trait AgentToolBackend: Send + Sync + 'static {
    fn tools(&self) -> Vec<Tool>;
    async fn call(&self, name: &str, arguments: Value) -> Result<CallToolResult, ErrorData>;
}

/// M1's production backend: the channel, envelope and handshake are real, the
/// tools arrive in M2. Saying so beats advertising tools that cannot run.
pub struct UnavailableBackend;

#[async_trait]
impl AgentToolBackend for UnavailableBackend {
    fn tools(&self) -> Vec<Tool> {
        Vec::new()
    }

    async fn call(&self, name: &str, _arguments: Value) -> Result<CallToolResult, ErrorData> {
        Err(error_with_code(
            "unavailable",
            format!("{name} is not available on this head yet"),
        ))
    }
}

/// Canned browser answers for tests and the stub head. Carries one tool
/// (`bash`) the whitelist must hide, so filtering is exercised for real.
pub struct StubBrowserBackend;

fn schema(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

#[async_trait]
impl AgentToolBackend for StubBrowserBackend {
    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool::new(
                "browser_snapshot",
                "Accessibility snapshot of a tab, with element ids",
                schema(json!({"type": "object", "properties": {"tab_id": {"type": "integer"}}})),
            ),
            Tool::new(
                "browser_navigate",
                "Open a URL in a new tab and return its id",
                schema(json!({
                    "type": "object",
                    "properties": {"url": {"type": "string"}},
                    "required": ["url"]
                })),
            ),
            Tool::new(
                "bash",
                "Run a shell command",
                schema(json!({"type": "object", "properties": {"command": {"type": "string"}}})),
            ),
        ]
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<CallToolResult, ErrorData> {
        let body = match name {
            "browser_snapshot" => json!({
                "title": "Example Domain",
                "url": "https://example.com/",
                "nodes": [{"id": 1, "role": "heading", "name": "Example Domain"}]
            }),
            "browser_navigate" => json!({
                "tab_id": 7,
                "url": arguments.get("url").cloned().unwrap_or(Value::Null)
            }),
            other => return Ok(AgentError::UnknownTool.to_result(other)),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whitelist_is_browser_tools_only_and_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for name in WHITELIST {
            assert!(name.starts_with("browser_"), "{name} is not a browser tool");
            assert!(seen.insert(*name), "{name} is listed twice");
        }
        assert_eq!(WHITELIST.len(), 16);
    }

    #[test]
    fn powerful_tools_stay_off_the_agent_channel() {
        for name in [
            "bash",
            "run_command",
            "read_file",
            "write_file",
            "browser_eval_js",
            "browser_ask_user",
            "browser_read_artifact",
            "browser_edit_artifact",
            "content_store_set",
            "loop_create",
            "schedule_create",
            "canvas_create",
            "remote.pair",
            "account.status",
        ] {
            assert!(
                !is_allowed(name),
                "{name} must not be reachable by an agent"
            );
        }
    }

    #[test]
    fn an_error_carries_its_code_in_data() {
        let e = error_with_code("not_allowed", "nope");
        assert_eq!(e.code.0, -32602);
        assert_eq!(e.data, Some(serde_json::json!({"code": "not_allowed"})));
    }

    #[tokio::test]
    async fn the_unavailable_backend_lists_nothing_and_says_why() {
        let b = UnavailableBackend;
        assert!(b.tools().is_empty());
        let err = b
            .call("browser_snapshot", serde_json::json!({}))
            .await
            .expect_err("nothing runs in M1");
        assert_eq!(err.data, Some(serde_json::json!({"code": "unavailable"})));
    }

    #[tokio::test]
    async fn the_stub_backend_answers_like_a_browser() {
        let b = StubBrowserBackend;
        let names: Vec<String> = b.tools().iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"browser_snapshot".to_string()));
        assert!(
            names.contains(&"bash".to_string()),
            "carries one tool the whitelist must hide"
        );
        let out = b
            .call(
                "browser_navigate",
                serde_json::json!({"url": "https://example.com/"}),
            )
            .await
            .unwrap();
        let text = serde_json::to_value(&out).unwrap().to_string();
        assert!(text.contains("https://example.com/"));
    }

    use nevoflux_protocol::ExecutionTier;

    fn every_error() -> Vec<(AgentError, &'static str)> {
        vec![
            (AgentError::UnknownTool, "unknown_tool"),
            (
                AgentError::TierDenied {
                    tier: ExecutionTier::ReadOnly,
                    required: ExecutionTier::BrowserAuto,
                },
                "tier_denied",
            ),
            (
                AgentError::ApprovalDenied {
                    reason: REASON_DECLINED,
                    message: "the person declined browser_click".into(),
                },
                "approval_denied",
            ),
            (AgentError::ApprovalTimeout, "approval_timeout"),
            (AgentError::BrowserUnavailable, "browser_unavailable"),
            (AgentError::NoAgentTab, "no_agent_tab"),
            (AgentError::TabClosed { tab_id: 7 }, "tab_closed"),
            (AgentError::ToolFailed { detail: "-1: Element not found".into() }, "tool_failed"),
        ]
    }

    #[test]
    fn every_error_carries_the_code_the_protocol_names() {
        for (e, code) in every_error() {
            assert_eq!(e.code(), code);
            assert_eq!(e.body("browser_click")["error"]["code"], code);
            assert!(e.body("browser_click")["error"]["message"].is_string());
        }
    }

    #[test]
    fn extra_fields_travel_with_their_codes() {
        let tier = AgentError::TierDenied {
            tier: ExecutionTier::ReadOnly,
            required: ExecutionTier::BrowserAuto,
        }
        .body("browser_click");
        assert_eq!(tier["error"]["tier"], "read-only");
        assert_eq!(tier["error"]["required"], "browser-auto");
        let denied = AgentError::ApprovalDenied {
            reason: REASON_CLIENT_CANNOT_APPROVE,
            message: "m".into(),
        }
        .body("x");
        assert_eq!(denied["error"]["reason"], "client_cannot_approve");
        assert_eq!(AgentError::TabClosed { tab_id: 7 }.body("x")["error"]["tab_id"], 7);
        assert_eq!(
            AgentError::ToolFailed { detail: "boom".into() }.body("x")["error"]["detail"],
            "boom"
        );
        assert_eq!(
            AgentError::NoAgentTab.body("x")["error"].as_object().unwrap().len(),
            2,
            "code and message only"
        );
    }

    #[test]
    fn an_error_is_a_tool_result_not_a_protocol_error() {
        let result = AgentError::NoAgentTab.to_result("browser_snapshot");
        let body = AgentError::NoAgentTab.body("browser_snapshot");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, Some(body.clone()));
        let text = result.content[0].as_text().expect("text").text.clone();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), body);
    }

    #[tokio::test]
    async fn the_stub_answers_a_whitelisted_tool_it_lacks_with_unknown_tool() {
        let out = StubBrowserBackend
            .call("browser_click", json!({}))
            .await
            .expect("a tool error, not a protocol error");
        assert_eq!(out.structured_content.unwrap()["error"]["code"], "unknown_tool");
    }
}
