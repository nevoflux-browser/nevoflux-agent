//! The MCP server an agent channel talks to (design §5, §6).
//!
//! Transport-agnostic: [`super::mcp_gateway`] feeds it a (Sink, Stream) pair
//! per connection. It answers `tools/list` and `tools/call` through the
//! whitelist and leaves everything else to rmcp.

use std::collections::BTreeMap;
use std::sync::Arc;

use nevoflux_mcp::rmcp::handler::server::ServerHandler;
use nevoflux_mcp::rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use nevoflux_mcp::rmcp::service::RequestContext;
use nevoflux_mcp::rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{json, Value};

use super::mcp_tools::{error_with_code, is_allowed, AgentToolBackend};

/// The agent-channel protocol version. Same number as
/// `crates/daemon/tests/fixtures/muse/PROTOCOL_VERSION` (pinned by a test).
pub const PROTOCOL: u64 = 1;
pub const SERVER_NAME: &str = "nevoflux-head";

pub struct AgentMcpServer {
    backend: Arc<dyn AgentToolBackend>,
}

impl AgentMcpServer {
    pub fn new(backend: Arc<dyn AgentToolBackend>) -> Self {
        Self { backend }
    }
}

impl ServerHandler for AgentMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut nevoflux = serde_json::Map::new();
        nevoflux.insert("protocol".into(), json!(PROTOCOL));
        let mut experimental = BTreeMap::new();
        experimental.insert("nevoflux".to_string(), nevoflux);
        let capabilities = ServerCapabilities::builder()
            .enable_experimental_with(experimental)
            .enable_tools()
            .build();
        ServerInfo::new(capabilities)
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .backend
            .tools()
            .into_iter()
            .filter(|t| is_allowed(&t.name))
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if !is_allowed(&request.name) {
            tracing::info!(target: "remote", tool = %request.name, "agent asked for a tool off the whitelist");
            return Err(error_with_code(
                "not_allowed",
                format!("{} is not available over the agent channel", request.name),
            ));
        }
        let arguments = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        let result = self.backend.call(&request.name, arguments).await?;
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::mcp_tools::StubBrowserBackend;
    use nevoflux_mcp::rmcp::handler::server::ServerHandler;

    #[test]
    fn initialize_reports_identity_tools_and_the_protocol_version() {
        let info = AgentMcpServer::new(Arc::new(StubBrowserBackend)).get_info();
        assert_eq!(info.server_info.name, SERVER_NAME);
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.capabilities.tools.is_some());
        let experimental = info.capabilities.experimental.expect("experimental block");
        assert_eq!(
            experimental["nevoflux"]["protocol"],
            serde_json::json!(PROTOCOL)
        );
    }
}
