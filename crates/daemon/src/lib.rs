//! NevoFlux Daemon - Core daemon for NevoFlux Agent
//!
//! The daemon is the central processing unit that handles:
//! - Message routing between proxies and the agent core
//! - Session management and persistence
//! - Context building for LLM requests
//! - MCP server management
//! - Permission management
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Daemon Internals                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  ProxyRegistry          RequestRegistry      SessionManager  │
//! │  ┌─────────────┐       ┌───────────────┐    ┌───────────┐   │
//! │  │ proxy_id →  │       │ request_id →  │    │ session   │   │
//! │  │ ProxyInfo   │       │ ActiveRequest │    │ (SQLite)  │   │
//! │  └─────────────┘       └───────────────┘    └───────────┘   │
//! └─────────────────────────────────────────────────────────────┘
//! ```

pub mod config;
pub mod context;
pub mod error;
pub mod permission;
pub mod registry;
pub mod router;
pub mod server;
pub mod session;
pub mod wasm;

pub use config::DaemonConfig;
pub use error::{DaemonError, Result};
pub use permission::{Action, PermissionEnforcer, PermissionResult, ResourceType};
pub use registry::{ActiveRequest, ProxyInfo, ProxyRegistry, RequestRegistry};
pub use router::Router;
pub use server::{find_available_port, start_server, Server, ServerConfig};
pub use session::SessionManager;
pub use wasm::{create_linker, HostState, WasmConfig, WasmInstance, WasmRuntime};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_registry_registers_proxy() {
        // RED: ProxyRegistry should track connected proxies
        let registry = ProxyRegistry::new();
        registry.register("proxy-001", 12345);

        assert!(registry.is_registered("proxy-001"));
        assert!(!registry.is_registered("proxy-002"));
    }

    #[test]
    fn test_request_registry_tracks_active_requests() {
        // RED: RequestRegistry should track active requests
        let registry = RequestRegistry::new();
        registry.register("req-001", "proxy-001", "session-001");

        let request = registry.get("req-001");
        assert!(request.is_some());
        assert_eq!(request.unwrap().proxy_id, "proxy-001");
    }

    #[test]
    fn test_router_routes_to_correct_proxy() {
        // RED: Router should route messages to the correct proxy
        let router = Router::new();

        // Register a proxy and request
        router.proxy_registry().register("proxy-001", 12345);
        router
            .request_registry()
            .register("req-001", "proxy-001", "session-001");

        // Should find the proxy for a response
        let proxy_id = router.find_proxy_for_request("req-001");
        assert_eq!(proxy_id, Some("proxy-001".to_string()));
    }

    #[tokio::test]
    async fn test_session_manager_creates_session() {
        // RED: SessionManager should create and retrieve sessions
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        assert!(!session.id.is_empty());

        let retrieved = manager.get_session(&session.id).await.unwrap();
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_daemon_config_defaults() {
        // RED: DaemonConfig should have sensible defaults
        let config = DaemonConfig::default();

        assert!(config.idle_timeout_secs > 0);
        assert!(config.heartbeat_timeout_secs > 0);
    }
}
