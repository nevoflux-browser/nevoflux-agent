//! ZeroMQ ROUTER server for the daemon.

use crate::error::{DaemonError, Result};
use crate::router::Router;
use nevoflux_protocol::ProxyEnvelope;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use zeromq::Socket;

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

/// Start the ZeroMQ server.
pub async fn start_server(config: ServerConfig, router: Arc<Router>) -> Result<Server> {
    let port = find_available_port(&config).await?;
    let addr = format!("tcp://{}:{}", config.bind_address, port);

    info!("Starting daemon server on {}", addr);

    let mut socket = zeromq::RouterSocket::new();
    socket
        .bind(&addr)
        .await
        .map_err(|e| DaemonError::InternalError(format!("Failed to bind: {}", e)))?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    let (msg_tx, mut _msg_rx) = mpsc::channel::<(Vec<u8>, ProxyEnvelope)>(100);

    // Spawn receive loop
    let recv_socket = socket;
    let _router = router;
    tokio::spawn(async move {
        let mut socket = recv_socket;
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("Server shutdown signal received");
                    break;
                }
                msg = zeromq::SocketRecv::recv(&mut socket) => {
                    match msg {
                        Ok(zmq_msg) => {
                            let frames = zmq_msg.into_vec();
                            if frames.len() >= 2 {
                                let identity = frames[0].to_vec();
                                if let Ok(envelope) = serde_json::from_slice::<ProxyEnvelope>(&frames[1]) {
                                    debug!("Received message from {}", envelope.proxy_id);
                                    let _ = msg_tx.send((identity, envelope)).await;
                                }
                            }
                        }
                        Err(e) => {
                            error!("Receive error: {}", e);
                        }
                    }
                }
            }
        }
    });

    Ok(Server {
        port,
        shutdown_tx: Some(shutdown_tx),
    })
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

    #[tokio::test]
    async fn test_server_start_and_shutdown() {
        let config = ServerConfig::default();
        let router = Arc::new(Router::new());

        let server = start_server(config, router).await;
        assert!(server.is_ok());

        let mut server = server.unwrap();
        assert!(server.port() >= 19500);

        // Shutdown
        server.shutdown().await;
    }
}
