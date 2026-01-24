//! ZeroMQ client for communicating with the daemon.
//!
//! Uses DEALER socket to connect to daemon's ROUTER.

use crate::config::BridgeConfig;
use crate::error::{BridgeError, Result};
use crate::port_discovery::{discover_daemon, DaemonInfo};
use futures::stream::Stream;
use nevoflux_protocol::{Channel, DaemonEnvelope, ProxyEnvelope};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tracing::{debug, info};
use zeromq::{Socket, SocketRecv, SocketSend, ZmqMessage};

/// Message from daemon.
pub type DaemonMessage = DaemonEnvelope;

/// Client for communicating with the daemon over ZeroMQ.
pub struct DaemonClient {
    /// The proxy ID for this client.
    proxy_id: String,
    /// The ZeroMQ socket.
    socket: zeromq::DealerSocket,
    /// Configuration.
    config: Arc<BridgeConfig>,
    /// Connected daemon info.
    daemon_info: Option<DaemonInfo>,
}

impl DaemonClient {
    /// Create a new daemon client with the given proxy ID.
    pub fn new(proxy_id: impl Into<String>, config: BridgeConfig) -> Self {
        Self {
            proxy_id: proxy_id.into(),
            socket: zeromq::DealerSocket::new(),
            config: Arc::new(config),
            daemon_info: None,
        }
    }

    /// Get the proxy ID.
    pub fn proxy_id(&self) -> &str {
        &self.proxy_id
    }

    /// Get the configuration.
    pub fn config(&self) -> &BridgeConfig {
        &self.config
    }

    /// Get the connected daemon info.
    pub fn daemon_info(&self) -> Option<&DaemonInfo> {
        self.daemon_info.as_ref()
    }

    /// Connect to the daemon.
    ///
    /// Discovers the daemon port and establishes a ZeroMQ connection.
    pub async fn connect(&mut self) -> Result<()> {
        // Discover daemon
        let info = discover_daemon(&self.config).await?;

        let addr = format!("tcp://127.0.0.1:{}", info.port);
        info!("Connecting to daemon at {}", addr);

        // Set socket identity to proxy_id
        // Note: zeromq crate doesn't support identity yet, so we'll include proxy_id in messages

        // Connect
        self.socket
            .connect(&addr)
            .await
            .map_err(|e| BridgeError::ConnectionFailed(e.to_string()))?;

        self.daemon_info = Some(info);

        debug!("Connected to daemon");
        Ok(())
    }

    /// Connect to a specific address (for testing).
    pub async fn connect_to(&mut self, addr: &str) -> Result<()> {
        info!("Connecting to {}", addr);

        self.socket
            .connect(addr)
            .await
            .map_err(|e| BridgeError::ConnectionFailed(e.to_string()))?;

        Ok(())
    }

    /// Send a message to the daemon.
    pub async fn send(&mut self, envelope: ProxyEnvelope) -> Result<()> {
        let data = serde_json::to_vec(&envelope)?;
        let msg = ZmqMessage::from(data);

        self.socket.send(msg).await.map_err(BridgeError::from)?;

        debug!("Sent message to daemon: request_id={}", envelope.request_id);
        Ok(())
    }

    /// Send a chat message to the daemon.
    pub async fn send_chat(
        &mut self,
        request_id: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<()> {
        let envelope = ProxyEnvelope::new(&self.proxy_id, request_id, Channel::Chat, payload);
        self.send(envelope).await
    }

    /// Send an MCP message to the daemon.
    pub async fn send_mcp(
        &mut self,
        request_id: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<()> {
        let envelope = ProxyEnvelope::new(&self.proxy_id, request_id, Channel::Mcp, payload);
        self.send(envelope).await
    }

    /// Receive a message from the daemon.
    pub async fn recv(&mut self) -> Result<DaemonEnvelope> {
        let msg = self.socket.recv().await.map_err(BridgeError::from)?;

        let data = msg.into_vec();
        if data.is_empty() {
            return Err(BridgeError::NativeMessaging(
                "Empty message received".into(),
            ));
        }

        let envelope: DaemonEnvelope = serde_json::from_slice(&data[0])?;
        debug!(
            "Received message from daemon: request_id={:?}",
            envelope.request_id
        );
        Ok(envelope)
    }

    /// Close the connection.
    pub async fn close(&mut self) -> Result<()> {
        // ZeroMQ sockets are closed when dropped
        debug!("Closing daemon client connection");
        Ok(())
    }
}

/// A stream of messages from the daemon.
pub struct DaemonMessageStream {
    receiver: mpsc::Receiver<Result<DaemonEnvelope>>,
}

impl DaemonMessageStream {
    /// Create a new message stream from a receiver.
    pub fn new(receiver: mpsc::Receiver<Result<DaemonEnvelope>>) -> Self {
        Self { receiver }
    }
}

impl Stream for DaemonMessageStream {
    type Item = Result<DaemonEnvelope>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_recv(cx)
    }
}

/// Create a unique proxy ID.
pub fn generate_proxy_id() -> String {
    let uuid_str = uuid::Uuid::new_v4().to_string();
    format!("proxy-{}", &uuid_str[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_proxy_id() {
        let id1 = generate_proxy_id();
        let id2 = generate_proxy_id();

        assert!(id1.starts_with("proxy-"));
        assert!(id2.starts_with("proxy-"));
        assert_ne!(id1, id2);
        assert_eq!(id1.len(), 14); // "proxy-" + 8 chars
    }

    #[test]
    fn test_daemon_client_new() {
        let config = BridgeConfig::new();
        let client = DaemonClient::new("test-proxy", config);

        assert_eq!(client.proxy_id(), "test-proxy");
        assert!(client.daemon_info().is_none());
    }

    #[test]
    fn test_daemon_client_config() {
        let config = BridgeConfig::new().with_port_range(20000, 20100);
        let client = DaemonClient::new("test-proxy", config);

        assert_eq!(client.config().port_range_start, 20000);
    }

    #[tokio::test]
    async fn test_daemon_client_connect_no_daemon() {
        let temp = tempfile::TempDir::new().unwrap();
        let config = BridgeConfig::new().with_data_dir(temp.path());
        let mut client = DaemonClient::new("test-proxy", config);

        let result = client.connect().await;
        assert!(matches!(result, Err(BridgeError::PortFileNotFound(_))));
    }

    #[test]
    fn test_proxy_envelope_creation() {
        let envelope = ProxyEnvelope::new(
            "proxy-001",
            "req-001",
            Channel::Chat,
            serde_json::json!({"type": "chat_message"}),
        );

        assert_eq!(envelope.proxy_id, "proxy-001");
        assert_eq!(envelope.request_id, "req-001");
        assert_eq!(envelope.channel, Channel::Chat);
    }

    #[test]
    fn test_proxy_envelope_serialization() {
        let envelope = ProxyEnvelope::new(
            "proxy-001",
            "req-001",
            Channel::Chat,
            serde_json::json!({"type": "chat_message", "payload": {"text": "hello"}}),
        );

        let json = serde_json::to_string(&envelope).unwrap();
        let decoded: ProxyEnvelope = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.proxy_id, "proxy-001");
        assert_eq!(decoded.request_id, "req-001");
    }

    #[test]
    fn test_daemon_envelope_deserialization() {
        let json = r#"{
            "proxy_id": "proxy-001",
            "request_id": "req-001",
            "channel": "chat",
            "payload": {"type": "stream_chunk"},
            "timestamp_ms": 1706000000000
        }"#;

        let envelope: DaemonEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.proxy_id, "proxy-001");
        assert_eq!(envelope.channel, Channel::Chat);
    }

    #[tokio::test]
    async fn test_daemon_message_stream() {
        let (tx, rx) = mpsc::channel(10);
        let mut stream = DaemonMessageStream::new(rx);

        // Send a message
        let envelope = DaemonEnvelope::new("proxy-001", Channel::Chat, serde_json::json!({}));
        tx.send(Ok(envelope.clone())).await.unwrap();
        drop(tx);

        // Receive via stream
        use futures::StreamExt;
        let received = stream.next().await.unwrap().unwrap();
        assert_eq!(received.proxy_id, "proxy-001");
    }
}
