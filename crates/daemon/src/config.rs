//! Configuration file support for NevoFlux Agent.
//!
//! This module provides TOML-based configuration loading and saving
//! from the standard config directory (~/.config/nevoflux/config.toml).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during configuration operations.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to read configuration file.
    #[error("failed to read configuration file: {0}")]
    ReadError(#[from] std::io::Error),

    /// Failed to parse configuration file.
    #[error("failed to parse configuration file: {0}")]
    ParseError(#[from] toml::de::Error),

    /// Failed to serialize configuration.
    #[error("failed to serialize configuration: {0}")]
    SerializeError(#[from] toml::ser::Error),

    /// No config directory found.
    #[error("could not determine config directory")]
    NoConfigDir,
}

/// Top-level agent configuration.
///
/// This is the root configuration structure that contains all subsystem
/// configurations. It can be loaded from ~/.config/nevoflux/config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// Daemon-specific configuration.
    #[serde(default)]
    pub daemon: DaemonConfig,

    /// LLM provider configuration.
    #[serde(default)]
    pub llm: LlmConfig,

    /// Storage configuration.
    #[serde(default)]
    pub storage: StorageConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl AgentConfig {
    /// Returns the default configuration file path.
    ///
    /// This is typically ~/.config/nevoflux/config.toml on Linux/macOS
    /// or %APPDATA%\nevoflux\config.toml on Windows.
    pub fn default_config_path() -> Result<PathBuf, ConfigError> {
        let config_dir = dirs::config_dir().ok_or(ConfigError::NoConfigDir)?;
        Ok(config_dir.join("nevoflux").join("config.toml"))
    }

    /// Load configuration from the default path.
    ///
    /// Returns default configuration if the file doesn't exist.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::default_config_path()?;
        Self::load_from_path(&path)
    }

    /// Load configuration from a specific path.
    ///
    /// Returns default configuration if the file doesn't exist.
    pub fn load_from_path(path: &PathBuf) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        let config: AgentConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// Save configuration to the default path.
    ///
    /// Creates parent directories if they don't exist.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::default_config_path()?;
        self.save_to_path(&path)
    }

    /// Save configuration to a specific path.
    ///
    /// Creates parent directories if they don't exist.
    pub fn save_to_path(&self, path: &PathBuf) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Merge with another configuration, preferring non-default values from other.
    pub fn merge(&mut self, other: &AgentConfig) {
        // Merge daemon config
        if other.daemon.port_range_start != DaemonConfig::default().port_range_start {
            self.daemon.port_range_start = other.daemon.port_range_start;
        }
        if other.daemon.port_range_end != DaemonConfig::default().port_range_end {
            self.daemon.port_range_end = other.daemon.port_range_end;
        }
        if other.daemon.idle_timeout_secs != DaemonConfig::default().idle_timeout_secs {
            self.daemon.idle_timeout_secs = other.daemon.idle_timeout_secs;
        }

        // Merge LLM config
        if other.llm.default_provider.is_some() {
            self.llm.default_provider = other.llm.default_provider.clone();
        }
        if other.llm.default_model.is_some() {
            self.llm.default_model = other.llm.default_model.clone();
        }
        if other.llm.max_tokens != LlmConfig::default().max_tokens {
            self.llm.max_tokens = other.llm.max_tokens;
        }

        // Merge storage config
        if other.storage.data_dir.is_some() {
            self.storage.data_dir = other.storage.data_dir.clone();
        }
        if other.storage.max_size_mb != StorageConfig::default().max_size_mb {
            self.storage.max_size_mb = other.storage.max_size_mb;
        }

        // Merge logging config
        if other.logging.level != LoggingConfig::default().level {
            self.logging.level = other.logging.level.clone();
        }
        if other.logging.file.is_some() {
            self.logging.file = other.logging.file.clone();
        }
    }
}

/// LLM provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Default LLM provider (e.g., "anthropic", "openai", "qwen").
    #[serde(default)]
    pub default_provider: Option<String>,

    /// Default model name.
    #[serde(default)]
    pub default_model: Option<String>,

    /// Maximum tokens for responses.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// Temperature for generation.
    #[serde(default = "default_temperature")]
    pub temperature: f32,

    /// Request timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    /// Maximum retries for failed requests.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            default_provider: None,
            default_model: None,
            max_tokens: default_max_tokens(),
            temperature: default_temperature(),
            timeout_secs: default_timeout_secs(),
            max_retries: default_max_retries(),
        }
    }
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_temperature() -> f32 {
    0.7
}

fn default_timeout_secs() -> u64 {
    120
}

fn default_max_retries() -> u32 {
    3
}

/// Storage configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Custom data directory path.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,

    /// Maximum storage size in MB.
    #[serde(default = "default_max_size_mb")]
    pub max_size_mb: u64,

    /// Whether to enable WAL mode for SQLite.
    #[serde(default = "default_true")]
    pub wal_mode: bool,

    /// Whether to vacuum database on startup.
    #[serde(default)]
    pub vacuum_on_startup: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            max_size_mb: default_max_size_mb(),
            wal_mode: default_true(),
            vacuum_on_startup: false,
        }
    }
}

fn default_max_size_mb() -> u64 {
    1024
}

fn default_true() -> bool {
    true
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error).
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Optional log file path.
    #[serde(default)]
    pub file: Option<PathBuf>,

    /// Whether to log to stdout.
    #[serde(default = "default_true")]
    pub stdout: bool,

    /// Whether to use JSON format.
    #[serde(default)]
    pub json_format: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
            stdout: true,
            json_format: false,
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

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
    use std::io::Write;

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

    // New tests for AgentConfig and file operations

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();

        // Check daemon defaults are applied
        assert_eq!(config.daemon.port_range_start, 19500);
        assert_eq!(config.daemon.idle_timeout_secs, 1800);

        // Check LLM defaults
        assert_eq!(config.llm.max_tokens, 4096);
        assert_eq!(config.llm.temperature, 0.7);
        assert!(config.llm.default_provider.is_none());

        // Check storage defaults
        assert_eq!(config.storage.max_size_mb, 1024);
        assert!(config.storage.wal_mode);

        // Check logging defaults
        assert_eq!(config.logging.level, "info");
        assert!(config.logging.stdout);
    }

    #[test]
    fn test_config_load_from_nonexistent_returns_default() {
        let path = PathBuf::from("/nonexistent/path/config.toml");
        let config = AgentConfig::load_from_path(&path).unwrap();

        assert_eq!(config.daemon.port_range_start, 19500);
        assert_eq!(config.llm.max_tokens, 4096);
    }

    #[test]
    fn test_config_save_and_load() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        // Create a config with custom values
        let mut config = AgentConfig::default();
        config.daemon.port_range_start = 20000;
        config.daemon.idle_timeout_secs = 3600;
        config.llm.default_provider = Some("anthropic".to_string());
        config.llm.default_model = Some("claude-3".to_string());
        config.llm.max_tokens = 8192;
        config.storage.data_dir = Some(PathBuf::from("/custom/data"));
        config.logging.level = "debug".to_string();

        // Save the config
        config.save_to_path(&config_path).unwrap();

        // Verify file exists
        assert!(config_path.exists());

        // Load it back
        let loaded = AgentConfig::load_from_path(&config_path).unwrap();

        assert_eq!(loaded.daemon.port_range_start, 20000);
        assert_eq!(loaded.daemon.idle_timeout_secs, 3600);
        assert_eq!(loaded.llm.default_provider, Some("anthropic".to_string()));
        assert_eq!(loaded.llm.default_model, Some("claude-3".to_string()));
        assert_eq!(loaded.llm.max_tokens, 8192);
        assert_eq!(loaded.storage.data_dir, Some(PathBuf::from("/custom/data")));
        assert_eq!(loaded.logging.level, "debug");
    }

    #[test]
    fn test_config_toml_format() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let mut config = AgentConfig::default();
        config.llm.default_provider = Some("openai".to_string());
        config.save_to_path(&config_path).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();

        // Verify TOML structure
        assert!(content.contains("[daemon]"));
        assert!(content.contains("[llm]"));
        assert!(content.contains("[storage]"));
        assert!(content.contains("[logging]"));
        assert!(content.contains("default_provider = \"openai\""));
    }

    #[test]
    fn test_config_partial_toml() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        // Write a partial config (only LLM section)
        let partial_config = r#"
[llm]
default_provider = "qwen"
max_tokens = 2048

[logging]
level = "warn"
"#;
        std::fs::write(&config_path, partial_config).unwrap();

        // Load it - should use defaults for missing sections
        let config = AgentConfig::load_from_path(&config_path).unwrap();

        // Custom values should be loaded
        assert_eq!(config.llm.default_provider, Some("qwen".to_string()));
        assert_eq!(config.llm.max_tokens, 2048);
        assert_eq!(config.logging.level, "warn");

        // Default values should be applied for missing fields
        assert_eq!(config.daemon.port_range_start, 19500);
        assert_eq!(config.storage.max_size_mb, 1024);
    }

    #[test]
    fn test_config_creates_parent_directories() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir
            .path()
            .join("nested")
            .join("dirs")
            .join("config.toml");

        let config = AgentConfig::default();
        config.save_to_path(&config_path).unwrap();

        assert!(config_path.exists());
    }

    #[test]
    fn test_config_merge() {
        let mut base = AgentConfig::default();
        let mut other = AgentConfig::default();

        // Set some non-default values in other
        other.daemon.port_range_start = 21000;
        other.llm.default_provider = Some("anthropic".to_string());
        other.storage.data_dir = Some(PathBuf::from("/merged/path"));
        other.logging.level = "trace".to_string();

        base.merge(&other);

        // Merged values should be applied
        assert_eq!(base.daemon.port_range_start, 21000);
        assert_eq!(base.llm.default_provider, Some("anthropic".to_string()));
        assert_eq!(base.storage.data_dir, Some(PathBuf::from("/merged/path")));
        assert_eq!(base.logging.level, "trace");

        // Values that weren't changed should keep their defaults
        assert_eq!(base.daemon.idle_timeout_secs, 1800);
        assert_eq!(base.llm.max_tokens, 4096);
    }

    #[test]
    fn test_llm_config_defaults() {
        let config = LlmConfig::default();

        assert!(config.default_provider.is_none());
        assert!(config.default_model.is_none());
        assert_eq!(config.max_tokens, 4096);
        assert_eq!(config.temperature, 0.7);
        assert_eq!(config.timeout_secs, 120);
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn test_storage_config_defaults() {
        let config = StorageConfig::default();

        assert!(config.data_dir.is_none());
        assert_eq!(config.max_size_mb, 1024);
        assert!(config.wal_mode);
        assert!(!config.vacuum_on_startup);
    }

    #[test]
    fn test_logging_config_defaults() {
        let config = LoggingConfig::default();

        assert_eq!(config.level, "info");
        assert!(config.file.is_none());
        assert!(config.stdout);
        assert!(!config.json_format);
    }

    #[test]
    fn test_default_config_path() {
        // This test just verifies the path logic works
        let result = AgentConfig::default_config_path();

        // On most systems this should succeed
        if let Ok(path) = result {
            assert!(path.ends_with("config.toml"));
            assert!(path.to_string_lossy().contains("nevoflux"));
        }
    }

    #[test]
    fn test_config_invalid_toml_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        // Write invalid TOML
        let mut file = std::fs::File::create(&config_path).unwrap();
        file.write_all(b"this is not valid toml {{{{").unwrap();

        let result = AgentConfig::load_from_path(&config_path);
        assert!(result.is_err());

        match result {
            Err(ConfigError::ParseError(_)) => (),
            _ => panic!("Expected ParseError"),
        }
    }
}
