//! ZeroMQ ROUTER server for the daemon.

use crate::error::{DaemonError, Result};
use tokio::sync::mpsc;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Port range start.
    pub port_start: u16,
    /// Port range end.
    pub port_end: u16,
    /// Bind address (default: 127.0.0.1).
    pub bind_address: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port_start: 19500,
            port_end: 19600,
            bind_address: "127.0.0.1".into(),
        }
    }
}

/// The ZeroMQ server handle.
pub struct Server {
    /// The bound port.
    port: u16,
    /// Shutdown signal sender.
    shutdown_tx: Option<mpsc::Sender<()>>,
}

impl Server {
    /// Get the bound port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Signal the server to shutdown.
    pub async fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
    }
}

/// Find an available port in the range.
pub async fn find_available_port(config: &ServerConfig) -> Result<u16> {
    use std::net::TcpListener;

    for port in config.port_start..=config.port_end {
        if TcpListener::bind((&*config.bind_address, port)).is_ok() {
            return Ok(port);
        }
    }

    Err(DaemonError::PortExhausted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.port_start, 19500);
        assert_eq!(config.port_end, 19600);
        assert_eq!(config.bind_address, "127.0.0.1");
    }

    #[tokio::test]
    async fn test_find_available_port() {
        let config = ServerConfig::default();
        let port = find_available_port(&config).await;
        assert!(port.is_ok());
        let port = port.unwrap();
        assert!(port >= 19500 && port <= 19600);
    }
}
