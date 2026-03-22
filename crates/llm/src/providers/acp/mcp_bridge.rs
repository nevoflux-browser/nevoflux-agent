//! MCP-over-ACP bridge for native tool calling.
//!
//! Provides the bridge between sacp's MCP server and NevoFlux tool execution.
//! Used when `AcpProviderConfig::use_mcp_bridge` is true (Claude Code).

use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{mpsc, oneshot};

use sacp::mcp::McpServerToClient;
use sacp::mcp_server::{McpContext, McpServerConnect};
use sacp::{ByteStreams, Component, DynComponent, JrLink};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

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

/// Convert a McpToolDef to an rmcp Tool model.
pub fn tool_def_to_rmcp_tool(def: &McpToolDef) -> rmcp::model::Tool {
    rmcp::model::Tool {
        name: def.name.clone().into(),
        description: Some(def.description.clone().into()),
        input_schema: serde_json::from_value(def.input_schema.clone()).unwrap_or_default(),
        output_schema: None,
        annotations: None,
        icons: None,
        meta: None,
        title: None,
    }
}

/// MCP server that bridges to NevoFlux tool execution.
pub(crate) struct NevoFluxMcpServer {
    pub tool_bridge: Arc<McpToolBridge>,
}

impl<Link: JrLink> McpServerConnect<Link> for NevoFluxMcpServer {
    fn name(&self) -> String {
        "nevoflux-tools".to_string()
    }

    fn connect(&self, _cx: McpContext<Link>) -> DynComponent<McpServerToClient> {
        DynComponent::new(NevoFluxMcpHandler {
            tool_bridge: self.tool_bridge.clone(),
        })
    }
}

/// rmcp ServerHandler that dispatches tool calls through McpToolBridge.
pub(crate) struct NevoFluxMcpHandler {
    tool_bridge: Arc<McpToolBridge>,
}

impl Component<McpServerToClient> for NevoFluxMcpHandler {
    async fn serve(
        self,
        client: impl Component<sacp::mcp::McpClientToServer>,
    ) -> Result<(), sacp::Error> {
        // Duplex stream pattern from sacp::mcp_server::builder.rs:359-390
        let (mcp_server_stream, mcp_client_stream) = tokio::io::duplex(8192);
        let (mcp_server_read, mcp_server_write) = tokio::io::split(mcp_server_stream);
        let (mcp_client_read, mcp_client_write) = tokio::io::split(mcp_client_stream);

        let byte_streams =
            ByteStreams::new(mcp_client_write.compat_write(), mcp_client_read.compat());

        tokio::spawn(async move {
            let _ = Component::<McpServerToClient>::serve(byte_streams, client).await;
        });

        let running_server =
            rmcp::ServiceExt::serve(self, (mcp_server_read, mcp_server_write))
                .await
                .map_err(sacp::Error::into_internal_error)?;

        running_server
            .waiting()
            .await
            .map(|_| ())
            .map_err(sacp::Error::into_internal_error)
    }
}

impl rmcp::ServerHandler for NevoFluxMcpHandler {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo {
            protocol_version: rmcp::model::ProtocolVersion::default(),
            capabilities: rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: rmcp::model::Implementation {
                name: "nevoflux-tools".to_string(),
                version: "1.0.0".to_string(),
                title: None,
                icons: None,
                website_url: None,
            },
            instructions: Some("NevoFlux browser and computer control tools".to_string()),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParam>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let tools: Vec<rmcp::model::Tool> = self
            .tool_bridge
            .get_tools()
            .iter()
            .map(tool_def_to_rmcp_tool)
            .collect();
        Ok(rmcp::model::ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParam,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let tx = self.tool_bridge.clone_executor().ok_or_else(|| {
            rmcp::ErrorData::internal_error("no active tool executor", None)
        })?;

        let (result_tx, result_rx) = oneshot::channel();
        tx.send(ToolCallRequest {
            name: request.name.to_string(),
            arguments: serde_json::to_value(request.arguments).unwrap_or_default(),
            result_tx,
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        match result_rx.await {
            Ok(Ok(text)) => Ok(rmcp::model::CallToolResult::success(vec![
                rmcp::model::Content::text(text),
            ])),
            Ok(Err(e)) => Ok(rmcp::model::CallToolResult::error(vec![
                rmcp::model::Content::text(e),
            ])),
            Err(_) => Err(rmcp::ErrorData::internal_error(
                "tool executor dropped",
                None,
            )),
        }
    }
}

/// Build an MCP server from a tool bridge.
pub(crate) fn build_mcp_server(
    bridge: &Arc<McpToolBridge>,
) -> sacp::mcp_server::McpServer<sacp::ClientToAgent, sacp::NullResponder> {
    sacp::mcp_server::McpServer::new(
        NevoFluxMcpServer {
            tool_bridge: bridge.clone(),
        },
        sacp::NullResponder,
    )
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

    #[test]
    fn test_tool_def_to_rmcp_tool() {
        let def = McpToolDef {
            name: "browser_navigate".to_string(),
            description: "Navigate browser to URL".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"}
                },
                "required": ["url"]
            }),
        };
        let tool = tool_def_to_rmcp_tool(&def);
        assert_eq!(&*tool.name, "browser_navigate");
        assert_eq!(tool.description.as_deref(), Some("Navigate browser to URL"));
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
