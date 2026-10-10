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

const TAB_ID: &str = "Tab to act in. Omit it to act in your own tab (the one your last \
browser_navigate opened); pass an id from browser_get_tabs to act in any other tab. An id \
must come from your own browser_navigate or a browser_get_tabs result in the current browser \
session; any other id answers tab_closed.";

fn tab_id() -> Value {
    json!({ "type": "integer", "description": TAB_ID })
}

fn element_id() -> Value {
    json!({ "type": "string", "description": "[eN] id from browser_snapshot" })
}

fn selector() -> Value {
    json!({ "type": "string", "description": "CSS selector" })
}

/// The schemas the agent channel publishes (M2a §3.2). Its own, not the MCP
/// catalogue's: these declare `tab_id` and say what omitting it means here.
/// Daemon-only keys (`background`, `window`, `workspace_dir`) are absent on
/// purpose — the backend also strips them if an agent sends them anyway.
pub fn agent_tools() -> Vec<Tool> {
    fn tool(name: &'static str, description: &'static str, properties: Value, required: &[&str]) -> Tool {
        Tool::new(
            name,
            description,
            schema(json!({ "type": "object", "properties": properties, "required": required })),
        )
    }
    vec![
        tool(
            "browser_snapshot",
            "Accessibility snapshot of the page, with [eN] element ids for the *_by_id tools.",
            json!({ "tab_id": tab_id() }),
            &[],
        ),
        tool(
            "browser_get_markdown",
            "The page's content as clean markdown. The cheapest way to read a page.",
            json!({ "tab_id": tab_id() }),
            &[],
        ),
        tool(
            "browser_screenshot",
            "A picture of the tab's visible viewport.",
            json!({
                "full_page": { "type": "boolean", "default": false, "description": "Capture the whole page instead of the viewport" },
                "tab_id": tab_id()
            }),
            &[],
        ),
        tool(
            "browser_get_tabs",
            "Every open tab with its id, title and URL. Tabs you opened carry owned: true; \
the person's own tabs carry owned: false — leave those alone unless asked.",
            json!({
                "scope": { "type": "string", "enum": ["all", "space", "folder", "live_folder"], "description": "Zen scope filter; omit for every tab outside live folders" },
                "scope_id": { "type": "string", "description": "Space, folder or live-folder id that narrows scope" }
            }),
            &[],
        ),
        tool(
            "browser_navigate",
            "Load a URL. Without tab_id it runs in your own tab, opening one in the background \
the first time; it never takes over the tab the person is looking at.",
            json!({
                "url": { "type": "string", "description": "Where to go: an http:// or https:// URL, or about:blank" },
                "new_tab": { "type": "boolean", "default": false, "description": "Open another tab of your own instead of reusing your current one" },
                "tab_id": tab_id()
            }),
            &["url"],
        ),
        tool(
            "browser_activate_tab",
            "Bring a tab to the front. This changes what the person is looking at.",
            json!({ "tab_id": { "type": "integer", "description": "The tab to show: one you opened, or one from a browser_get_tabs result in the current browser session" } }),
            &["tab_id"],
        ),
        tool(
            "browser_click_by_id",
            "Click an element by its [eN] id. Clicking an option id of a <select> chooses it.",
            json!({ "element_id": element_id(), "tab_id": tab_id() }),
            &["element_id"],
        ),
        tool(
            "browser_fill_by_id",
            "Set a form field's value by its [eN] id. On a <select>, pass an option's label or value.",
            json!({ "element_id": element_id(), "value": { "type": "string", "description": "Value to set" }, "tab_id": tab_id() }),
            &["element_id", "value"],
        ),
        tool(
            "browser_type_by_id",
            "Type text key by key into an element by its [eN] id. For autocomplete and search \
boxes; prefer browser_fill_by_id otherwise.",
            json!({ "element_id": element_id(), "text": { "type": "string", "description": "Text to type" }, "tab_id": tab_id() }),
            &["element_id", "text"],
        ),
        tool(
            "browser_click",
            "Click an element by CSS selector.",
            json!({ "selector": selector(), "tab_id": tab_id() }),
            &["selector"],
        ),
        tool(
            "browser_fill",
            "Set an input's value by CSS selector.",
            json!({ "selector": selector(), "value": { "type": "string", "description": "Value to set" }, "tab_id": tab_id() }),
            &["selector", "value"],
        ),
        tool(
            "browser_type",
            "Type text key by key into an element by CSS selector.",
            json!({ "selector": selector(), "text": { "type": "string", "description": "Text to type" }, "tab_id": tab_id() }),
            &["selector", "text"],
        ),
        tool(
            "browser_key_press",
            "Press a key, such as Enter, Tab or Escape.",
            json!({
                "key": { "type": "string", "description": "Key name" },
                "modifiers": { "type": "array", "items": { "type": "string" }, "description": "Modifier keys held down, e.g. [\"Control\"]" },
                "tab_id": tab_id()
            }),
            &["key"],
        ),
        tool(
            "browser_scroll",
            "Scroll the page.",
            json!({
                "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                "amount": { "type": "string", "description": "'page' (default), 'half', or a pixel count such as '300'" },
                "tab_id": tab_id()
            }),
            &["direction"],
        ),
        tool(
            "browser_wait_for",
            "Wait until an element appears.",
            json!({
                "element_id": element_id(),
                "selector": selector(),
                "timeout_ms": { "type": "integer", "default": 10000, "description": "Give up after this many milliseconds" },
                "tab_id": tab_id()
            }),
            &[],
        ),
        tool(
            "browser_upload_file",
            "Put a file from the desktop's NevoFlux workspace folder into an <input type=\"file\">.",
            json!({
                "selector": selector(),
                "file_path": { "type": "string", "description": "Absolute path of a file inside the workspace folder" },
                "tab_id": tab_id()
            }),
            &["selector", "file_path"],
        ),
    ]
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

/// What became of a question put to the agent's client (M2a §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Approval {
    /// Accepted with `approve: true`.
    Approved,
    /// Accepted with anything but `approve: true`, declined, or cancelled.
    Declined,
    /// Nobody answered in time.
    TimedOut,
    /// The client never declared it can be asked.
    Unsupported,
    /// The question could not be put (session gone, client error).
    Failed,
}

/// Puts one question to whoever is on the other end of the agent channel.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn ask(&self, message: String) -> Approval;
}

/// Nobody to ask: for backends that never need to, and for tests.
pub struct NoApprover;

#[async_trait]
impl Approver for NoApprover {
    async fn ask(&self, _message: String) -> Approval {
        Approval::Unsupported
    }
}

/// Whatever actually runs the tools behind an agent channel.
#[async_trait]
pub trait AgentToolBackend: Send + Sync + 'static {
    fn tools(&self) -> Vec<Tool>;
    /// Run one tool. `approver` is how a rule's question reaches the agent's
    /// client; it is valid only for the duration of this call.
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        approver: &dyn Approver,
    ) -> Result<CallToolResult, ErrorData>;
}

/// M1's production backend: the channel, envelope and handshake are real, the
/// tools arrive in M2. Saying so beats advertising tools that cannot run.
pub struct UnavailableBackend;

#[async_trait]
impl AgentToolBackend for UnavailableBackend {
    fn tools(&self) -> Vec<Tool> {
        Vec::new()
    }

    async fn call(
        &self,
        name: &str,
        _arguments: Value,
        _approver: &dyn Approver,
    ) -> Result<CallToolResult, ErrorData> {
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

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _approver: &dyn Approver,
    ) -> Result<CallToolResult, ErrorData> {
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
            .call("browser_snapshot", serde_json::json!({}), &NoApprover)
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
                &NoApprover,
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
            .call("browser_click", json!({}), &NoApprover)
            .await
            .expect("a tool error, not a protocol error");
        assert_eq!(out.structured_content.unwrap()["error"]["code"], "unknown_tool");
    }

    fn schema_of(t: &Tool) -> Value {
        Value::Object((*t.input_schema).clone())
    }

    #[test]
    fn the_agent_tools_are_exactly_the_whitelist() {
        let mut names: Vec<String> = agent_tools().iter().map(|t| t.name.to_string()).collect();
        names.sort();
        let mut want: Vec<String> = WHITELIST.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(names, want);
    }

    #[test]
    fn every_tab_scoped_tool_lets_the_agent_name_a_tab() {
        for t in agent_tools() {
            let s = schema_of(&t);
            if t.name == "browser_get_tabs" {
                assert!(s["properties"].get("tab_id").is_none());
                continue;
            }
            assert_eq!(s["properties"]["tab_id"]["type"], "integer", "{}", t.name);
        }
    }

    #[test]
    fn navigate_offers_a_new_tab_and_activating_needs_a_tab() {
        let tools = agent_tools();
        let nav = schema_of(tools.iter().find(|t| t.name == "browser_navigate").unwrap());
        assert_eq!(nav["properties"]["new_tab"]["type"], "boolean");
        assert_eq!(nav["required"], json!(["url"]));
        let act = schema_of(tools.iter().find(|t| t.name == "browser_activate_tab").unwrap());
        assert_eq!(act["required"], json!(["tab_id"]));
    }

    #[test]
    fn placement_and_workspace_are_never_the_agents_to_choose() {
        for t in agent_tools() {
            let props = schema_of(&t)["properties"].clone();
            for key in ["background", "window", "workspace_dir"] {
                assert!(props.get(key).is_none(), "{} offers {key}", t.name);
            }
        }
    }

    #[test]
    fn every_agent_tool_runs_below_full_auto_except_upload() {
        for name in WHITELIST {
            let min = nevoflux_protocol::minimum_tier(name);
            if *name == "browser_upload_file" {
                assert_eq!(min, ExecutionTier::FullAuto);
            } else {
                assert_ne!(min, ExecutionTier::FullAuto, "{name}");
            }
        }
    }

    #[test]
    fn the_agent_channel_cannot_reach_the_content_store_or_its_own_tier() {
        for name in ["content_store_set", "content_store.set", "remote.agent_set_tier", "remote.pairings"] {
            assert!(!is_allowed(name), "{name}");
            assert!(agent_tools().iter().all(|t| t.name != name), "{name}");
        }
    }
}
