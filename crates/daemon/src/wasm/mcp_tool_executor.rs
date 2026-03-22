//! MCP tool executor for ACP bridge mode.

use nevoflux_llm::providers::acp::mcp_bridge::ToolCallRequest;
use nevoflux_protocol::BrowserToolAction;
use tokio::sync::mpsc;

use super::services::{BrowserContext, BrowserRequest};

/// Run the tool executor loop.
pub async fn run_tool_executor(
    mut rx: mpsc::Receiver<ToolCallRequest>,
    browser_ctx: BrowserContext,
) {
    while let Some(req) = rx.recv().await {
        let result = execute_mcp_tool(&req.name, &req.arguments, &browser_ctx).await;
        let _ = req.result_tx.send(result);
    }
}

/// Map tool name to BrowserToolAction.
fn tool_name_to_action(name: &str) -> Option<BrowserToolAction> {
    let key = name.strip_prefix("browser_").unwrap_or(name);
    match key {
        "navigate" => Some(BrowserToolAction::Navigate),
        "go_back" => Some(BrowserToolAction::GoBack),
        "go_forward" => Some(BrowserToolAction::GoForward),
        "click" => Some(BrowserToolAction::Click),
        "click_by_id" => Some(BrowserToolAction::ClickById),
        "type" => Some(BrowserToolAction::Type),
        "type_by_id" => Some(BrowserToolAction::TypeById),
        "fill" => Some(BrowserToolAction::Fill),
        "fill_by_id" => Some(BrowserToolAction::FillById),
        "get_content" => Some(BrowserToolAction::GetContent),
        "get_markdown" => Some(BrowserToolAction::GetMarkdown),
        "screenshot" => Some(BrowserToolAction::Screenshot),
        "snapshot" => Some(BrowserToolAction::Snapshot),
        "eval_js" => Some(BrowserToolAction::EvalJs),
        "wait_for" => Some(BrowserToolAction::WaitFor),
        "wait_for_stable" => Some(BrowserToolAction::WaitForStable),
        "scroll" => Some(BrowserToolAction::Scroll),
        "get_element" => Some(BrowserToolAction::GetElement),
        "get_elements" => Some(BrowserToolAction::GetElements),
        "query_all" => Some(BrowserToolAction::QueryAll),
        "get_tabs" | "list_tabs" => Some(BrowserToolAction::ListTabs),
        "query_tabs" => Some(BrowserToolAction::QueryTabs),
        "key_press" => Some(BrowserToolAction::KeyPress),
        "read_artifact" => Some(BrowserToolAction::ReadArtifact),
        "edit_artifact" => Some(BrowserToolAction::EditArtifact),
        "ask_user" => Some(BrowserToolAction::AskUser),
        "web_search" => Some(BrowserToolAction::WebSearch),
        "fetch_page" => Some(BrowserToolAction::WebFetch),
        _ => None,
    }
}

/// Execute a single MCP tool call via BrowserContext.
pub async fn execute_mcp_tool(
    name: &str,
    arguments: &serde_json::Value,
    browser_ctx: &BrowserContext,
) -> Result<String, String> {
    use tokio::sync::oneshot;

    let action = tool_name_to_action(name)
        .ok_or_else(|| format!("unknown tool: {name}"))?;

    let tab_id = arguments.get("tab_id").and_then(|v| v.as_i64());

    let (response_tx, response_rx) = oneshot::channel();

    let request = BrowserRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id: String::new(),
        tab_id,
        action,
        params: arguments.clone(),
        timeout_ms: 30_000,
        client_identity: browser_ctx.client_identity.clone(),
        proxy_id: browser_ctx.proxy_id.clone(),
    };

    browser_ctx
        .sender
        .send((request, response_tx))
        .await
        .map_err(|e| format!("browser_sender failed: {e}"))?;

    let response = response_rx
        .await
        .map_err(|_| "browser response channel closed".to_string())?;

    if response.success {
        Ok(response
            .result
            .map(|v| serde_json::to_string(&v).unwrap_or_default())
            .unwrap_or_default())
    } else {
        Err(response
            .error
            .map(|e| format!("{}: {}", e.code, e.message))
            .unwrap_or_else(|| "unknown tool error".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_name_to_action() {
        assert_eq!(
            tool_name_to_action("browser_navigate"),
            Some(BrowserToolAction::Navigate)
        );
        assert_eq!(
            tool_name_to_action("browser_snapshot"),
            Some(BrowserToolAction::Snapshot)
        );
        assert_eq!(
            tool_name_to_action("web_search"),
            Some(BrowserToolAction::WebSearch)
        );
        assert_eq!(tool_name_to_action("unknown_tool"), None);
    }

    #[test]
    fn test_tool_name_to_action_bare_names() {
        assert_eq!(
            tool_name_to_action("navigate"),
            Some(BrowserToolAction::Navigate)
        );
        assert_eq!(
            tool_name_to_action("screenshot"),
            Some(BrowserToolAction::Screenshot)
        );
        assert_eq!(
            tool_name_to_action("get_tabs"),
            Some(BrowserToolAction::ListTabs)
        );
        assert_eq!(
            tool_name_to_action("list_tabs"),
            Some(BrowserToolAction::ListTabs)
        );
    }

    #[test]
    fn test_tool_name_to_action_all_variants() {
        let cases = [
            ("browser_go_back", BrowserToolAction::GoBack),
            ("browser_go_forward", BrowserToolAction::GoForward),
            ("browser_click", BrowserToolAction::Click),
            ("browser_click_by_id", BrowserToolAction::ClickById),
            ("browser_type", BrowserToolAction::Type),
            ("browser_type_by_id", BrowserToolAction::TypeById),
            ("browser_fill", BrowserToolAction::Fill),
            ("browser_fill_by_id", BrowserToolAction::FillById),
            ("browser_get_content", BrowserToolAction::GetContent),
            ("browser_get_markdown", BrowserToolAction::GetMarkdown),
            ("browser_eval_js", BrowserToolAction::EvalJs),
            ("browser_wait_for", BrowserToolAction::WaitFor),
            ("browser_wait_for_stable", BrowserToolAction::WaitForStable),
            ("browser_scroll", BrowserToolAction::Scroll),
            ("browser_get_element", BrowserToolAction::GetElement),
            ("browser_get_elements", BrowserToolAction::GetElements),
            ("browser_query_all", BrowserToolAction::QueryAll),
            ("browser_query_tabs", BrowserToolAction::QueryTabs),
            ("browser_key_press", BrowserToolAction::KeyPress),
            ("browser_read_artifact", BrowserToolAction::ReadArtifact),
            ("browser_edit_artifact", BrowserToolAction::EditArtifact),
            ("browser_ask_user", BrowserToolAction::AskUser),
            ("fetch_page", BrowserToolAction::WebFetch),
        ];
        for (name, expected) in cases {
            assert_eq!(tool_name_to_action(name), Some(expected), "failed for {name}");
        }
    }
}
