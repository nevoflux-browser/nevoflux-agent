//! MCP-over-ACP bridge for native tool calling.
//!
//! Provides the bridge between sacp's MCP server and NevoFlux tool execution.
//! Used when `AcpProviderConfig::use_mcp_bridge` is true (Claude Code).

use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{mpsc, oneshot};

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

/// Bridge between sacp MCP server and daemon tool execution.
///
/// Two layers:
/// - `tools`: static tool definitions, updated per-request
/// - `executor`: dynamic per-request channel to daemon executor
pub struct McpToolBridge {
    tools: Arc<RwLock<Vec<McpToolDef>>>,
    executor: Arc<Mutex<Option<mpsc::Sender<ToolCallRequest>>>>,
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
        // Guard dropped — executor should be cleared
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
}
