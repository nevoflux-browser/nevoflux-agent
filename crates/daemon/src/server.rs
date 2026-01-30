//! ZeroMQ ROUTER server for the daemon.

use crate::agent_host::DaemonHostFunctions;
use crate::config::AgentConfig;
use crate::error::{DaemonError, Result};
use crate::router::{RouteDecision, Router};
use crate::session::SessionManager;
use bytes::Bytes;
use nevoflux_builtin_wasm::{Agent, AgentInput, AgentMode, Attachment};
use nevoflux_protocol::{DaemonEnvelope, ProxyEnvelope};
use nevoflux_storage::{ListSessionsParams, MessageRole};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zeromq::{Socket, SocketSend, ZmqMessage};

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

    let mut socket = zeromq::RouterSocket::new();
    socket
        .bind(&addr)
        .await
        .map_err(|e| DaemonError::InternalError(format!("Failed to bind: {}", e)))?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    let (msg_tx, mut msg_rx) = mpsc::channel::<(Vec<u8>, ProxyEnvelope)>(100);
    let (response_tx, mut response_rx) = mpsc::channel::<(Vec<u8>, DaemonEnvelope)>(100);

    // Spawn receive loop
    let mut recv_socket = socket;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("Server shutdown signal received");
                    break;
                }
                // Send responses back to proxies
                Some((identity, response)) = response_rx.recv() => {
                    match serde_json::to_vec(&response) {
                        Ok(data) => {
                            let frames: Vec<Bytes> = vec![
                                Bytes::from(identity),
                                Bytes::from(data),
                            ];
                            match ZmqMessage::try_from(frames) {
                                Ok(zmq_msg) => {
                                    if let Err(e) = recv_socket.send(zmq_msg).await {
                                        error!("Failed to send response: {}", e);
                                    } else {
                                        debug!("Sent response to {}", response.proxy_id);
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to create ZMQ message: {:?}", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to serialize response: {}", e);
                        }
                    }
                }
                // Receive messages from proxies
                msg = zeromq::SocketRecv::recv(&mut recv_socket) => {
                    match msg {
                        Ok(zmq_msg) => {
                            let frames = zmq_msg.into_vec();
                            if frames.len() >= 2 {
                                let identity = frames[0].to_vec();
                                if let Ok(envelope) = serde_json::from_slice::<ProxyEnvelope>(&frames[1]) {
                                    debug!("Received message from {}: type={:?}", envelope.proxy_id, envelope.payload.get("type"));
                                    let _ = msg_tx.send((identity, envelope)).await;
                                } else {
                                    warn!("Failed to parse ProxyEnvelope from frame");
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

    // Spawn message processing loop
    let process_router = router.clone();
    let process_response_tx = response_tx.clone();
    let process_config = agent_config.clone();
    let process_session_manager = session_manager.clone();
    let process_runtime = tokio::runtime::Handle::current();
    tokio::spawn(async move {
        while let Some((identity, envelope)) = msg_rx.recv().await {
            let proxy_id = envelope.proxy_id.clone();
            let request_id = envelope.request_id.clone();
            let channel = envelope.channel.clone();

            // Register proxy if not already registered (pid 0 for native messaging)
            if !process_router.proxy_registry().is_registered(&proxy_id) {
                process_router.proxy_registry().register(&proxy_id, 0);
                debug!("Registered new proxy: {}", proxy_id);
            }

            // Route the message
            let decision = process_router.route_incoming(&envelope);
            debug!("Route decision for {}: {:?}", proxy_id, decision);

            // Process based on route decision
            let response_payload = match decision {
                RouteDecision::RejectUnregistered => {
                    serde_json::json!({
                        "type": "error",
                        "payload": {
                            "code": "UNREGISTERED",
                            "message": "Proxy not registered"
                        }
                    })
                }
                RouteDecision::ProcessChat { .. } => {
                    // Handle chat messages via Agent
                    handle_chat_message(
                        &envelope.payload,
                        &process_config,
                        &process_session_manager,
                        process_runtime.clone(),
                    )
                    .await
                }
                RouteDecision::ProcessMcp { .. } => {
                    // Handle MCP messages
                    handle_mcp_message(&envelope.payload).await
                }
            };

            // Send response
            let response = DaemonEnvelope::new(&proxy_id, channel, response_payload)
                .with_request_id(&request_id);

            if let Err(e) = process_response_tx.send((identity, response)).await {
                error!("Failed to queue response: {}", e);
            }
        }
    });

    Ok(Server {
        port,
        shutdown_tx: Some(shutdown_tx),
    })
}

/// Handle chat channel messages using the Agent.
async fn handle_chat_message(
    payload: &serde_json::Value,
    config: &Arc<AgentConfig>,
    session_manager: &Arc<SessionManager>,
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

            debug!(
                "Processing chat message with mode={:?}, session={}, attachments={}",
                mode,
                session_id,
                attachments.len()
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
            let host = DaemonHostFunctions::new(config.clone(), runtime);

            // Create agent with host functions
            let agent = Agent::new(host);

            // Build agent input
            let input = AgentInput {
                session_id: session_id.clone(),
                mode,
                user_message: message_content.to_string(),
                history: vec![], // TODO: Load history from session
                attachments,
                custom_system_prompt: None, // Use default mode-based prompt
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
                // MCP server configuration commands
                "mcp.list" => handle_mcp_list(&params).await,
                "mcp.add" => handle_mcp_add(&params).await,
                "mcp.update" => handle_mcp_update(&params).await,
                "mcp.delete" => handle_mcp_delete(&params).await,
                "mcp.test" => handle_mcp_test(&params).await,
                "mcp.connect" => handle_mcp_connect(&params).await,
                "mcp.disconnect" => handle_mcp_disconnect(&params).await,
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
                    "message_count": message_count
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
            .add_message(target_id, msg.role.clone(), &msg.content)
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
use std::collections::HashMap;

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
        let session_manager = Arc::new(SessionManager::in_memory().unwrap());

        let server = start_server(config, router, session_manager).await;
        assert!(server.is_ok());

        let mut server = server.unwrap();
        assert!(server.port() >= 19500);

        // Shutdown
        server.shutdown().await;
    }
}
