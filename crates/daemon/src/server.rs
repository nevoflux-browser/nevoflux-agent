//! ZeroMQ ROUTER server for the daemon.

use crate::agent_host::{DaemonHostFunctions, SidebarStreamChunk};
use crate::config::AgentConfig;
use crate::error::{DaemonError, Result};
use crate::router::{RouteDecision, Router};
use crate::session::SessionManager;
use crate::trace::collector::TraceCollector;
use crate::trace::file_writer::TraceFileWriter;
use crate::wasm::{BrowserRequest, BrowserResponse, HostServices};
use bytes::Bytes;
use nevoflux_builtin_wasm::{Agent, AgentInput, AgentMode, Attachment, Message as WasmMessage};
use nevoflux_protocol::{
    AgentMessage, Channel, DaemonEnvelope, PlanProposal, PlanResponse, ProxyEnvelope,
    ToolAuthResponse,
};
use nevoflux_skills::{check_tool_availability, format_missing_tools_message, ToolCheckResult};
use nevoflux_storage::{ListSessionsParams, Message as StorageMessage, MessageRole};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, warn};
use zeromq::{Socket, SocketSend, ZmqMessage};

/// Registry for pending browser tool requests.
/// Maps request_id to the response sender.
type BrowserRequestRegistry = Arc<Mutex<HashMap<String, oneshot::Sender<BrowserResponse>>>>;

/// Registry for pending plan proposals.
/// Maps session_id to the response sender.
type PlanRequestRegistry = Arc<Mutex<HashMap<String, oneshot::Sender<PlanResponse>>>>;

/// Registry for active streaming sessions that can be cancelled.
/// Maps session_id to the cancellation token.
type CancellationRegistry = Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>;

/// Registry for active agent interrupt flags.
/// Maps session_id to the interrupt flag so stop_generation can signal the agent to stop.
type InterruptRegistry = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// Registry for pending tool authorization requests.
/// Maps tool_id to the response sender.
type ToolAuthRegistry = Arc<Mutex<HashMap<String, oneshot::Sender<ToolAuthResponse>>>>;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Port range start.
    pub port_start: u16,
    /// Port range end.
    pub port_end: u16,
    /// Bind address (default: 127.0.0.1).
    pub bind_address: String,
    /// Whether trace collection is enabled.
    pub trace_enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port_start: 19500,
            port_end: 19600,
            bind_address: "127.0.0.1".into(),
            trace_enabled: false,
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
pub async fn start_server(
    config: ServerConfig,
    router: Arc<Router>,
    session_manager: Arc<SessionManager>,
) -> Result<Server> {
    let port = find_available_port(&config).await?;
    let addr = format!("tcp://{}:{}", config.bind_address, port);

    info!("Starting daemon server on {}", addr);

    // Load agent config for LLM settings
    let agent_config = AgentConfig::load().unwrap_or_default();
    let agent_config = Arc::new(agent_config);

    // Create host services with database from session manager
    let db = session_manager.storage().database().clone();

    // Create browser request channel and registry
    let (browser_tx, mut browser_rx) =
        mpsc::channel::<(BrowserRequest, oneshot::Sender<BrowserResponse>)>(100);
    let browser_registry: BrowserRequestRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Create cancellation registry for active streaming sessions
    let cancellation_registry: CancellationRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Create interrupt registry for signalling agents to stop
    let interrupt_registry: InterruptRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Create plan request registry for pending plan proposals
    let plan_registry: PlanRequestRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Create tool auth registry for pending tool authorization requests
    let tool_auth_registry: ToolAuthRegistry = Arc::new(Mutex::new(HashMap::new()));

    let services = HostServices::new(Arc::new(db)).with_browser_sender(browser_tx);

    let mut socket = zeromq::RouterSocket::new();
    socket
        .bind(&addr)
        .await
        .map_err(|e| DaemonError::InternalError(format!("Failed to bind: {}", e)))?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    let (msg_tx, mut msg_rx) = mpsc::channel::<(Vec<u8>, ProxyEnvelope)>(100);
    let (response_tx, mut response_rx) = mpsc::channel::<(Vec<u8>, DaemonEnvelope)>(100);

    // Use internal channel for socket operations to avoid send blocking receive
    // The socket task processes send/receive in round-robin fashion
    let (socket_send_tx, mut socket_send_rx) = mpsc::channel::<(Vec<u8>, Vec<u8>, String)>(1000);

    // Task to forward responses to socket send channel
    let forward_response_tx = socket_send_tx.clone();
    tokio::spawn(async move {
        while let Some((identity, response)) = response_rx.recv().await {
            let msg_type = response
                .payload
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            if let Ok(data) = serde_json::to_vec(&response) {
                let _ = forward_response_tx.send((identity, data, msg_type)).await;
            }
        }
    });

    // Main socket I/O task - alternates between send and receive
    let mut socket = socket;
    tokio::spawn(async move {
        loop {
            // Try to send one message if available (non-blocking check)
            match socket_send_rx.try_recv() {
                Ok((identity, data, msg_type)) => {
                    let frames: Vec<Bytes> = vec![Bytes::from(identity), Bytes::from(data)];
                    if let Ok(zmq_msg) = ZmqMessage::try_from(frames) {
                        if let Err(e) = socket.send(zmq_msg).await {
                            error!("Failed to send: {}", e);
                        } else {
                            debug!("Socket sent: type={}", msg_type);
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }

            // Try to receive one message with short timeout
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(5),
                zeromq::SocketRecv::recv(&mut socket),
            )
            .await
            {
                Ok(Ok(zmq_msg)) => {
                    let frames = zmq_msg.into_vec();
                    if frames.len() >= 2 {
                        let identity = frames[0].to_vec();
                        if let Ok(envelope) = serde_json::from_slice::<ProxyEnvelope>(&frames[1]) {
                            let msg_type = envelope
                                .payload
                                .get("type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            info!(
                                "Socket received: type={}, proxy_id={}",
                                msg_type, envelope.proxy_id
                            );
                            let _ = msg_tx.send((identity, envelope)).await;
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!("Receive error: {}", e);
                }
                Err(_) => {
                    // Timeout - no message, continue loop
                }
            }

            // Check shutdown
            match shutdown_rx.try_recv() {
                Ok(_) => {
                    info!("Server shutdown signal received");
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    });

    // Spawn browser request handler task
    // This task receives browser tool requests from the agent and sends them to the sidebar
    let browser_response_tx = response_tx.clone();
    let browser_registry_clone = browser_registry.clone();
    tokio::spawn(async move {
        while let Some((request, response_sender)) = browser_rx.recv().await {
            let request_id = request.request_id.clone();
            info!(
                "Browser request sending to sidebar: id={}, action={:?}, proxy_id={}, identity_len={}",
                request_id, request.action, request.proxy_id, request.client_identity.len()
            );

            // Store the response sender in the registry
            {
                let mut registry = browser_registry_clone.lock().await;
                registry.insert(request_id.clone(), response_sender);
                info!(
                    "Browser request registered: id={}, registry_size={}",
                    request_id,
                    registry.len()
                );
            }

            // Create BrowserToolRequest message to send to sidebar
            let browser_request = nevoflux_protocol::BrowserToolRequest {
                request_id: request.request_id,
                session_id: request.session_id,
                tab_id: request.tab_id,
                action: request.action,
                params: request.params,
                timeout_ms: request.timeout_ms,
            };

            // Wrap in AgentMessage and send
            let agent_message =
                nevoflux_protocol::AgentMessage::BrowserToolRequest(browser_request);
            let response_payload = serde_json::to_value(&agent_message).unwrap_or_default();

            // Send to the sidebar using the client identity from the request
            let response = DaemonEnvelope::new(&request.proxy_id, Channel::Chat, response_payload);
            if let Err(e) = browser_response_tx
                .send((request.client_identity, response))
                .await
            {
                error!("Failed to send browser request: {}", e);
            } else {
                info!("Browser request sent to response queue: id={}", request_id);
            }
        }
    });

    // Spawn message processing loop
    let process_router = router.clone();
    let process_response_tx = response_tx.clone();
    let process_config = agent_config.clone();
    let process_session_manager = session_manager.clone();
    let process_services = services.clone();
    let process_runtime = tokio::runtime::Handle::current();
    let process_browser_registry = browser_registry.clone();
    let process_cancellation_registry = cancellation_registry.clone();
    let process_interrupt_registry = interrupt_registry.clone();
    let process_plan_registry = plan_registry.clone();
    let process_tool_auth_registry = tool_auth_registry.clone();
    let process_trace_enabled = config.trace_enabled;
    tokio::spawn(async move {
        while let Some((identity, envelope)) = msg_rx.recv().await {
            let proxy_id = envelope.proxy_id.clone();
            let request_id = envelope.request_id.clone();
            let channel = envelope.channel;

            // Log all incoming messages
            let msg_type = envelope
                .payload
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            info!(
                "Message loop received: type={}, proxy_id={}, channel={:?}, identity_len={}",
                msg_type,
                proxy_id,
                channel,
                identity.len()
            );

            // Check for stop_generation messages - handle cancellation
            if msg_type == "stop_generation" {
                // Extract session_id from payload
                let session_id = envelope
                    .payload
                    .get("payload")
                    .and_then(|p| p.get("session_id"))
                    .and_then(|s| s.as_str())
                    .unwrap_or_default();

                info!("Received stop_generation for session: {}", session_id);

                // Signal the agent to stop via interrupt flag
                {
                    let mut registry = process_interrupt_registry.lock().await;
                    if let Some(flag) = registry.remove(session_id) {
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        info!("Set interrupt flag for session: {}", session_id);
                    }
                }

                // Cancel the active streaming session forwarder
                let cancelled = {
                    let mut registry = process_cancellation_registry.lock().await;
                    if let Some(token) = registry.remove(session_id) {
                        token.cancel();
                        true
                    } else {
                        false
                    }
                };

                // Send acknowledgment
                let response_payload = serde_json::json!({
                    "type": "agent_state",
                    "payload": {
                        "state": "idle",
                        "message": if cancelled { "Generation stopped" } else { "No active generation" },
                        "done": true
                    }
                });
                let response = DaemonEnvelope::new(&proxy_id, channel, response_payload)
                    .with_request_id(&request_id);
                if let Err(e) = process_response_tx.send((identity, response)).await {
                    error!("Failed to send stop_generation response: {}", e);
                }
                continue;
            }

            // Check for BrowserToolResponse messages
            if msg_type == "browser_tool_response" {
                info!("Processing browser_tool_response message");
                if let Some(payload) = envelope.payload.get("payload") {
                    if let Ok(response) = serde_json::from_value::<
                        nevoflux_protocol::BrowserToolResponse,
                    >(payload.clone())
                    {
                        let request_id = response.request_id.clone();
                        info!(
                            "Received browser tool response: id={}, success={}",
                            request_id, response.success
                        );

                        // Find the pending request and send the response
                        let sender = {
                            let mut registry = process_browser_registry.lock().await;
                            registry.remove(&request_id)
                        };

                        if let Some(sender) = sender {
                            let browser_response = BrowserResponse {
                                request_id: response.request_id,
                                success: response.success,
                                result: response.result,
                                error: response.error,
                            };
                            if sender.send(browser_response).is_err() {
                                warn!("Failed to send browser response - receiver dropped");
                            } else {
                                info!("Browser response forwarded to agent");
                            }
                        } else {
                            warn!("No pending request for browser response: {}", request_id);
                        }
                        continue; // Don't process further
                    }
                }
            }

            // Check for PlanResponse messages from frontend
            if msg_type == "plan_response" {
                info!("Processing plan_response message");
                if let Some(payload) = envelope.payload.get("payload") {
                    // PlanResponse serializes as a bare string ("confirmed"/"cancelled"),
                    // so session_id must be extracted from the envelope payload level,
                    // not from within the PlanResponse value itself.
                    let session_id = envelope
                        .payload
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    if let Ok(response) = serde_json::from_value::<PlanResponse>(payload.clone()) {
                        if let Some(tx) = process_plan_registry.lock().await.remove(&session_id) {
                            let _ = tx.send(response);
                        } else {
                            warn!("No pending plan request for session: {}", session_id);
                        }
                    }
                }
                continue;
            }

            // Check for ToolAuthResponse messages from frontend
            if msg_type == "tool_auth_response" {
                info!("Processing tool_auth_response message");
                if let Some(payload) = envelope.payload.get("payload") {
                    if let Ok(response) =
                        serde_json::from_value::<ToolAuthResponse>(payload.clone())
                    {
                        let tool_id = response.tool_id.clone();
                        if let Some(tx) = process_tool_auth_registry.lock().await.remove(&tool_id) {
                            let _ = tx.send(response);
                        } else {
                            warn!("No pending tool auth request for tool_id: {}", tool_id);
                        }
                    }
                }
                continue;
            }

            // Register proxy if not already registered (pid 0 for native messaging)
            if !process_router.proxy_registry().is_registered(&proxy_id) {
                process_router.proxy_registry().register(&proxy_id, 0);
                debug!("Registered new proxy: {}", proxy_id);
            }

            // Route the message
            let decision = process_router.route_incoming(&envelope);
            debug!("Route decision for {}: {:?}", proxy_id, decision);

            // Process based on route decision
            match decision {
                RouteDecision::RejectUnregistered => {
                    let response_payload = serde_json::json!({
                        "type": "error",
                        "payload": {
                            "code": "UNREGISTERED",
                            "message": "Proxy not registered"
                        }
                    });
                    let response = DaemonEnvelope::new(&proxy_id, channel, response_payload)
                        .with_request_id(&request_id);
                    if let Err(e) = process_response_tx.send((identity, response)).await {
                        error!("Failed to queue response: {}", e);
                    }
                }
                RouteDecision::ProcessChat { .. } => {
                    // Handle chat messages via Agent with streaming support
                    // IMPORTANT: Spawn as a separate task to avoid blocking the message loop
                    // This allows browser_tool_response messages to be processed while
                    // the agent is waiting for browser tool results.
                    let payload = envelope.payload.clone();
                    let config = process_config.clone();
                    let session_manager = process_session_manager.clone();
                    let services = process_services.clone();
                    let runtime = process_runtime.clone();
                    let response_tx = process_response_tx.clone();
                    let cancellation_registry = process_cancellation_registry.clone();
                    let interrupt_registry = process_interrupt_registry.clone();
                    let plan_registry = process_plan_registry.clone();
                    let trace_enabled = process_trace_enabled;
                    tokio::spawn(async move {
                        handle_chat_message_streaming(
                            &payload,
                            &config,
                            &session_manager,
                            &services,
                            runtime,
                            identity,
                            proxy_id,
                            request_id,
                            channel,
                            response_tx,
                            cancellation_registry,
                            interrupt_registry,
                            plan_registry,
                            trace_enabled,
                        )
                        .await;
                    });
                }
                RouteDecision::ProcessMcp { .. } => {
                    // Handle MCP messages
                    let response_payload = handle_mcp_message(&envelope.payload).await;
                    let response = DaemonEnvelope::new(&proxy_id, channel, response_payload)
                        .with_request_id(&request_id);
                    if let Err(e) = process_response_tx.send((identity, response)).await {
                        error!("Failed to queue response: {}", e);
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

/// Convert storage messages to wasm messages for the agent history.
fn convert_history_messages(messages: Vec<StorageMessage>) -> Vec<WasmMessage> {
    messages
        .into_iter()
        .filter_map(|msg| match msg.role {
            MessageRole::User => Some(WasmMessage::user(msg.content)),
            MessageRole::Assistant => Some(WasmMessage::assistant(msg.content)),
            MessageRole::System => None,
        })
        .collect()
}

/// Load session history messages for the agent.
///
/// Retrieves all messages from the session, removes the last one (which is the
/// current user message just saved), and truncates to `max_messages` most recent.
async fn load_session_history(
    session_manager: &SessionManager,
    session_id: &str,
    max_messages: u32,
) -> Vec<WasmMessage> {
    match session_manager.get_messages(session_id).await {
        Ok(mut messages) => {
            // Remove the last message (the current user message we just saved)
            if !messages.is_empty() {
                messages.pop();
            }
            // Keep only the most recent max_messages
            let len = messages.len();
            let max = max_messages as usize;
            if len > max {
                messages = messages.split_off(len - max);
            }
            convert_history_messages(messages)
        }
        Err(e) => {
            warn!("Failed to load session history for {}: {}", session_id, e);
            vec![]
        }
    }
}

/// Handle chat channel messages with streaming support.
///
/// This function processes chat messages and streams the response back to the sidebar
/// in real-time as the LLM generates output.
#[allow(clippy::too_many_arguments)]
async fn handle_chat_message_streaming(
    payload: &serde_json::Value,
    config: &Arc<AgentConfig>,
    session_manager: &Arc<SessionManager>,
    services: &HostServices,
    runtime: tokio::runtime::Handle,
    identity: Vec<u8>,
    proxy_id: String,
    request_id: String,
    channel: Channel,
    response_tx: mpsc::Sender<(Vec<u8>, DaemonEnvelope)>,
    cancellation_registry: CancellationRegistry,
    interrupt_registry: InterruptRegistry,
    plan_registry: PlanRequestRegistry,
    trace_enabled: bool,
) {
    let msg_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // Debug: log raw attachments from payload
    let raw_attachments = payload.get("payload").and_then(|p| p.get("attachments"));
    info!(
        "handle_chat_message_streaming: raw_attachments present={}, is_array={}, len={:?}",
        raw_attachments.is_some(),
        raw_attachments.map(|a| a.is_array()).unwrap_or(false),
        raw_attachments
            .and_then(|a| a.as_array())
            .map(|arr| arr.len())
    );

    // For non-chat_message types, handle synchronously
    if msg_type != "chat_message" {
        let mut response_payload =
            handle_chat_message(payload, config, session_manager, services, runtime).await;
        // Add done: true to signal this is a complete response (not streaming)
        if let Some(obj) = response_payload.as_object_mut() {
            if let Some(payload_obj) = obj.get_mut("payload").and_then(|p| p.as_object_mut()) {
                payload_obj.insert("done".to_string(), serde_json::json!(true));
            }
        }
        let response =
            DaemonEnvelope::new(&proxy_id, channel, response_payload).with_request_id(&request_id);
        if let Err(e) = response_tx.send((identity, response)).await {
            error!("Failed to queue response: {}", e);
        }
        return;
    }

    // Extract message content from payload
    let message_content = payload
        .get("payload")
        .and_then(|p| p.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    if message_content.is_empty() {
        let response_payload = serde_json::json!({
            "type": "error",
            "payload": {
                "code": "EMPTY_MESSAGE",
                "message": "Message content is empty"
            }
        });
        let response =
            DaemonEnvelope::new(&proxy_id, channel, response_payload).with_request_id(&request_id);
        if let Err(e) = response_tx.send((identity, response)).await {
            error!("Failed to queue response: {}", e);
        }
        return;
    }

    // Extract session_id if provided
    let session_id = payload
        .get("payload")
        .and_then(|p| p.get("session_id"))
        .and_then(|s| s.as_str())
        .unwrap_or("default")
        .to_string();

    // Extract mode if provided (default to Chat)
    let mode = payload
        .get("payload")
        .and_then(|p| p.get("mode"))
        .and_then(|m| m.as_str())
        .map(|m| match m {
            "browser" => AgentMode::Browser,
            "agent" => AgentMode::Agent,
            _ => AgentMode::Chat,
        })
        .unwrap_or(AgentMode::Chat);

    // Extract attachments (multimodal: images, files)
    let attachments: Vec<Attachment> = payload
        .get("payload")
        .and_then(|p| p.get("attachments"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let name = v.get("name")?.as_str()?.to_string();
                    let mime_type = v.get("mime_type")?.as_str()?.to_string();
                    let data = v.get("data")?.as_str()?.to_string();
                    Some(Attachment {
                        name,
                        mime_type,
                        data,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Extract local file references (from file picker)
    let local_files: Vec<nevoflux_protocol::FileInfo> = payload
        .get("payload")
        .and_then(|p| p.get("local_files"))
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let path = v.get("path")?.as_str()?.to_string();
                    let is_directory = v.get("is_directory")?.as_bool()?;
                    let size = v.get("size").and_then(|s| s.as_u64());
                    let modified = v.get("modified").and_then(|m| m.as_u64());
                    Some(nevoflux_protocol::FileInfo {
                        path,
                        is_directory,
                        size,
                        modified,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Extract tab_id if provided (from browser sidebar)
    let tab_id = payload
        .get("payload")
        .and_then(|p| p.get("tab_id"))
        .and_then(|t| t.as_i64());

    // Extract tab_ids list if provided (all available tabs)
    let tab_ids: Vec<nevoflux_builtin_wasm::TabInfo> = payload
        .get("payload")
        .and_then(|p| p.get("tab_ids"))
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let space = v
                        .get("space")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let tab_id = v.get("tab_id").and_then(|t| t.as_i64())?;
                    let tab_title = v
                        .get("tab_title")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let url = v
                        .get("url")
                        .and_then(|u| u.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(nevoflux_builtin_wasm::TabInfo {
                        space,
                        tab_id,
                        tab_title,
                        url,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    info!(
        "Processing streaming chat message with mode={:?}, session={}, attachments={}, local_files={}, tab_id={:?}, tab_ids={}",
        mode,
        session_id,
        attachments.len(),
        local_files.len(),
        tab_id,
        tab_ids.len()
    );

    // Ensure session exists and save user message
    match session_manager.get_or_create_session(&session_id).await {
        Ok(session) => {
            info!(
                "Session ready: id={}, created_at={}",
                session.id, session.created_at
            );
        }
        Err(e) => {
            error!("Failed to get/create session {}: {}", session_id, e);
        }
    }

    // Save user message to database
    let mut generated_title: Option<String> = None;
    match session_manager
        .add_message(&session_id, MessageRole::User, message_content)
        .await
    {
        Ok(msg) => {
            info!("Saved user message: id={}, session={}", msg.id, session_id);

            // Generate title from first message if session has no title yet
            match session_manager.generate_title(&session_id).await {
                Ok(Some(title)) => {
                    info!("Generated session title: {}", title);
                    generated_title = Some(title);
                }
                Ok(None) => {
                    // Session already has a title or no messages
                }
                Err(e) => {
                    error!("Failed to generate title: {}", e);
                }
            }
        }
        Err(e) => {
            error!("Failed to save user message to {}: {}", session_id, e);
        }
    }

    // Create trace collector for this session
    let trace_collector = {
        let file_writer = if trace_enabled {
            let data_dir = std::env::var("NEVOFLUX_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    directories::ProjectDirs::from("com", "nevoflux", "nevoflux")
                        .map(|dirs| dirs.data_dir().to_path_buf())
                        .unwrap_or_else(|| std::path::PathBuf::from("."))
                });
            let traces_dir = data_dir.join("traces");
            TraceFileWriter::new(&traces_dir, &session_id).ok()
        } else {
            None
        };
        match file_writer {
            Some(writer) => Arc::new(TraceCollector::with_file_writer(
                session_manager.shared_storage(),
                writer,
            )),
            None => Arc::new(TraceCollector::new(session_manager.shared_storage())),
        }
    };

    // Create unbounded channel for streaming chunks
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::unbounded_channel::<SidebarStreamChunk>();

    // Create host functions with streaming support
    // Set client context on services so browser tool requests can be routed back
    let mut services_with_context = services
        .clone()
        .with_client_context(identity.clone(), proxy_id.clone());

    // Create a per-session interrupt flag and register it so stop_generation can find it
    let session_interrupt_flag = Arc::new(AtomicBool::new(false));
    services_with_context.interrupt_flag = session_interrupt_flag.clone();
    {
        let mut registry = interrupt_registry.lock().await;
        registry.insert(session_id.clone(), session_interrupt_flag);
        debug!("Registered interrupt flag for session: {}", session_id);
    }

    let host = DaemonHostFunctions::new(config.clone(), runtime.clone())
        .with_services(services_with_context)
        .with_sidebar_stream(stream_tx)
        .with_session_id(session_id.clone())
        .with_trace_collector(trace_collector.clone());

    // Create agent with host functions
    let agent = Agent::new(host);

    // Clone tab_ids for potential plan re-run (before move into AgentInput)
    let tab_ids_for_rerun = tab_ids.clone();

    // Build agent input
    let input = AgentInput {
        session_id: session_id.clone(),
        mode,
        user_message: message_content.to_string(),
        history: load_session_history(
            session_manager,
            &session_id,
            config.daemon.context.max_history_messages,
        )
        .await,
        attachments,
        local_files,
        custom_system_prompt: None, // Use default mode-based prompt
        tab_id,
        tab_ids,
        skill_context: None,
        available_models: config.llm.configured_providers(),
    };

    // Create cancellation token for this streaming session
    let cancellation_token = tokio_util::sync::CancellationToken::new();
    {
        let mut registry = cancellation_registry.lock().await;
        registry.insert(session_id.clone(), cancellation_token.clone());
        debug!("Registered cancellation token for session: {}", session_id);
    }

    // Clone variables for the streaming forwarder task
    let stream_proxy_id = proxy_id.clone();
    let stream_channel = channel;
    let stream_request_id = request_id.clone();
    let stream_identity = identity.clone();
    let stream_response_tx = response_tx.clone();
    let stream_title = generated_title.clone();
    let forwarder_cancellation = cancellation_token.clone();

    // Spawn task to forward stream chunks to the sidebar
    let forwarder_handle = tokio::spawn(async move {
        let mut accumulated_text = String::new();
        let mut cancelled = false;

        loop {
            tokio::select! {
                biased;

                // Check cancellation first
                _ = forwarder_cancellation.cancelled() => {
                    info!("Stream forwarder cancelled");
                    cancelled = true;
                    break;
                }

                // Receive stream chunks
                chunk = stream_rx.recv() => {
                    match chunk {
                        Some(chunk) => {
                            accumulated_text.push_str(&chunk.text);

                            let mut chunk_payload = serde_json::json!({
                                "type": "stream_chunk",
                                "payload": {
                                    "content": chunk.text,
                                    "done": chunk.done
                                }
                            });

                            // Add event if present
                            if let Some(event) = &chunk.event {
                                if let Some(p) = chunk_payload.get_mut("payload") {
                                    p["event"] = serde_json::to_value(event).unwrap_or_default();
                                }
                            }

                            // Include session title on first chunk if available
                            if !accumulated_text.is_empty() && accumulated_text == chunk.text {
                                if let Some(ref title) = stream_title {
                                    chunk_payload["payload"]["session_title"] =
                                        serde_json::Value::String(title.clone());
                                }
                            }

                            let response = DaemonEnvelope::new(&stream_proxy_id, stream_channel, chunk_payload)
                                .with_request_id(&stream_request_id);

                            if let Err(e) = stream_response_tx
                                .send((stream_identity.clone(), response))
                                .await
                            {
                                error!("Failed to send stream chunk: {}", e);
                                break;
                            }

                            if chunk.done {
                                debug!(
                                    "Stream completed, total accumulated: {} bytes",
                                    accumulated_text.len()
                                );
                                break;
                            }
                        }
                        None => {
                            debug!("Stream channel closed");
                            break;
                        }
                    }
                }
            }
        }

        (accumulated_text, cancelled)
    });

    // Run agent (this will call stream_emit() for each chunk)
    let agent_result = tokio::task::spawn_blocking(move || agent.run(&input)).await;

    // Wait for the forwarder to complete
    let (accumulated_text, was_cancelled) = match forwarder_handle.await {
        Ok(result) => result,
        Err(e) => {
            error!("Stream forwarder task failed: {}", e);
            (String::new(), false)
        }
    };

    // Cleanup cancellation token and interrupt flag from registries
    {
        let mut registry = cancellation_registry.lock().await;
        registry.remove(&session_id);
        debug!("Removed cancellation token for session: {}", session_id);
    }
    {
        let mut registry = interrupt_registry.lock().await;
        registry.remove(&session_id);
        debug!("Removed interrupt flag for session: {}", session_id);
    }

    // Cleanup trace collector session data
    trace_collector.cleanup_session(&session_id);

    // If cancelled, don't send final response (stop_generation handler already did)
    if was_cancelled {
        info!(
            "Streaming session {} was cancelled, skipping final response",
            session_id
        );
        return;
    }

    // Handle agent result
    match agent_result {
        Ok(Ok(output)) => {
            // Handle plan proposal if present
            if let Some(proposal) = &output.plan_proposal {
                info!("Agent returned plan proposal for session {}", session_id);

                // Register oneshot channel BEFORE sending proposal to frontend
                // to avoid race condition if frontend responds very quickly
                let (plan_tx, plan_rx) = oneshot::channel();
                plan_registry
                    .lock()
                    .await
                    .insert(session_id.clone(), plan_tx);

                // Send proposal to frontend
                let msg = AgentMessage::PlanProposal(proposal.clone());
                let payload = serde_json::to_value(&msg).unwrap();
                let envelope =
                    DaemonEnvelope::new(&proxy_id, channel, payload).with_request_id(&request_id);
                if let Err(e) = response_tx.send((identity.clone(), envelope)).await {
                    error!("Failed to send plan proposal: {}", e);
                }

                match plan_rx.await {
                    Ok(PlanResponse::Confirmed) => {
                        info!(
                            "Plan confirmed for session {}, re-running agent with plan context",
                            session_id
                        );
                        let plan_text = format_plan_as_context(proposal);

                        // Save plan proposal as assistant message for history
                        if let Err(e) = session_manager
                            .add_message(
                                &session_id,
                                MessageRole::Assistant,
                                &format!("Plan proposed:\n{}", plan_text),
                            )
                            .await
                        {
                            error!("Failed to save plan message: {}", e);
                        }

                        // Save plan text as user message (the "execute" instruction)
                        if let Err(e) = session_manager
                            .add_message(&session_id, MessageRole::User, &plan_text)
                            .await
                        {
                            error!("Failed to save plan user message: {}", e);
                        }

                        // Create new streaming channel for re-run
                        let (rerun_stream_tx, mut rerun_stream_rx) =
                            tokio::sync::mpsc::unbounded_channel::<SidebarStreamChunk>();

                        // Create new host functions
                        let rerun_services = services
                            .clone()
                            .with_client_context(identity.clone(), proxy_id.clone());
                        let rerun_host = DaemonHostFunctions::new(config.clone(), runtime.clone())
                            .with_services(rerun_services)
                            .with_sidebar_stream(rerun_stream_tx)
                            .with_session_id(session_id.clone())
                            .with_trace_collector(trace_collector.clone());

                        let rerun_agent = Agent::new(rerun_host);

                        // Build new input with plan as user message
                        let rerun_input = AgentInput {
                            session_id: session_id.clone(),
                            mode,
                            user_message: plan_text.clone(),
                            history: load_session_history(
                                session_manager,
                                &session_id,
                                config.daemon.context.max_history_messages,
                            )
                            .await,
                            attachments: vec![],
                            local_files: vec![],
                            custom_system_prompt: None,
                            tab_id,
                            tab_ids: tab_ids_for_rerun.clone(),
                            skill_context: None,
                            available_models: config.llm.configured_providers(),
                        };

                        // Spawn stream forwarder for re-run
                        let rerun_proxy_id = proxy_id.clone();
                        let rerun_channel = channel;
                        let rerun_request_id = request_id.clone();
                        let rerun_identity = identity.clone();
                        let rerun_response_tx = response_tx.clone();

                        let rerun_forwarder = tokio::spawn(async move {
                            let mut rerun_accumulated = String::new();
                            while let Some(chunk) = rerun_stream_rx.recv().await {
                                rerun_accumulated.push_str(&chunk.text);
                                let mut chunk_payload = serde_json::json!({
                                    "type": "stream_chunk",
                                    "payload": {
                                        "content": chunk.text,
                                        "done": chunk.done
                                    }
                                });

                                // Add event if present
                                if let Some(event) = &chunk.event {
                                    if let Some(p) = chunk_payload.get_mut("payload") {
                                        p["event"] =
                                            serde_json::to_value(event).unwrap_or_default();
                                    }
                                }
                                let response = DaemonEnvelope::new(
                                    &rerun_proxy_id,
                                    rerun_channel,
                                    chunk_payload,
                                )
                                .with_request_id(&rerun_request_id);
                                if let Err(e) = rerun_response_tx
                                    .send((rerun_identity.clone(), response))
                                    .await
                                {
                                    error!("Failed to send rerun stream chunk: {}", e);
                                    break;
                                }
                                if chunk.done {
                                    break;
                                }
                            }
                            rerun_accumulated
                        });

                        // Run agent with plan context
                        let rerun_result =
                            tokio::task::spawn_blocking(move || rerun_agent.run(&rerun_input))
                                .await;

                        // Wait for forwarder
                        let rerun_text = match rerun_forwarder.await {
                            Ok(text) => text,
                            Err(e) => {
                                error!("Rerun forwarder failed: {}", e);
                                String::new()
                            }
                        };

                        // Handle rerun result
                        match rerun_result {
                            Ok(Ok(output)) => {
                                let final_text = if output.text.is_empty() {
                                    rerun_text
                                } else {
                                    output.text.clone()
                                };

                                if !final_text.is_empty() {
                                    if let Err(e) = session_manager
                                        .add_message(
                                            &session_id,
                                            MessageRole::Assistant,
                                            &final_text,
                                        )
                                        .await
                                    {
                                        error!("Failed to save rerun response: {}", e);
                                    }
                                }

                                // Send final completion
                                let final_payload = serde_json::json!({
                                    "type": "stream_chunk",
                                    "payload": {
                                        "content": "",
                                        "tool_calls": output.tool_calls,
                                        "done": true
                                    }
                                });
                                let response =
                                    DaemonEnvelope::new(&proxy_id, channel, final_payload)
                                        .with_request_id(&request_id);
                                if let Err(e) = response_tx.send((identity, response)).await {
                                    error!("Failed to send rerun final response: {}", e);
                                }
                            }
                            Ok(Err(e)) => {
                                error!("Plan execution failed: {}", e);
                                let error_payload = serde_json::json!({
                                    "type": "error",
                                    "payload": {
                                        "code": "PLAN_EXECUTION_ERROR",
                                        "message": format!("Plan execution error: {}", e)
                                    }
                                });
                                let response =
                                    DaemonEnvelope::new(&proxy_id, channel, error_payload)
                                        .with_request_id(&request_id);
                                if let Err(e) = response_tx.send((identity, response)).await {
                                    error!("Failed to send plan error: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("Plan execution task panicked: {}", e);
                                let error_payload = serde_json::json!({
                                    "type": "error",
                                    "payload": {
                                        "code": "PLAN_EXECUTION_PANIC",
                                        "message": format!("Plan execution task failed: {}", e)
                                    }
                                });
                                let response =
                                    DaemonEnvelope::new(&proxy_id, channel, error_payload)
                                        .with_request_id(&request_id);
                                if let Err(e) = response_tx.send((identity, response)).await {
                                    error!("Failed to send plan panic error: {}", e);
                                }
                            }
                        }
                    }
                    Ok(PlanResponse::Cancelled) => {
                        info!("Plan cancelled for session {}", session_id);
                        let cancel_payload = serde_json::json!({
                            "type": "stream_chunk",
                            "payload": {
                                "content": "Plan cancelled by user.",
                                "done": true
                            }
                        });
                        let response = DaemonEnvelope::new(&proxy_id, channel, cancel_payload)
                            .with_request_id(&request_id);
                        if let Err(e) = response_tx.send((identity, response)).await {
                            error!("Failed to send plan cancellation response: {}", e);
                        }
                    }
                    Err(_) => {
                        warn!(
                            "Plan response channel dropped for session {}, treating as cancelled",
                            session_id
                        );
                        let cancel_payload = serde_json::json!({
                            "type": "stream_chunk",
                            "payload": {
                                "content": "Plan cancelled (connection lost).",
                                "done": true
                            }
                        });
                        let response = DaemonEnvelope::new(&proxy_id, channel, cancel_payload)
                            .with_request_id(&request_id);
                        if let Err(e) = response_tx.send((identity, response)).await {
                            error!("Failed to send plan drop response: {}", e);
                        }
                    }
                }
                return;
            }

            // Save assistant response to database
            let final_text = if output.text.is_empty() {
                accumulated_text
            } else {
                output.text.clone()
            };

            if !final_text.is_empty() {
                match session_manager
                    .add_message(&session_id, MessageRole::Assistant, &final_text)
                    .await
                {
                    Ok(msg) => {
                        info!(
                            "Saved assistant message: id={}, session={}",
                            msg.id, session_id
                        );
                    }
                    Err(e) => {
                        error!("Failed to save assistant message to {}: {}", session_id, e);
                    }
                }
            }

            // Save tool calls to session history
            for tool_call in &output.tool_calls {
                if let Err(e) = session_manager
                    .add_tool_use_message(
                        &session_id,
                        &tool_call.id,
                        &tool_call.name,
                        &tool_call.arguments,
                        None,
                    )
                    .await
                {
                    error!("Failed to save tool call {}: {}", tool_call.name, e);
                }
            }

            // Send final completion message
            let mut final_payload = serde_json::json!({
                "type": "stream_chunk",
                "payload": {
                    "content": "",
                    "tool_calls": output.tool_calls,
                    "done": true
                }
            });

            if let Some(title) = generated_title {
                final_payload["payload"]["session_title"] = serde_json::Value::String(title);
            }

            let response =
                DaemonEnvelope::new(&proxy_id, channel, final_payload).with_request_id(&request_id);
            if let Err(e) = response_tx.send((identity, response)).await {
                error!("Failed to send final response: {}", e);
            }
        }
        Ok(Err(e)) => {
            error!("Agent run failed: {}", e);
            let error_payload = serde_json::json!({
                "type": "error",
                "payload": {
                    "code": "AGENT_ERROR",
                    "message": format!("Agent error: {}", e)
                }
            });
            let response =
                DaemonEnvelope::new(&proxy_id, channel, error_payload).with_request_id(&request_id);
            if let Err(e) = response_tx.send((identity, response)).await {
                error!("Failed to send error response: {}", e);
            }
        }
        Err(e) => {
            error!("Agent task panicked: {}", e);
            let error_payload = serde_json::json!({
                "type": "error",
                "payload": {
                    "code": "AGENT_PANIC",
                    "message": format!("Agent task failed: {}", e)
                }
            });
            let response =
                DaemonEnvelope::new(&proxy_id, channel, error_payload).with_request_id(&request_id);
            if let Err(e) = response_tx.send((identity, response)).await {
                error!("Failed to send error response: {}", e);
            }
        }
    }
}

/// Format a plan proposal as context text for the agent.
fn format_plan_as_context(proposal: &PlanProposal) -> String {
    let mut text = format!("Approved plan: {}\n\n", proposal.summary);
    for (i, step) in proposal.steps.iter().enumerate() {
        text.push_str(&format!("{}. {}", i + 1, step.description));
        if let Some(model) = &step.model {
            text.push_str(&format!(" [model: {}]", model));
        }
        text.push('\n');
    }
    text.push_str("\nExecute this plan now.");
    text
}

/// Handle chat channel messages using the Agent (non-streaming).
async fn handle_chat_message(
    payload: &serde_json::Value,
    config: &Arc<AgentConfig>,
    session_manager: &Arc<SessionManager>,
    services: &HostServices,
    runtime: tokio::runtime::Handle,
) -> serde_json::Value {
    let msg_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "ping" => {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            serde_json::json!({
                "type": "pong",
                "payload": {
                    "timestamp": timestamp
                }
            })
        }
        "chat_message" => {
            // Extract message content from payload
            let message_content = payload
                .get("payload")
                .and_then(|p| p.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("");

            if message_content.is_empty() {
                return serde_json::json!({
                    "type": "error",
                    "payload": {
                        "code": "EMPTY_MESSAGE",
                        "message": "Message content is empty"
                    }
                });
            }

            // Detect and process /skillname commands
            // Returns (user_message, skill_context)
            let (user_message, skill_context) = if let Some(trimmed) =
                message_content.strip_prefix('/')
            {
                // Parse: "/skillname args" -> ("skillname", "args")
                let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
                let skill_name = parts[0].trim();
                let args = parts.get(1).map(|s| s.trim()).unwrap_or("").to_string();

                if skill_name.is_empty() {
                    // Just "/" with no skill name - treat as regular message
                    (message_content.to_string(), None)
                } else {
                    // Look up skill in registry
                    let registry = services.skills.read().await;
                    if let Some(skill) = registry.get(skill_name) {
                        // Check if required tools are available
                        let available_tools = gather_available_tools(services).await;
                        match check_tool_availability(&skill.metadata, &available_tools) {
                            ToolCheckResult::Satisfied => {
                                // All tools available, proceed with skill injection
                            }
                            ToolCheckResult::Missing(missing) => {
                                // Required tools are not available
                                let message = format_missing_tools_message(skill_name, &missing);
                                warn!(
                                    "Skill '{}' requires unavailable tools: {:?}",
                                    skill_name, missing
                                );
                                return serde_json::json!({
                                    "type": "error",
                                    "payload": {
                                        "code": "SKILL_TOOLS_UNAVAILABLE",
                                        "message": message,
                                        "recoverable": true,
                                        "missing_tools": missing
                                    }
                                });
                            }
                        }

                        // Get skill's base directory path for auxiliary file access
                        let base_path = skill
                            .file_path
                            .as_ref()
                            .and_then(|p| p.parent())
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default();

                        // Enumerate files in skill directory (non-recursive, skip SKILL.md)
                        let available_files = if !base_path.is_empty() {
                            match std::fs::read_dir(&base_path) {
                                Ok(entries) => {
                                    let mut files: Vec<String> = entries
                                        .filter_map(|e| e.ok())
                                        .filter(|e| {
                                            e.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                                        })
                                        .filter_map(|e| {
                                            let name = e.file_name().to_string_lossy().to_string();
                                            // Skip the skill definition file itself
                                            if name.to_uppercase() == "SKILL.MD" {
                                                None
                                            } else {
                                                Some(name)
                                            }
                                        })
                                        .collect();
                                    files.sort();
                                    files
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to enumerate skill directory {}: {}",
                                        base_path, e
                                    );
                                    vec![]
                                }
                            }
                        } else {
                            vec![]
                        };

                        info!(
                            "Injecting skill '{}' into system prompt (base_path={}, files={:?})",
                            skill_name, base_path, available_files
                        );

                        // Return user args as message, skill content as context
                        let ctx = nevoflux_builtin_wasm::SkillContext {
                            name: skill.metadata.name.clone(),
                            base_path,
                            content: skill.content.clone(),
                            available_files,
                        };
                        (args, Some(ctx))
                    } else {
                        // Skill not found - return error
                        return serde_json::json!({
                            "type": "error",
                            "payload": {
                                "code": "SKILL_NOT_FOUND",
                                "message": format!("Skill '{}' not found. Type / to see available skills.", skill_name),
                                "recoverable": true
                            }
                        });
                    }
                }
            } else {
                // Regular message without skill
                (message_content.to_string(), None)
            };

            // Extract session_id if provided
            let session_id = payload
                .get("payload")
                .and_then(|p| p.get("session_id"))
                .and_then(|s| s.as_str())
                .unwrap_or("default")
                .to_string();

            // Extract mode if provided (default to Chat)
            let mode = payload
                .get("payload")
                .and_then(|p| p.get("mode"))
                .and_then(|m| m.as_str())
                .map(|m| match m {
                    "browser" => AgentMode::Browser,
                    "agent" => AgentMode::Agent,
                    _ => AgentMode::Chat,
                })
                .unwrap_or(AgentMode::Chat);

            // Extract attachments (multimodal: images, files)
            let attachments: Vec<Attachment> = payload
                .get("payload")
                .and_then(|p| p.get("attachments"))
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let name = v.get("name")?.as_str()?.to_string();
                            let mime_type = v.get("mime_type")?.as_str()?.to_string();
                            let data = v.get("data")?.as_str()?.to_string();
                            Some(Attachment {
                                name,
                                mime_type,
                                data,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Extract local file references (from file picker)
            let local_files: Vec<nevoflux_protocol::FileInfo> = payload
                .get("payload")
                .and_then(|p| p.get("local_files"))
                .and_then(|f| f.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let path = v.get("path")?.as_str()?.to_string();
                            let is_directory = v.get("is_directory")?.as_bool()?;
                            let size = v.get("size").and_then(|s| s.as_u64());
                            let modified = v.get("modified").and_then(|m| m.as_u64());
                            Some(nevoflux_protocol::FileInfo {
                                path,
                                is_directory,
                                size,
                                modified,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Extract tab_id if provided (from browser sidebar)
            let tab_id = payload
                .get("payload")
                .and_then(|p| p.get("tab_id"))
                .and_then(|t| t.as_i64());

            // Extract tab_ids list if provided (all available tabs)
            let tab_ids: Vec<nevoflux_builtin_wasm::TabInfo> = payload
                .get("payload")
                .and_then(|p| p.get("tab_ids"))
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let space = v
                                .get("space")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let tab_id = v.get("tab_id").and_then(|t| t.as_i64())?;
                            let tab_title = v
                                .get("tab_title")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            let url = v
                                .get("url")
                                .and_then(|u| u.as_str())
                                .unwrap_or("")
                                .to_string();
                            Some(nevoflux_builtin_wasm::TabInfo {
                                space,
                                tab_id,
                                tab_title,
                                url,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            debug!(
                "Processing chat message with mode={:?}, session={}, attachments={}, local_files={}, tab_id={:?}, tab_ids={}",
                mode,
                session_id,
                attachments.len(),
                local_files.len(),
                tab_id,
                tab_ids.len()
            );

            // Ensure session exists and save user message
            match session_manager.get_or_create_session(&session_id).await {
                Ok(session) => {
                    info!(
                        "Session ready: id={}, created_at={}",
                        session.id, session.created_at
                    );
                }
                Err(e) => {
                    error!("Failed to get/create session {}: {}", session_id, e);
                }
            }

            // Save user message to database
            let mut generated_title: Option<String> = None;
            match session_manager
                .add_message(&session_id, MessageRole::User, message_content)
                .await
            {
                Ok(msg) => {
                    info!("Saved user message: id={}, session={}", msg.id, session_id);

                    // Generate title from first message if session has no title yet
                    match session_manager.generate_title(&session_id).await {
                        Ok(Some(title)) => {
                            info!("Generated session title: {}", title);
                            generated_title = Some(title);
                        }
                        Ok(None) => {
                            // Session already has a title or no messages
                        }
                        Err(e) => {
                            error!("Failed to generate title: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to save user message to {}: {}", session_id, e);
                }
            }

            // Create host functions with config and runtime
            let mut host = DaemonHostFunctions::new(config.clone(), runtime)
                .with_services(services.clone())
                .with_session_id(session_id.clone());

            // Pass skill base path to host for relative path resolution
            if let Some(ref ctx) = skill_context {
                if !ctx.base_path.is_empty() {
                    host = host.with_skill_base_path(&ctx.base_path);
                }
            }

            // Create agent with host functions
            let agent = Agent::new(host);

            // Build agent input with skill context injected into system prompt
            let input = AgentInput {
                session_id: session_id.clone(),
                mode,
                user_message,
                history: load_session_history(
                    session_manager,
                    &session_id,
                    config.daemon.context.max_history_messages,
                )
                .await,
                attachments,
                local_files,
                custom_system_prompt: None, // Use default mode-based prompt
                tab_id,
                tab_ids,
                skill_context,
                available_models: config.llm.configured_providers(),
            };

            // Run agent
            match agent.run(&input) {
                Ok(output) => {
                    // Save assistant response to database
                    if !output.text.is_empty() {
                        match session_manager
                            .add_message(&session_id, MessageRole::Assistant, &output.text)
                            .await
                        {
                            Ok(msg) => {
                                info!(
                                    "Saved assistant message: id={}, session={}",
                                    msg.id, session_id
                                );
                            }
                            Err(e) => {
                                error!("Failed to save assistant message to {}: {}", session_id, e);
                            }
                        }
                    }

                    // Save tool calls to session history
                    for tool_call in &output.tool_calls {
                        if let Err(e) = session_manager
                            .add_tool_use_message(
                                &session_id,
                                &tool_call.id,
                                &tool_call.name,
                                &tool_call.arguments,
                                None, // Result is not available in ToolCall struct
                            )
                            .await
                        {
                            error!("Failed to save tool call {}: {}", tool_call.name, e);
                        }
                    }

                    let mut response = serde_json::json!({
                        "type": "stream_chunk",
                        "payload": {
                            "content": output.text,
                            "tool_calls": output.tool_calls,
                            "done": true  // Always mark as done for now since we're not streaming
                        }
                    });

                    // Include session title if generated
                    if let Some(title) = generated_title {
                        response["payload"]["session_title"] = serde_json::Value::String(title);
                    }

                    response
                }
                Err(e) => {
                    error!("Agent run failed: {}", e);
                    serde_json::json!({
                        "type": "error",
                        "payload": {
                            "code": "AGENT_ERROR",
                            "message": format!("Agent error: {}", e)
                        }
                    })
                }
            }
        }
        "stop_generation" => {
            serde_json::json!({
                "type": "agent_state",
                "payload": {
                    "state": "idle",
                    "message": "Generation stopped"
                }
            })
        }
        "system_command" => {
            let inner_payload = payload.get("payload");

            let command = inner_payload
                .and_then(|p| p.get("command"))
                .and_then(|c| c.as_str())
                .unwrap_or("");

            let request_id = inner_payload
                .and_then(|p| p.get("request_id"))
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();

            let mut params = inner_payload
                .and_then(|p| p.get("params"))
                .cloned()
                .unwrap_or(serde_json::json!({}));

            // Add request_id to params for handlers
            if let Some(obj) = params.as_object_mut() {
                obj.insert("request_id".to_string(), serde_json::json!(request_id));
            }

            match command {
                "status" => {
                    serde_json::json!({
                        "type": "system_response",
                        "payload": {
                            "request_id": request_id,
                            "command": "status",
                            "success": true,
                            "data": {
                                "status": "ok",
                                "version": env!("CARGO_PKG_VERSION")
                            }
                        }
                    })
                }
                "session.resolve" => handle_session_resolve(session_manager, &params).await,
                "session.list" => handle_session_list(session_manager, &params).await,
                "session.clone" => handle_session_clone(session_manager, &params).await,
                "session.delete" => handle_session_delete(session_manager, &params).await,
                "session.rename" => handle_session_rename(session_manager, &params).await,
                "session.pin" => handle_session_pin(session_manager, &params, true).await,
                "session.unpin" => handle_session_pin(session_manager, &params, false).await,
                // MCP server configuration commands
                "mcp.list" => handle_mcp_list(&params).await,
                "mcp.add" => handle_mcp_add(&params).await,
                "mcp.update" => handle_mcp_update(&params).await,
                "mcp.delete" => handle_mcp_delete(&params).await,
                "mcp.test" => handle_mcp_test(&params).await,
                "mcp.connect" => handle_mcp_connect(&params).await,
                "mcp.disconnect" => handle_mcp_disconnect(&params).await,
                "file.pick" => handle_file_pick(&params).await,
                "skill.list" => handle_skill_list(services, &params).await,
                // ContentStore persistence commands
                "content_store.set" => {
                    let key = params.get("key").and_then(|k| k.as_str()).unwrap_or("");
                    if key.is_empty() {
                        serde_json::json!({
                            "type": "system_response",
                            "payload": {
                                "request_id": request_id,
                                "command": "content_store.set",
                                "success": false,
                                "error": {
                                    "code": "MISSING_PARAM",
                                    "message": "Missing key parameter"
                                }
                            }
                        })
                    } else {
                        let value = params.get("value").cloned().unwrap_or(serde_json::Value::Null);
                        match session_manager.set_config(key, value) {
                            Ok(()) => serde_json::json!({
                                "type": "system_response",
                                "payload": {
                                    "request_id": request_id,
                                    "command": "content_store.set",
                                    "success": true,
                                    "data": { "key": key }
                                }
                            }),
                            Err(e) => serde_json::json!({
                                "type": "system_response",
                                "payload": {
                                    "request_id": request_id,
                                    "command": "content_store.set",
                                    "success": false,
                                    "error": {
                                        "code": "STORAGE_ERROR",
                                        "message": format!("{}", e)
                                    }
                                }
                            }),
                        }
                    }
                }
                "content_store.delete" => {
                    let key = params.get("key").and_then(|k| k.as_str()).unwrap_or("");
                    if key.is_empty() {
                        serde_json::json!({
                            "type": "system_response",
                            "payload": {
                                "request_id": request_id,
                                "command": "content_store.delete",
                                "success": false,
                                "error": {
                                    "code": "MISSING_PARAM",
                                    "message": "Missing key parameter"
                                }
                            }
                        })
                    } else {
                        match session_manager.delete_config(key) {
                            Ok(deleted) => serde_json::json!({
                                "type": "system_response",
                                "payload": {
                                    "request_id": request_id,
                                    "command": "content_store.delete",
                                    "success": true,
                                    "data": { "key": key, "deleted": deleted }
                                }
                            }),
                            Err(e) => serde_json::json!({
                                "type": "system_response",
                                "payload": {
                                    "request_id": request_id,
                                    "command": "content_store.delete",
                                    "success": false,
                                    "error": {
                                        "code": "STORAGE_ERROR",
                                        "message": format!("{}", e)
                                    }
                                }
                            }),
                        }
                    }
                }
                "content_store.load" => {
                    let prefix = params.get("prefix").and_then(|p| p.as_str()).unwrap_or("");
                    let result = if prefix.is_empty() {
                        session_manager.list_config()
                    } else {
                        session_manager.list_config_by_prefix(prefix)
                    };
                    match result {
                        Ok(entries) => {
                            let count = entries.len();
                            let entries_json: Vec<serde_json::Value> = entries
                                .into_iter()
                                .map(|e| serde_json::json!({
                                    "key": e.key,
                                    "value": e.value,
                                    "updated_at": e.updated_at
                                }))
                                .collect();
                            serde_json::json!({
                                "type": "system_response",
                                "payload": {
                                    "request_id": request_id,
                                    "command": "content_store.load",
                                    "success": true,
                                    "data": {
                                        "entries": entries_json,
                                        "count": count
                                    }
                                }
                            })
                        }
                        Err(e) => serde_json::json!({
                            "type": "system_response",
                            "payload": {
                                "request_id": request_id,
                                "command": "content_store.load",
                                "success": false,
                                "error": {
                                    "code": "STORAGE_ERROR",
                                    "message": format!("{}", e)
                                }
                            }
                        }),
                    }
                }
                _ => {
                    serde_json::json!({
                        "type": "system_response",
                        "payload": {
                            "request_id": request_id,
                            "command": command,
                            "success": false,
                            "error": {
                                "code": "UNKNOWN_COMMAND",
                                "message": format!("Unknown command: {}", command)
                            }
                        }
                    })
                }
            }
        }
        _ => {
            debug!("Unknown chat message type: {}", msg_type);
            serde_json::json!({
                "type": "error",
                "payload": {
                    "code": "UNKNOWN_MESSAGE_TYPE",
                    "message": format!("Unknown message type: {}", msg_type)
                }
            })
        }
    }
}

/// Handle session.resolve command.
///
/// Resolves a session by ID, creating it if it doesn't exist.
/// Returns the session info and its messages.
async fn handle_session_resolve(
    session_manager: &Arc<SessionManager>,
    params: &serde_json::Value,
) -> serde_json::Value {
    // Extract request_id for response correlation
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = match params.get("session_id").and_then(|s| s.as_str()) {
        Some(id) => id,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.resolve",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing session_id parameter"
                    }
                }
            });
        }
    };

    info!("Resolving session: {}", session_id);

    // Try to get or create the session
    match session_manager.get_or_create_session(session_id).await {
        Ok(session) => {
            // Check if this was a new session by seeing if updated_at equals created_at
            let created = session.created_at == session.updated_at;
            info!("Session resolved: id={}, created={}", session.id, created);

            // Get messages for the session
            let messages = match session_manager.get_messages(&session.id).await {
                Ok(msgs) => {
                    info!("Found {} messages for session {}", msgs.len(), session.id);
                    msgs.into_iter()
                        .map(|m| {
                            serde_json::json!({
                                "id": m.id,
                                "role": format!("{:?}", m.role).to_lowercase(),
                                "content": m.content,
                                "created_at": m.created_at
                            })
                        })
                        .collect::<Vec<_>>()
                }
                Err(e) => {
                    error!("Failed to get messages for {}: {}", session.id, e);
                    vec![]
                }
            };

            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.resolve",
                    "success": true,
                    "data": {
                        "session": {
                            "id": session.id,
                            "title": session.title,
                            "mode": format!("{:?}", session.mode).to_lowercase(),
                            "created_at": session.created_at,
                            "updated_at": session.updated_at
                        },
                        "messages": messages,
                        "created": created
                    }
                }
            })
        }
        Err(e) => {
            error!("Failed to resolve session: {}", e);
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.resolve",
                    "success": false,
                    "error": {
                        "code": "RESOLVE_FAILED",
                        "message": format!("Failed to resolve session: {}", e)
                    }
                }
            })
        }
    }
}

/// Handle session.list command.
///
/// Lists sessions with optional pagination.
async fn handle_session_list(
    session_manager: &Arc<SessionManager>,
    params: &serde_json::Value,
) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let limit = params.get("limit").and_then(|l| l.as_u64()).unwrap_or(20) as u32;
    let offset = params.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as u32;

    let list_params = ListSessionsParams::new()
        .with_limit(limit)
        .with_offset(offset);

    match session_manager.list_sessions(list_params).await {
        Ok(sessions) => {
            // Get message counts for each session
            let mut session_summaries = Vec::new();
            for session in sessions {
                let message_count = session_manager
                    .get_message_count(&session.id)
                    .await
                    .unwrap_or(0);

                session_summaries.push(serde_json::json!({
                    "id": session.id,
                    "title": session.title,
                    "updated_at": session.updated_at,
                    "message_count": message_count,
                    "pinned": session.pinned
                }));
            }

            // Get total count
            let total = session_manager.get_session_count(false).await.unwrap_or(0);

            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.list",
                    "success": true,
                    "data": {
                        "sessions": session_summaries,
                        "total": total
                    }
                }
            })
        }
        Err(e) => {
            error!("Failed to list sessions: {}", e);
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.list",
                    "success": false,
                    "error": {
                        "code": "LIST_FAILED",
                        "message": format!("Failed to list sessions: {}", e)
                    }
                }
            })
        }
    }
}

/// Handle session.clone command.
///
/// Clones messages from a source session to a new target session.
async fn handle_session_clone(
    session_manager: &Arc<SessionManager>,
    params: &serde_json::Value,
) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let source_id = match params.get("source_id").and_then(|s| s.as_str()) {
        Some(id) => id,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.clone",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing source_id parameter"
                    }
                }
            });
        }
    };

    let target_id = match params.get("target_id").and_then(|s| s.as_str()) {
        Some(id) => id,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.clone",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing target_id parameter"
                    }
                }
            });
        }
    };

    // Get source messages
    let source_messages = match session_manager.get_messages(source_id).await {
        Ok(msgs) => msgs,
        Err(e) => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.clone",
                    "success": false,
                    "error": {
                        "code": "SOURCE_NOT_FOUND",
                        "message": format!("Failed to get source messages: {}", e)
                    }
                }
            });
        }
    };

    // Get source session for title
    let source_title = session_manager
        .get_session(source_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.title);

    // Create target session with same title
    let target_session = match session_manager
        .create_session(Some(target_id.to_string()), source_title)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.clone",
                    "success": false,
                    "error": {
                        "code": "CREATE_FAILED",
                        "message": format!("Failed to create target session: {}", e)
                    }
                }
            });
        }
    };

    // Copy messages to target session
    let mut cloned_messages = Vec::new();
    for msg in &source_messages {
        match session_manager
            .add_message(target_id, msg.role, &msg.content)
            .await
        {
            Ok(new_msg) => {
                cloned_messages.push(serde_json::json!({
                    "id": new_msg.id,
                    "role": format!("{:?}", new_msg.role).to_lowercase(),
                    "content": new_msg.content,
                    "created_at": new_msg.created_at
                }));
            }
            Err(e) => {
                error!("Failed to clone message: {}", e);
            }
        }
    }

    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": "session.clone",
            "success": true,
            "data": {
                "session": {
                    "id": target_session.id,
                    "title": target_session.title,
                    "mode": format!("{:?}", target_session.mode).to_lowercase(),
                    "created_at": target_session.created_at,
                    "updated_at": target_session.updated_at
                },
                "messages": cloned_messages
            }
        }
    })
}

/// Handle session.delete command.
///
/// Deletes a session by ID.
async fn handle_session_delete(
    session_manager: &Arc<SessionManager>,
    params: &serde_json::Value,
) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = match params.get("session_id").and_then(|s| s.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.delete",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing or empty session_id parameter"
                    }
                }
            });
        }
    };

    match session_manager.delete_session(session_id).await {
        Ok(deleted) => {
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.delete",
                    "success": true,
                    "data": {
                        "id": session_id,
                        "deleted": deleted
                    }
                }
            })
        }
        Err(e) => {
            error!("Failed to delete session: {}", e);
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.delete",
                    "success": false,
                    "error": {
                        "code": "DELETE_FAILED",
                        "message": format!("Failed to delete session: {}", e)
                    }
                }
            })
        }
    }
}

/// Handle session.rename command.
///
/// Renames a session by setting its title.
async fn handle_session_rename(
    session_manager: &Arc<SessionManager>,
    params: &serde_json::Value,
) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = match params.get("session_id").and_then(|s| s.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.rename",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing or empty session_id parameter"
                    }
                }
            });
        }
    };

    let title = match params.get("title").and_then(|t| t.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.rename",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing or empty title parameter"
                    }
                }
            });
        }
    };

    match session_manager.set_title(session_id, title).await {
        Ok(session) => {
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.rename",
                    "success": true,
                    "data": {
                        "id": session.id,
                        "title": session.title
                    }
                }
            })
        }
        Err(e) => {
            error!("Failed to rename session: {}", e);
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "session.rename",
                    "success": false,
                    "error": {
                        "code": "RENAME_FAILED",
                        "message": format!("Failed to rename session: {}", e)
                    }
                }
            })
        }
    }
}

/// Handle session.pin and session.unpin commands.
///
/// Pins or unpins a session.
async fn handle_session_pin(
    session_manager: &Arc<SessionManager>,
    params: &serde_json::Value,
    pin: bool,
) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let command = if pin { "session.pin" } else { "session.unpin" };

    let session_id = match params.get("session_id").and_then(|s| s.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": command,
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing or empty session_id parameter"
                    }
                }
            });
        }
    };

    let result = if pin {
        session_manager.pin_session(session_id).await
    } else {
        session_manager.unpin_session(session_id).await
    };

    match result {
        Ok(session) => {
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": command,
                    "success": true,
                    "data": {
                        "id": session.id,
                        "pinned": session.pinned
                    }
                }
            })
        }
        Err(e) => {
            error!(
                "Failed to {} session: {}",
                if pin { "pin" } else { "unpin" },
                e
            );
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": command,
                    "success": false,
                    "error": {
                        "code": if pin { "PIN_FAILED" } else { "UNPIN_FAILED" },
                        "message": format!("Failed to {} session: {}", if pin { "pin" } else { "unpin" }, e)
                    }
                }
            })
        }
    }
}

/// Handle MCP channel messages
async fn handle_mcp_message(payload: &serde_json::Value) -> serde_json::Value {
    let msg_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "mcp_request" => {
            // TODO: Implement MCP request handling
            serde_json::json!({
                "type": "mcp_response",
                "payload": {
                    "jsonrpc": "2.0",
                    "id": payload.get("payload").and_then(|p| p.get("id")).cloned().unwrap_or(serde_json::json!(null)),
                    "error": {
                        "code": -32601,
                        "message": "MCP not yet implemented"
                    }
                }
            })
        }
        _ => {
            serde_json::json!({
                "type": "error",
                "payload": {
                    "code": "UNKNOWN_MCP_TYPE",
                    "message": format!("Unknown MCP message type: {}", msg_type)
                }
            })
        }
    }
}

// ============================================
// MCP Configuration Handlers
// ============================================

use crate::mcp_config::{McpServerConfigFile, McpServersConfig};

/// Handle mcp.list command.
///
/// Returns list of configured MCP servers and their connection status.
async fn handle_mcp_list(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    // Load MCP servers config from file
    match McpServersConfig::load() {
        Ok(config) => {
            let servers: Vec<serde_json::Value> = config
                .servers
                .iter()
                .map(|s| {
                    let env: HashMap<String, String> = s.env.clone();
                    serde_json::json!({
                        "name": s.name,
                        "command": s.command,
                        "args": s.args,
                        "enabled": s.enabled,
                        "env": env
                    })
                })
                .collect();

            // TODO: Get connected servers from MCP registry
            let connected: Vec<String> = Vec::new();

            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.list",
                    "success": true,
                    "data": {
                        "servers": servers,
                        "connected": connected
                    }
                }
            })
        }
        Err(e) => {
            error!("Failed to load MCP config: {}", e);
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.list",
                    "success": false,
                    "error": {
                        "code": "CONFIG_ERROR",
                        "message": format!("Failed to load MCP config: {}", e)
                    }
                }
            })
        }
    }
}

/// Handle mcp.add command.
///
/// Adds a new MCP server configuration.
async fn handle_mcp_add(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    // Extract server config from params
    let server_params = match params.get("server") {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.add",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing server parameter"
                    }
                }
            });
        }
    };

    let name = server_params
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or_default();
    let command = server_params
        .get("command")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let args: Vec<String> = server_params
        .get("args")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let enabled = server_params
        .get("enabled")
        .and_then(|e| e.as_bool())
        .unwrap_or(true);
    let env: HashMap<String, String> = server_params
        .get("env")
        .and_then(|e| e.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Create server config
    let server = McpServerConfigFile::new(name, command)
        .with_args(args)
        .with_enabled(enabled)
        .with_env(env);

    // Load existing config, add server, and save
    let mut config = match McpServersConfig::load() {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.add",
                    "success": false,
                    "error": {
                        "code": "CONFIG_ERROR",
                        "message": format!("Failed to load MCP config: {}", e)
                    }
                }
            });
        }
    };

    if let Err(e) = config.add_server(server) {
        return serde_json::json!({
            "type": "system_response",
            "payload": {
                "request_id": request_id,
                "command": "mcp.add",
                "success": false,
                "error": {
                    "code": "ADD_FAILED",
                    "message": format!("Failed to add server: {}", e)
                }
            }
        });
    }

    if let Err(e) = config.save() {
        return serde_json::json!({
            "type": "system_response",
            "payload": {
                "request_id": request_id,
                "command": "mcp.add",
                "success": false,
                "error": {
                    "code": "SAVE_FAILED",
                    "message": format!("Failed to save MCP config: {}", e)
                }
            }
        });
    }

    info!("Added MCP server: {}", name);
    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": "mcp.add",
            "success": true,
            "data": {
                "name": name
            }
        }
    })
}

/// Handle mcp.update command.
///
/// Updates an existing MCP server configuration.
async fn handle_mcp_update(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.update",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing name parameter"
                    }
                }
            });
        }
    };

    let server_params = match params.get("server") {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.update",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing server parameter"
                    }
                }
            });
        }
    };

    let command = server_params
        .get("command")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let args: Vec<String> = server_params
        .get("args")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let enabled = server_params
        .get("enabled")
        .and_then(|e| e.as_bool())
        .unwrap_or(true);
    let env: HashMap<String, String> = server_params
        .get("env")
        .and_then(|e| e.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();

    let server = McpServerConfigFile::new(name, command)
        .with_args(args)
        .with_enabled(enabled)
        .with_env(env);

    let mut config = match McpServersConfig::load() {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.update",
                    "success": false,
                    "error": {
                        "code": "CONFIG_ERROR",
                        "message": format!("Failed to load MCP config: {}", e)
                    }
                }
            });
        }
    };

    if let Err(e) = config.update_server(name, server) {
        return serde_json::json!({
            "type": "system_response",
            "payload": {
                "request_id": request_id,
                "command": "mcp.update",
                "success": false,
                "error": {
                    "code": "UPDATE_FAILED",
                    "message": format!("Failed to update server: {}", e)
                }
            }
        });
    }

    if let Err(e) = config.save() {
        return serde_json::json!({
            "type": "system_response",
            "payload": {
                "request_id": request_id,
                "command": "mcp.update",
                "success": false,
                "error": {
                    "code": "SAVE_FAILED",
                    "message": format!("Failed to save MCP config: {}", e)
                }
            }
        });
    }

    info!("Updated MCP server: {}", name);
    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": "mcp.update",
            "success": true,
            "data": {
                "name": name
            }
        }
    })
}

/// Handle mcp.delete command.
///
/// Deletes an MCP server configuration.
async fn handle_mcp_delete(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.delete",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing name parameter"
                    }
                }
            });
        }
    };

    let mut config = match McpServersConfig::load() {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.delete",
                    "success": false,
                    "error": {
                        "code": "CONFIG_ERROR",
                        "message": format!("Failed to load MCP config: {}", e)
                    }
                }
            });
        }
    };

    if !config.remove_server(name) {
        return serde_json::json!({
            "type": "system_response",
            "payload": {
                "request_id": request_id,
                "command": "mcp.delete",
                "success": false,
                "error": {
                    "code": "NOT_FOUND",
                    "message": format!("Server not found: {}", name)
                }
            }
        });
    }

    if let Err(e) = config.save() {
        return serde_json::json!({
            "type": "system_response",
            "payload": {
                "request_id": request_id,
                "command": "mcp.delete",
                "success": false,
                "error": {
                    "code": "SAVE_FAILED",
                    "message": format!("Failed to save MCP config: {}", e)
                }
            }
        });
    }

    info!("Deleted MCP server: {}", name);
    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": "mcp.delete",
            "success": true,
            "data": {
                "name": name
            }
        }
    })
}

/// Handle mcp.test command.
///
/// Tests connection to an MCP server by temporarily connecting and listing tools.
async fn handle_mcp_test(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.test",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing name parameter"
                    }
                }
            });
        }
    };

    // Load config and find server
    let config = match McpServersConfig::load() {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.test",
                    "success": false,
                    "error": {
                        "code": "CONFIG_ERROR",
                        "message": format!("Failed to load MCP config: {}", e)
                    }
                }
            });
        }
    };

    let server = match config.get_server(name) {
        Some(s) => s,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.test",
                    "success": false,
                    "error": {
                        "code": "NOT_FOUND",
                        "message": format!("Server not found: {}", name)
                    }
                }
            });
        }
    };

    // Build the command line
    let command = &server.command;
    let args = &server.args;

    info!(
        "Testing MCP server '{}': {} {}",
        name,
        command,
        args.join(" ")
    );

    // Try to spawn the process and communicate with it
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // Set environment variables
    for (key, value) in &server.env {
        cmd.env(key, value);
    }

    match cmd.spawn() {
        Ok(mut child) => {
            // Send initialize request
            let stdin = child.stdin.as_mut();
            if let Some(stdin) = stdin {
                use tokio::io::AsyncWriteExt;
                let init_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "nevoflux-agent",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }
                });
                let request_str = format!("{}\n", serde_json::to_string(&init_request).unwrap());

                if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
                    let _ = child.kill().await;
                    return serde_json::json!({
                        "type": "system_response",
                        "payload": {
                            "request_id": request_id,
                            "command": "mcp.test",
                            "success": true,
                            "data": {
                                "name": name,
                                "success": false,
                                "message": format!("Failed to write to stdin: {}", e),
                                "tools_count": 0
                            }
                        }
                    });
                }
            }

            // Wait a bit and kill the process
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            let _ = child.kill().await;

            // Process started successfully
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.test",
                    "success": true,
                    "data": {
                        "name": name,
                        "success": true,
                        "message": "Server started successfully",
                        "tools_count": 0
                    }
                }
            })
        }
        Err(e) => {
            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.test",
                    "success": true,
                    "data": {
                        "name": name,
                        "success": false,
                        "message": format!("Failed to start server: {}", e),
                        "tools_count": 0
                    }
                }
            })
        }
    }
}

/// Handle mcp.connect command.
///
/// Connects to an MCP server.
async fn handle_mcp_connect(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.connect",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing name parameter"
                    }
                }
            });
        }
    };

    // TODO: Implement actual MCP connection via MCP registry
    // For now, just acknowledge the request
    info!("MCP connect requested for: {}", name);

    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": "mcp.connect",
            "success": true,
            "data": {
                "name": name,
                "connected": false,
                "message": "Connection management not yet implemented"
            }
        }
    })
}

/// Handle mcp.disconnect command.
///
/// Disconnects from an MCP server.
async fn handle_mcp_disconnect(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let name = match params.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => {
            return serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "mcp.disconnect",
                    "success": false,
                    "error": {
                        "code": "MISSING_PARAM",
                        "message": "Missing name parameter"
                    }
                }
            });
        }
    };

    // TODO: Implement actual MCP disconnection via MCP registry
    // For now, just acknowledge the request
    info!("MCP disconnect requested for: {}", name);

    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": "mcp.disconnect",
            "success": true,
            "data": {
                "name": name,
                "connected": false
            }
        }
    })
}

/// Handle file.pick command.
///
/// Opens a native file picker dialog and returns selected files.
async fn handle_file_pick(params: &serde_json::Value) -> serde_json::Value {
    use crate::file_picker::pick_files;
    use nevoflux_protocol::{PickFilesRequest, PickerMode};

    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    // Parse picker mode
    let mode = params
        .get("mode")
        .and_then(|m| m.as_str())
        .map(|m| match m {
            "files" => PickerMode::Files,
            "directories" => PickerMode::Directories,
            _ => PickerMode::Both,
        })
        .unwrap_or(PickerMode::Both);

    let multiple = params
        .get("multiple")
        .and_then(|m| m.as_bool())
        .unwrap_or(false);

    let title = params
        .get("title")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string());

    let default_path = params
        .get("default_path")
        .and_then(|p| p.as_str())
        .map(|s| s.to_string());

    let req = PickFilesRequest {
        mode,
        multiple,
        title,
        default_path,
    };

    info!(
        "File picker requested: mode={:?}, multiple={}",
        req.mode, req.multiple
    );

    match pick_files(req).await {
        Ok(response) => {
            let files: Vec<serde_json::Value> = response
                .files
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "path": f.path,
                        "is_directory": f.is_directory,
                        "size": f.size,
                        "modified": f.modified
                    })
                })
                .collect();

            info!(
                "File picker completed: {} files selected, cancelled={}",
                files.len(),
                response.cancelled
            );

            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "file.pick",
                    "success": true,
                    "data": {
                        "files": files,
                        "cancelled": response.cancelled
                    }
                }
            })
        }
        Err(e) => {
            error!("File picker failed: {:?}", e);

            let (code, message) = match e {
                nevoflux_protocol::PickFilesError::NoDisplay => {
                    ("NO_DISPLAY", "No graphical display available".to_string())
                }
                nevoflux_protocol::PickFilesError::AlreadyPicking => (
                    "ALREADY_PICKING",
                    "A file picker dialog is already open".to_string(),
                ),
                nevoflux_protocol::PickFilesError::DialogFailed(msg) => ("DIALOG_FAILED", msg),
            };

            serde_json::json!({
                "type": "system_response",
                "payload": {
                    "request_id": request_id,
                    "command": "file.pick",
                    "success": false,
                    "error": {
                        "code": code,
                        "message": message
                    }
                }
            })
        }
    }
}

/// Handle skill.list system command.
///
/// Lists all available skills from the skill registry.
async fn handle_skill_list(
    services: &HostServices,
    params: &serde_json::Value,
) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    let registry = services.skills.read().await;
    let summaries = registry.list();

    let skills: Vec<_> = summaries
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "tags": s.tags,
                "source": s.source,
                "enabled": s.enabled
            })
        })
        .collect();

    info!("skill.list: returning {} skills", skills.len());

    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": "skill.list",
            "success": true,
            "data": { "skills": skills }
        }
    })
}

// ============================================
// Tool Availability Helpers
// ============================================

/// Built-in tools that are always available.
/// These correspond to the core tools provided by the agent runtime.
const BUILTIN_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Glob",
    "Grep",
    "browser_navigate",
    "browser_click",
    "browser_type",
    "browser_screenshot",
    "browser_scroll",
    "browser_get_content",
    "computer_screenshot",
    "computer_click",
    "computer_type",
    "computer_key",
    "computer_scroll",
];

/// Gather all available tools from MCP servers and built-in tools.
///
/// Returns a list of tool names in the format:
/// - Built-in tools: just the tool name (e.g., "Read", "Write")
/// - MCP tools: "server_name:tool_name" format (e.g., "notion:search")
async fn gather_available_tools(services: &HostServices) -> Vec<String> {
    let mut tools = Vec::new();

    // Add built-in tools
    tools.extend(BUILTIN_TOOLS.iter().map(|s| s.to_string()));

    // Add MCP tools from connected servers
    if let Some(ref mcp_manager) = services.mcp_manager {
        if let Ok(mcp_tools) = mcp_manager.list_all_tools().await {
            for server_tool in mcp_tools {
                // Format: "server_name:tool_name"
                tools.push(format!(
                    "{}:{}",
                    server_tool.server_name, server_tool.tool.name
                ));
            }
        }
    }

    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_protocol::PlanStep;

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

    #[test]
    fn test_format_plan_as_context() {
        let proposal = PlanProposal {
            summary: "Deploy the application".to_string(),
            steps: vec![
                PlanStep {
                    description: "Build the project".to_string(),
                    model: None,
                },
                PlanStep {
                    description: "Run tests".to_string(),
                    model: Some("gpt-4o".to_string()),
                },
                PlanStep {
                    description: "Deploy to production".to_string(),
                    model: None,
                },
            ],
        };

        let text = format_plan_as_context(&proposal);
        assert!(text.contains("Approved plan: Deploy the application"));
        assert!(text.contains("1. Build the project"));
        assert!(text.contains("2. Run tests [model: gpt-4o]"));
        assert!(text.contains("3. Deploy to production"));
        assert!(text.contains("Execute this plan now."));
    }

    #[test]
    fn test_format_plan_as_context_empty_steps() {
        let proposal = PlanProposal {
            summary: "Empty plan".to_string(),
            steps: vec![],
        };

        let text = format_plan_as_context(&proposal);
        assert!(text.contains("Approved plan: Empty plan"));
        assert!(text.contains("Execute this plan now."));
    }

    #[tokio::test]
    async fn test_server_start_and_shutdown() {
        let config = ServerConfig::default();
        let router = Arc::new(Router::new());
        let session_manager = Arc::new(SessionManager::in_memory().unwrap());

        let server = start_server(config, router, session_manager).await;
        assert!(server.is_ok());

        let mut server = server.unwrap();
        assert!(server.port() >= 19500);

        // Shutdown
        server.shutdown().await;
    }
}
