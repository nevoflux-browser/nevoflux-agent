//! MCP-over-HTTP bridge for native tool calling.
//!
//! Provides the bridge between the MCP HTTP server and NevoFlux tool execution.
//! Used when `AcpProviderConfig::use_mcp_bridge` is true (Claude Code).

use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Tool definition for MCP tools/list.
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Request sent from MCP tool handler to daemon executor.
pub struct ToolCallRequest {
    pub name: String,
    pub arguments: serde_json::Value,
    pub result_tx: oneshot::Sender<Result<String, String>>,
}

/// Bridge between MCP HTTP server and daemon tool execution.
pub struct McpToolBridge {
    tools: Arc<RwLock<Vec<McpToolDef>>>,
    executor: Arc<Mutex<Option<mpsc::Sender<ToolCallRequest>>>>,
    mcp_server_url: OnceLock<String>,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for McpToolBridge {
    fn drop(&mut self) {
        if let Some(handle) = self.server_handle.lock().unwrap().take() {
            handle.abort();
        }
    }
}

/// RAII guard that clears the executor slot on drop.
pub struct ToolExecutorGuard {
    bridge: Arc<McpToolBridge>,
}

impl Drop for ToolExecutorGuard {
    fn drop(&mut self) {
        self.bridge.clear_executor_sync();
    }
}

impl McpToolBridge {
    pub fn new() -> Self {
        Self {
            tools: Arc::new(RwLock::new(Vec::new())),
            executor: Arc::new(Mutex::new(None)),
            mcp_server_url: OnceLock::new(),
            server_handle: Mutex::new(None),
        }
    }

    pub fn update_tools(&self, tools: Vec<McpToolDef>) {
        *self.tools.write().unwrap() = tools;
    }

    pub fn get_tools(&self) -> Vec<McpToolDef> {
        self.tools.read().unwrap().clone()
    }

    pub fn set_executor(&self, tx: mpsc::Sender<ToolCallRequest>) {
        *self.executor.lock().unwrap() = Some(tx);
    }

    pub fn clear_executor_sync(&self) {
        *self.executor.lock().unwrap() = None;
    }

    pub fn clone_executor(&self) -> Option<mpsc::Sender<ToolCallRequest>> {
        self.executor.lock().unwrap().clone()
    }

    pub fn executor_guard(self: &Arc<Self>) -> ToolExecutorGuard {
        ToolExecutorGuard {
            bridge: self.clone(),
        }
    }

    /// Set the MCP HTTP server URL (called once on first startup).
    pub fn set_mcp_server_url(&self, url: String) {
        let _ = self.mcp_server_url.set(url);
    }

    /// Get the MCP HTTP server URL, if started.
    pub fn mcp_server_url(&self) -> Option<&str> {
        self.mcp_server_url.get().map(|s| s.as_str())
    }

    /// Store the server task handle for shutdown.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        *self.server_handle.lock().unwrap() = Some(handle);
    }
}

/// Convert a tool definition to MCP JSON format.
pub fn tool_def_to_json(def: &McpToolDef) -> serde_json::Value {
    serde_json::json!({
        "name": def.name,
        "description": def.description,
        "inputSchema": def.input_schema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_and_get_tools() {
        let bridge = McpToolBridge::new();
        assert!(bridge.get_tools().is_empty());

        bridge.update_tools(vec![McpToolDef {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }]);

        let tools = bridge.get_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "test_tool");
    }

    #[test]
    fn test_set_and_clear_executor() {
        let bridge = McpToolBridge::new();
        assert!(bridge.clone_executor().is_none());

        let (tx, _rx) = mpsc::channel::<ToolCallRequest>(1);
        bridge.set_executor(tx);
        assert!(bridge.clone_executor().is_some());

        bridge.clear_executor_sync();
        assert!(bridge.clone_executor().is_none());
    }

    #[test]
    fn test_executor_guard_clears_on_drop() {
        let bridge = Arc::new(McpToolBridge::new());
        let (tx, _rx) = mpsc::channel::<ToolCallRequest>(1);
        bridge.set_executor(tx);

        {
            let _guard = bridge.executor_guard();
            assert!(bridge.clone_executor().is_some());
        }
        assert!(bridge.clone_executor().is_none());
    }

    #[tokio::test]
    async fn test_tool_call_through_channel() {
        let bridge = McpToolBridge::new();
        let (tx, mut rx) = mpsc::channel::<ToolCallRequest>(1);
        bridge.set_executor(tx);

        let sender = bridge.clone_executor().unwrap();
        let (result_tx, result_rx) = oneshot::channel();
        sender
            .send(ToolCallRequest {
                name: "test".to_string(),
                arguments: serde_json::json!({"key": "value"}),
                result_tx,
            })
            .await
            .unwrap();

        let req = rx.recv().await.unwrap();
        assert_eq!(req.name, "test");
        let _ = req.result_tx.send(Ok("result".to_string()));

        let result = result_rx.await.unwrap();
        assert_eq!(result, Ok("result".to_string()));
    }

    #[test]
    fn test_mcp_server_url() {
        let bridge = McpToolBridge::new();
        assert!(bridge.mcp_server_url().is_none());

        bridge.set_mcp_server_url("http://127.0.0.1:12345/mcp".to_string());
        assert_eq!(
            bridge.mcp_server_url(),
            Some("http://127.0.0.1:12345/mcp")
        );
    }

    #[test]
    fn test_tool_def_to_json() {
        let def = McpToolDef {
            name: "browser_navigate".to_string(),
            description: "Navigate to URL".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let json = tool_def_to_json(&def);
        assert_eq!(json["name"], "browser_navigate");
        assert_eq!(json["description"], "Navigate to URL");
    }
}
