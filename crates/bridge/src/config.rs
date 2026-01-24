//! Configuration types for the bridge.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Bridge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// ZeroMQ port range start.
    pub port_range_start: u16,
    /// ZeroMQ port range end.
    pub port_range_end: u16,
    /// Connection timeout.
    #[serde(with = "duration_serde")]
    pub connect_timeout: Duration,
    /// Heartbeat interval.
    #[serde(with = "duration_serde")]
    pub heartbeat_interval: Duration,
    /// Whether to auto-launch daemon if not running.
    pub auto_launch_daemon: bool,
    /// Data directory for port files, etc.
    pub data_dir: Option<PathBuf>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            port_range_start: 19500,
            port_range_end: 19600,
            connect_timeout: Duration::from_secs(10),
            heartbeat_interval: Duration::from_secs(10),
            auto_launch_daemon: true,
            data_dir: None,
        }
    }
}

impl BridgeConfig {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the port range.
    pub fn with_port_range(mut self, start: u16, end: u16) -> Self {
        self.port_range_start = start;
        self.port_range_end = end;
        self
    }

    /// Set the connection timeout.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set the heartbeat interval.
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// Set auto-launch daemon.
    pub fn with_auto_launch(mut self, auto_launch: bool) -> Self {
        self.auto_launch_daemon = auto_launch;
        self
    }

    /// Set the data directory.
    pub fn with_data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// Get the data directory, using platform default if not set.
    pub fn data_directory(&self) -> PathBuf {
        if let Some(ref dir) = self.data_dir {
            dir.clone()
        } else {
            default_data_dir()
        }
    }

    /// Get the port file path.
    pub fn port_file_path(&self) -> PathBuf {
        self.data_directory().join("daemon.port")
    }

    /// Get the PID file path.
    pub fn pid_file_path(&self) -> PathBuf {
        self.data_directory().join("daemon.pid")
    }

    /// Get the lock file path.
    pub fn lock_file_path(&self) -> PathBuf {
        self.data_directory().join("daemon.lock")
    }
}

/// Get the default data directory for the current platform.
pub fn default_data_dir() -> PathBuf {
    if let Some(data_dir) = directories::ProjectDirs::from("com", "nevoflux", "nevoflux") {
        data_dir.data_dir().to_path_buf()
    } else {
        // Fallback
        PathBuf::from("~/.local/share/nevoflux")
    }
}

/// Serde helpers for Duration.
mod duration_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration.as_millis().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = BridgeConfig::default();
        assert_eq!(config.port_range_start, 19500);
        assert_eq!(config.port_range_end, 19600);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.heartbeat_interval, Duration::from_secs(10));
        assert!(config.auto_launch_daemon);
        assert!(config.data_dir.is_none());
    }

    #[test]
    fn test_config_builder() {
        let config = BridgeConfig::new()
            .with_port_range(20000, 20100)
            .with_connect_timeout(Duration::from_secs(5))
            .with_heartbeat_interval(Duration::from_secs(30))
            .with_auto_launch(false)
            .with_data_dir("/tmp/nevoflux");

        assert_eq!(config.port_range_start, 20000);
        assert_eq!(config.port_range_end, 20100);
        assert_eq!(config.connect_timeout, Duration::from_secs(5));
        assert_eq!(config.heartbeat_interval, Duration::from_secs(30));
        assert!(!config.auto_launch_daemon);
        assert_eq!(config.data_dir, Some(PathBuf::from("/tmp/nevoflux")));
    }

    #[test]
    fn test_config_data_directory_custom() {
        let config = BridgeConfig::new().with_data_dir("/custom/path");
        assert_eq!(config.data_directory(), PathBuf::from("/custom/path"));
    }

    #[test]
    fn test_config_file_paths() {
        let config = BridgeConfig::new().with_data_dir("/test/dir");

        assert_eq!(
            config.port_file_path(),
            PathBuf::from("/test/dir/daemon.port")
        );
        assert_eq!(
            config.pid_file_path(),
            PathBuf::from("/test/dir/daemon.pid")
        );
        assert_eq!(
            config.lock_file_path(),
            PathBuf::from("/test/dir/daemon.lock")
        );
    }

    #[test]
    fn test_config_json_serialization() {
        let config = BridgeConfig::new()
            .with_port_range(19500, 19600)
            .with_connect_timeout(Duration::from_millis(5000));

        let json = serde_json::to_string(&config).unwrap();
        let decoded: BridgeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.port_range_start, 19500);
        assert_eq!(decoded.connect_timeout, Duration::from_millis(5000));
    }

    #[test]
    fn test_default_data_dir_not_empty() {
        let dir = default_data_dir();
        assert!(!dir.as_os_str().is_empty());
    }
}
