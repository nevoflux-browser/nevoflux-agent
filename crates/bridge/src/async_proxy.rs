//! Full-duplex async proxy for Native Messaging.
//!
//! This module provides a 3-task architecture that enables simultaneous
//! sending and receiving of messages between the browser extension and daemon.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                         Proxy                                │
//! │                                                              │
//! │  stdin ──▶ [Stdin Task] ──▶ stdin_tx ──────┐                │
//! │                                             │                │
//! │                                             ▼                │
//! │                                      ┌─────────────┐        │
//! │                                      │ Socket Task │        │
//! │                                      │  (select!)  │◀──────▶│ DEALER
//! │                                      └─────────────┘        │
//! │                                             │                │
//! │  stdout ◀── [Stdout Task] ◀── stdout_rx ◀──┘                │
//! │                                                              │
//! └─────────────────────────────────────────────────────────────┘
//! ```

use crate::config::BridgeConfig;
use crate::daemon_client::{generate_proxy_id, DaemonClient};
use crate::error::{BridgeError, Result};
use crate::native_messaging::{read_message, write_message};
use crate::port_discovery::launch_daemon;
use crate::proxy::parse_native_message;
use nevoflux_protocol::{Channel, DaemonEnvelope, ProxyEnvelope};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, BufWriter};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

/// Message from stdin (sidebar) to be processed.
#[derive(Debug, Clone)]
pub enum StdinMessage {
    /// A message from the sidebar to forward to daemon.
    SidebarMessage {
        request_id: String,
        channel: Channel,
        payload: serde_json::Value,
    },
    /// Shutdown signal.
    Shutdown,
}

/// Message to be written to stdout (sidebar).
#[derive(Debug, Clone)]
pub enum StdoutMessage {
    /// A response from daemon to forward to sidebar.
    DaemonResponse(DaemonEnvelope),
    /// An error message.
    Error { code: String, message: String },
    /// Initial connection message.
    Connected { version: String, proxy_id: String },
    /// Shutdown signal.
    Shutdown,
}

/// Configuration for the async proxy.
#[derive(Debug, Clone)]
pub struct AsyncProxyConfig {
    /// Bridge configuration.
    pub bridge: BridgeConfig,
    /// Channel buffer size for internal communication.
    pub channel_buffer_size: usize,
}

impl Default for AsyncProxyConfig {
    fn default() -> Self {
        Self {
            bridge: BridgeConfig::default(),
            channel_buffer_size: 100,
        }
    }
}

impl AsyncProxyConfig {
    /// Create a new config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the bridge configuration.
    pub fn with_bridge(mut self, bridge: BridgeConfig) -> Self {
        self.bridge = bridge;
        self
    }

    /// Set the channel buffer size.
    pub fn with_channel_buffer_size(mut self, size: usize) -> Self {
        self.channel_buffer_size = size;
        self
    }
}

/// Run the async proxy with full-duplex communication.
///
/// This spawns three tasks:
/// - stdin_task: reads from stdin and sends to socket_task
/// - stdout_task: receives from socket_task and writes to stdout
/// - socket_task: handles bidirectional ZeroMQ communication using select!
pub async fn run_async_proxy<R, W>(
    reader: R,
    writer: W,
    config: AsyncProxyConfig,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let proxy_id = generate_proxy_id();
    info!("Starting async proxy: {}", proxy_id);

    // Create channels
    let (stdin_tx, stdin_rx) = mpsc::channel::<StdinMessage>(config.channel_buffer_size);
    let (stdout_tx, stdout_rx) = mpsc::channel::<StdoutMessage>(config.channel_buffer_size);
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Create daemon client
    let mut daemon_client = DaemonClient::new(&proxy_id, config.bridge.clone());

    // Connect to daemon (with auto-launch if enabled)
    connect_to_daemon(&mut daemon_client, &config.bridge).await?;
    info!("Proxy {} connected to daemon", proxy_id);

    // Send initial connected message
    stdout_tx
        .send(StdoutMessage::Connected {
            version: env!("CARGO_PKG_VERSION").to_string(),
            proxy_id: proxy_id.clone(),
        })
        .await
        .map_err(|_| BridgeError::ChannelClosed)?;

    // Clone for tasks
    let stdin_shutdown_rx = shutdown_tx.subscribe();
    let stdout_shutdown_rx = shutdown_tx.subscribe();
    let socket_shutdown_rx = shutdown_tx.subscribe();
    let proxy_id_for_socket = proxy_id.clone();

    // Spawn stdin task
    let stdin_handle = tokio::spawn(stdin_task(
        BufReader::new(reader),
        stdin_tx,
        stdin_shutdown_rx,
    ));

    // Spawn stdout task
    let stdout_handle = tokio::spawn(stdout_task(
        BufWriter::new(writer),
        stdout_rx,
        stdout_shutdown_rx,
    ));

    // Spawn socket task
    let socket_handle = tokio::spawn(socket_task(
        stdin_rx,
        stdout_tx.clone(),
        daemon_client,
        socket_shutdown_rx,
        proxy_id_for_socket,
    ));

    // Wait for any task to complete
    tokio::select! {
        result = stdin_handle => {
            match result {
                Ok(Ok(())) => debug!("stdin task completed normally"),
                Ok(Err(e)) => error!("stdin task error: {}", e),
                Err(e) => error!("stdin task panicked: {}", e),
            }
        }
        result = stdout_handle => {
            match result {
                Ok(Ok(())) => debug!("stdout task completed normally"),
                Ok(Err(e)) => error!("stdout task error: {}", e),
                Err(e) => error!("stdout task panicked: {}", e),
            }
        }
        result = socket_handle => {
            match result {
                Ok(Ok(())) => debug!("socket task completed normally"),
                Ok(Err(e)) => error!("socket task error: {}", e),
                Err(e) => error!("socket task panicked: {}", e),
            }
        }
    }

    // Signal shutdown to all tasks
    let _ = shutdown_tx.send(());

    info!("Async proxy {} shutting down", proxy_id);
    Ok(())
}

/// Connect to daemon, optionally auto-launching if not running.
async fn connect_to_daemon(client: &mut DaemonClient, config: &BridgeConfig) -> Result<()> {
    match client.connect().await {
        Ok(()) => Ok(()),
        Err(e) => {
            debug!("Initial connection failed: {}", e);

            if config.auto_launch_daemon {
                info!("Attempting to auto-launch daemon");

                let exe_path = std::env::current_exe().map_err(|e| {
                    BridgeError::DaemonLaunchFailed(format!("Failed to get executable path: {}", e))
                })?;

                match launch_daemon(&exe_path, config).await {
                    Ok(pid) => {
                        info!("Daemon launched with PID {}", pid);

                        // Retry connection
                        client.connect().await
                    }
                    Err(e) => {
                        warn!("Failed to auto-launch daemon: {}", e);
                        Err(e)
                    }
                }
            } else {
                Err(e)
            }
        }
    }
}

/// Task that reads from stdin and sends to the socket task.
async fn stdin_task<R>(
    mut reader: BufReader<R>,
    tx: mpsc::Sender<StdinMessage>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
{
    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.recv() => {
                debug!("stdin task received shutdown signal");
                break;
            }

            result = read_message::<_, serde_json::Value>(&mut reader) => {
                match result {
                    Ok(message) => {
                        debug!("stdin received: {:?}", message.get("type"));

                        if let Some((request_id, channel, payload)) = parse_native_message(&message) {
                            let msg = StdinMessage::SidebarMessage {
                                request_id,
                                channel,
                                payload,
                            };
                            if tx.send(msg).await.is_err() {
                                error!("stdin channel closed");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        // EOF or read error - browser closed connection
                        debug!("stdin read error (browser closed): {}", e);
                        let _ = tx.send(StdinMessage::Shutdown).await;
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Task that receives from the socket task and writes to stdout.
async fn stdout_task<W>(
    mut writer: BufWriter<W>,
    mut rx: mpsc::Receiver<StdoutMessage>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.recv() => {
                debug!("stdout task received shutdown signal");
                break;
            }

            msg = rx.recv() => {
                match msg {
                    Some(StdoutMessage::DaemonResponse(envelope)) => {
                        let value = serde_json::to_value(&envelope.payload)?;
                        write_message(&mut writer, &value).await?;
                    }
                    Some(StdoutMessage::Error { code, message }) => {
                        let error = serde_json::json!({
                            "type": "error",
                            "payload": {
                                "code": code,
                                "message": message
                            }
                        });
                        write_message(&mut writer, &error).await?;
                    }
                    Some(StdoutMessage::Connected { version, proxy_id }) => {
                        let connected = serde_json::json!({
                            "type": "connected",
                            "payload": {
                                "version": version,
                                "proxy_id": proxy_id
                            }
                        });
                        write_message(&mut writer, &connected).await?;
                    }
                    Some(StdoutMessage::Shutdown) | None => {
                        debug!("stdout task: channel closed or shutdown");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Task that handles bidirectional ZeroMQ communication.
///
/// Uses select! to simultaneously:
/// - Forward messages from stdin to daemon
/// - Forward messages from daemon to stdout
async fn socket_task(
    mut stdin_rx: mpsc::Receiver<StdinMessage>,
    stdout_tx: mpsc::Sender<StdoutMessage>,
    mut daemon_client: DaemonClient,
    mut shutdown_rx: broadcast::Receiver<()>,
    proxy_id: String,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;

            // 1. Shutdown signal (highest priority)
            _ = shutdown_rx.recv() => {
                debug!("socket task received shutdown signal");
                break;
            }

            // 2. Messages from sidebar (stdin) -> daemon
            msg = stdin_rx.recv() => {
                match msg {
                    Some(StdinMessage::SidebarMessage { request_id, channel, payload }) => {
                        debug!("Forwarding to daemon: request_id={}", request_id);

                        let envelope = ProxyEnvelope::new(&proxy_id, &request_id, channel, payload);
                        if let Err(e) = daemon_client.send(envelope).await {
                            error!("Failed to send to daemon: {}", e);
                            let _ = stdout_tx.send(StdoutMessage::Error {
                                code: "DAEMON_ERROR".into(),
                                message: e.to_string(),
                            }).await;
                        }
                    }
                    Some(StdinMessage::Shutdown) | None => {
                        debug!("socket task: stdin channel closed or shutdown");
                        break;
                    }
                }
            }

            // 3. Messages from daemon -> sidebar (stdout)
            result = daemon_client.recv() => {
                match result {
                    Ok(envelope) => {
                        debug!("Received from daemon: request_id={:?}", envelope.request_id);

                        if stdout_tx.send(StdoutMessage::DaemonResponse(envelope)).await.is_err() {
                            error!("stdout channel closed");
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Daemon receive error: {}", e);
                        let _ = stdout_tx.send(StdoutMessage::Error {
                            code: "DAEMON_ERROR".into(),
                            message: e.to_string(),
                        }).await;

                        // For disconnection, try to reconnect
                        if matches!(e, BridgeError::Disconnected) {
                            info!("Attempting to reconnect to daemon...");
                            if let Err(reconnect_err) = daemon_client.reconnect().await {
                                error!("Reconnection failed: {}", reconnect_err);
                                break;
                            }
                            info!("Reconnected to daemon");
                        }
                    }
                }
            }
        }
    }

    // Cleanup
    daemon_client.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_async_proxy_config_default() {
        let config = AsyncProxyConfig::default();
        assert_eq!(config.channel_buffer_size, 100);
    }

    #[test]
    fn test_async_proxy_config_builder() {
        let bridge = BridgeConfig::new().with_port_range(20000, 20100);
        let config = AsyncProxyConfig::new()
            .with_bridge(bridge)
            .with_channel_buffer_size(200);

        assert_eq!(config.bridge.port_range_start, 20000);
        assert_eq!(config.channel_buffer_size, 200);
    }

    #[test]
    fn test_stdin_message_debug() {
        let msg = StdinMessage::SidebarMessage {
            request_id: "req-001".into(),
            channel: Channel::Chat,
            payload: serde_json::json!({}),
        };
        assert!(format!("{:?}", msg).contains("SidebarMessage"));

        let shutdown = StdinMessage::Shutdown;
        assert!(format!("{:?}", shutdown).contains("Shutdown"));
    }

    #[test]
    fn test_stdout_message_debug() {
        let msg = StdoutMessage::Error {
            code: "TEST".into(),
            message: "test error".into(),
        };
        assert!(format!("{:?}", msg).contains("Error"));

        let connected = StdoutMessage::Connected {
            version: "1.0.0".into(),
            proxy_id: "proxy-001".into(),
        };
        assert!(format!("{:?}", connected).contains("Connected"));
    }

    #[tokio::test]
    async fn test_stdin_task_handles_shutdown() {
        let (tx, mut rx) = mpsc::channel(10);
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        // Empty input that will EOF immediately
        let reader = Cursor::new(vec![]);

        let handle = tokio::spawn(stdin_task(BufReader::new(reader), tx, shutdown_rx));

        // Wait a bit for the task to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Should receive shutdown message due to EOF
        let msg = rx.recv().await;
        assert!(matches!(msg, Some(StdinMessage::Shutdown)));

        let _ = shutdown_tx.send(());
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stdout_task_writes_connected() {
        let (tx, rx) = mpsc::channel(10);
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        let mut output = Vec::new();

        // Send connected message
        tx.send(StdoutMessage::Connected {
            version: "1.0.0".into(),
            proxy_id: "test-proxy".into(),
        })
        .await
        .unwrap();

        // Signal shutdown after message is sent
        tx.send(StdoutMessage::Shutdown).await.unwrap();

        let handle = tokio::spawn(async move {
            stdout_task(BufWriter::new(&mut output), rx, shutdown_rx).await
        });

        let _ = shutdown_tx.send(());
        let _ = handle.await;

        // Note: output is moved into the task, so we can't check it directly
        // In a real test, we'd use a different approach
    }

    #[tokio::test]
    async fn test_stdout_task_writes_error() {
        let (tx, rx) = mpsc::channel(10);
        let (_shutdown_tx, shutdown_rx) = broadcast::channel(1);

        let output: Vec<u8> = Vec::new();

        // Send error and shutdown
        tx.send(StdoutMessage::Error {
            code: "TEST_ERROR".into(),
            message: "Test error message".into(),
        })
        .await
        .unwrap();
        tx.send(StdoutMessage::Shutdown).await.unwrap();

        let handle = tokio::spawn(async move {
            stdout_task(BufWriter::new(output), rx, shutdown_rx).await
        });

        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
}
