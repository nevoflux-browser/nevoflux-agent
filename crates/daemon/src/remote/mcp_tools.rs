//! What an agent channel may call, and who answers (design §6).
//!
//! The whitelist is the boundary: `tools/list` shows only these and
//! `tools/call` refuses everything else, whatever the backend could do. A new
//! daemon tool is therefore unreachable over this channel until somebody adds
//! it here on purpose.

use async_trait::async_trait;
use nevoflux_mcp::rmcp::model::{CallToolResult, ContentBlock, ErrorCode, Tool};
use nevoflux_mcp::rmcp::ErrorData;
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
            other => {
                return Err(error_with_code(
                    "unknown_tool",
                    format!("the stub has no {other}"),
                ))
            }
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
}
