//! Daemon configuration.

use serde::{Deserialize, Serialize};

/// Configuration for the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Port range start for daemon server.
    pub port_range_start: u16,
    /// Port range end for daemon server.
    pub port_range_end: u16,
    /// Idle timeout in seconds before daemon shuts down.
    pub idle_timeout_secs: u64,
    /// Heartbeat timeout in seconds for proxy connections.
    pub heartbeat_timeout_secs: u64,
    /// Heartbeat interval in seconds.
    pub heartbeat_interval_secs: u64,
    /// Maximum number of concurrent requests.
    pub max_concurrent_requests: usize,
    /// Whether to keep alive for MCP connections.
    pub keep_alive_for_mcp: bool,
    /// Session configuration.
    pub session: SessionConfig,
    /// Context configuration.
    pub context: ContextConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            port_range_start: 19500,
            port_range_end: 19600,
            idle_timeout_secs: 1800, // 30 minutes
            heartbeat_timeout_secs: 30,
            heartbeat_interval_secs: 10,
            max_concurrent_requests: 100,
            keep_alive_for_mcp: true,
            session: SessionConfig::default(),
            context: ContextConfig::default(),
        }
    }
}

impl DaemonConfig {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the idle timeout.
    pub fn with_idle_timeout(mut self, secs: u64) -> Self {
        self.idle_timeout_secs = secs;
        self
    }

    /// Set the heartbeat timeout.
    pub fn with_heartbeat_timeout(mut self, secs: u64) -> Self {
        self.heartbeat_timeout_secs = secs;
        self
    }

    /// Set keep alive for MCP.
    pub fn with_keep_alive_for_mcp(mut self, keep_alive: bool) -> Self {
        self.keep_alive_for_mcp = keep_alive;
        self
    }
}

/// Session management configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Maximum number of sessions to keep.
    pub max_sessions: u32,
    /// Days after which inactive sessions are cleaned up.
    pub inactive_days: u32,
    /// Maximum storage size in MB.
    pub max_storage_mb: u32,
    /// Whether to auto-create sessions.
    pub auto_create: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_sessions: 500,
            inactive_days: 90,
            max_storage_mb: 500,
            auto_create: true,
        }
    }
}

/// Context building configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Reserved tokens for system prompt.
    pub system_prompt_reserve: u32,
    /// Safety margin tokens.
    pub safety_margin: u32,
    /// Maximum history messages to include.
    pub max_history_messages: u32,
    /// Whether to include memory in context.
    pub include_memory: bool,
    /// Whether to include current page info.
    pub include_current_page: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            system_prompt_reserve: 2000,
            safety_margin: 500,
            max_history_messages: 50,
            include_memory: true,
            include_current_page: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_config_default() {
        let config = DaemonConfig::default();

        assert_eq!(config.port_range_start, 19500);
        assert_eq!(config.port_range_end, 19600);
        assert_eq!(config.idle_timeout_secs, 1800);
        assert_eq!(config.heartbeat_timeout_secs, 30);
    }

    #[test]
    fn test_daemon_config_builder() {
        let config = DaemonConfig::new()
            .with_idle_timeout(3600)
            .with_heartbeat_timeout(60)
            .with_keep_alive_for_mcp(false);

        assert_eq!(config.idle_timeout_secs, 3600);
        assert_eq!(config.heartbeat_timeout_secs, 60);
        assert!(!config.keep_alive_for_mcp);
    }

    #[test]
    fn test_session_config_default() {
        let config = SessionConfig::default();

        assert_eq!(config.max_sessions, 500);
        assert_eq!(config.inactive_days, 90);
        assert!(config.auto_create);
    }

    #[test]
    fn test_context_config_default() {
        let config = ContextConfig::default();

        assert_eq!(config.system_prompt_reserve, 2000);
        assert!(config.include_memory);
    }

    #[test]
    fn test_config_serialization() {
        let config = DaemonConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: DaemonConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.idle_timeout_secs, decoded.idle_timeout_secs);
    }
}
