//! ACP (Agent Communication Protocol) provider.
//!
//! Communicates with CLI agents (claude-code, gemini-cli) over stdio using the
//! sacp protocol. A background tokio task owns the `ClientToAgent` connection;
//! the public `AcpProvider` sends requests through an `mpsc` channel.

pub mod claude;
pub mod context;
pub mod gemini;

use anyhow::{Context as _, Result};
// Re-export key schema types so downstream crates (e.g. nevoflux-daemon)
// can construct ContentBlock values without a direct sacp dependency.
pub use sacp::schema::{ContentBlock, TextContent};

use sacp::schema::{
    ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, SetSessionModeRequest, StopReason,
};
use sacp::{ClientToAgent, JrConnectionCx};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for spawning an ACP agent process.
#[derive(Clone, Debug)]
pub struct AcpProviderConfig {
    /// Path to the CLI executable (e.g. `claude` or `gemini`).
    pub command: PathBuf,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Extra environment variables to set.
    pub env: Vec<(String, String)>,
    /// Environment variables to remove before spawning.
    pub env_remove: Vec<String>,
    /// Working directory for the agent session.
    pub work_dir: PathBuf,
    /// Session mode to request (e.g. "plan", "code").
    pub session_mode: String,
}

/// Internal request sent from `AcpProvider` to the background client loop.
pub(crate) enum ClientRequest {
    NewSession {
        response_tx: oneshot::Sender<Result<NewSessionResponse>>,
    },
    Prompt {
        session_id: SessionId,
        content: Vec<ContentBlock>,
        response_tx: mpsc::Sender<AcpUpdate>,
    },
    Shutdown,
}

/// Updates streamed back from the ACP agent during a prompt.
#[derive(Debug)]
pub enum AcpUpdate {
    /// Incremental text from the agent's response.
    Text(String),
    /// Incremental thought/reasoning from the agent.
    Thought(String),
    /// The agent has finished responding.
    Complete(StopReason),
    /// A protocol-level error occurred.
    Error(String),
}

/// ACP-based LLM provider that communicates with a CLI agent over stdio.
///
/// Use [`AcpProvider::new`] to create an instance, then [`AcpProvider::connect`]
/// to spawn the process and establish the protocol handshake.
pub struct AcpProvider {
    config: AcpProviderConfig,
    tx: Option<mpsc::Sender<ClientRequest>>,
}

impl AcpProvider {
    /// Create a new (disconnected) provider with the given config.
    pub fn new(config: AcpProviderConfig) -> Self {
        Self { config, tx: None }
    }

    /// Spawn the agent process and complete the ACP handshake.
    ///
    /// After this returns successfully the provider is ready for
    /// [`new_session`](Self::new_session) and [`prompt`](Self::prompt) calls.
    pub async fn connect(&mut self) -> Result<()> {
        let child = spawn_acp_process(&self.config).await?;
        let (tx, rx) = mpsc::channel(32);
        let (init_tx, init_rx) = oneshot::channel();

        let config = self.config.clone();
        tokio::spawn(async move {
            if let Err(e) = run_client_loop(config, child, rx, init_tx).await {
                tracing::error!(error = %e, "ACP client loop error");
            }
        });

        // Wait for the initialization handshake to complete.
        let _init_response = init_rx
            .await
            .context("ACP client initialization cancelled")??;

        self.tx = Some(tx);
        Ok(())
    }

    /// Whether the background client loop is still running.
    pub fn is_alive(&self) -> bool {
        self.tx.as_ref().is_some_and(|tx| !tx.is_closed())
    }

    /// Reconnect if the background client loop has died.
    pub async fn ensure_connected(&mut self) -> Result<()> {
        if !self.is_alive() {
            tracing::info!("ACP connection lost, reconnecting...");
            self.connect().await?;
        }
        Ok(())
    }

    /// Create a new session on the connected agent.
    ///
    /// Sends `session/new` followed by `session/set_mode` if the requested
    /// mode differs from the default.
    pub async fn new_session(&self) -> Result<SessionId> {
        let tx = self
            .tx
            .as_ref()
            .context("ACP provider is not connected")?;

        let (response_tx, response_rx) = oneshot::channel();
        tx.send(ClientRequest::NewSession { response_tx })
            .await
            .context("ACP client is unavailable")?;

        let session = response_rx.await.context("ACP session/new cancelled")??;
        Ok(session.session_id)
    }

    /// Send a prompt to the agent and receive streaming updates.
    ///
    /// Returns a receiver that yields [`AcpUpdate`] values. The last update
    /// will be either [`AcpUpdate::Complete`] or [`AcpUpdate::Error`].
    pub async fn prompt(
        &self,
        session_id: SessionId,
        content: Vec<ContentBlock>,
    ) -> Result<mpsc::Receiver<AcpUpdate>> {
        let tx = self
            .tx
            .as_ref()
            .context("ACP provider is not connected")?;

        let (response_tx, response_rx) = mpsc::channel(64);
        tx.send(ClientRequest::Prompt {
            session_id,
            content,
            response_tx,
        })
        .await
        .context("ACP client is unavailable")?;

        Ok(response_rx)
    }

    /// Shut down the agent process gracefully.
    pub async fn shutdown(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(ClientRequest::Shutdown).await;
        }
    }
}

impl Drop for AcpProvider {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            tokio::spawn(async move {
                let _ = tx.send(ClientRequest::Shutdown).await;
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Process spawning
// ---------------------------------------------------------------------------

/// Spawn the ACP agent as a child process with piped stdio.
async fn spawn_acp_process(config: &AcpProviderConfig) -> Result<Child> {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    for key in &config.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &config.env {
        cmd.env(key, value);
    }

    // On Windows, prevent the child from creating a visible console window.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.spawn().context("failed to spawn ACP process")
}

// ---------------------------------------------------------------------------
// Background client loop
// ---------------------------------------------------------------------------

/// Run the full ACP client lifecycle: spawn transport, build protocol client,
/// and process requests until shutdown.
async fn run_client_loop(
    config: AcpProviderConfig,
    mut child: Child,
    mut rx: mpsc::Receiver<ClientRequest>,
    init_tx: oneshot::Sender<Result<InitializeResponse>>,
) -> Result<()> {
    let stdin = child.stdin.take().context("no stdin on ACP child process")?;
    let stdout = child
        .stdout
        .take()
        .context("no stdout on ACP child process")?;

    let transport = sacp::ByteStreams::new(stdin.compat_write(), stdout.compat());

    // Shared slot for routing notifications to the active prompt's channel.
    let prompt_response_tx: Arc<Mutex<Option<mpsc::Sender<AcpUpdate>>>> =
        Arc::new(Mutex::new(None));

    ClientToAgent::builder()
        .on_receive_notification(
            {
                let prompt_response_tx = prompt_response_tx.clone();
                async move |notification: SessionNotification, _cx| {
                    if let Some(tx) = prompt_response_tx
                        .lock()
                        .ok()
                        .as_ref()
                        .and_then(|g| g.as_ref())
                    {
                        match notification.update {
                            SessionUpdate::AgentMessageChunk(ContentChunk {
                                content: ContentBlock::Text(TextContent { text, .. }),
                                ..
                            }) => {
                                let _ = tx.try_send(AcpUpdate::Text(text));
                            }
                            SessionUpdate::AgentThoughtChunk(ContentChunk {
                                content: ContentBlock::Text(TextContent { text, .. }),
                                ..
                            }) => {
                                let _ = tx.try_send(AcpUpdate::Thought(text));
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                }
            },
            sacp::on_receive_notification!(),
        )
        .on_receive_request(
            // Auto-approve all permission requests. We run in plan mode where
            // the agent does not execute dangerous tools, and we do not surface
            // permission prompts to the user.
            async move |_request: RequestPermissionRequest,
                        request_cx,
                        _connection_cx| {
                let response =
                    RequestPermissionResponse::new(RequestPermissionOutcome::Approved);
                request_cx.respond(response)
            },
            sacp::on_receive_request!(),
        )
        .connect_to(transport)?
        .run_until(move |cx: JrConnectionCx<ClientToAgent>| {
            handle_requests(config, cx, &mut rx, prompt_response_tx, init_tx)
        })
        .await?;

    Ok(())
}

/// Process requests from the `AcpProvider` channel. Runs inside `run_until`
/// so it has access to the protocol connection context.
async fn handle_requests(
    config: AcpProviderConfig,
    cx: JrConnectionCx<ClientToAgent>,
    rx: &mut mpsc::Receiver<ClientRequest>,
    prompt_response_tx: Arc<Mutex<Option<mpsc::Sender<AcpUpdate>>>>,
    init_tx: oneshot::Sender<Result<InitializeResponse>>,
) -> Result<(), sacp::Error> {
    let mut init_tx = Some(init_tx);

    // Perform the ACP initialization handshake.
    let init_response = cx
        .send_request(InitializeRequest::new(ProtocolVersion::LATEST))
        .block_task()
        .await
        .map_err(|err| {
            let message = format!("ACP initialize failed: {err}");
            if let Some(tx) = init_tx.take() {
                let _ = tx.send(Err(anyhow::anyhow!(message.clone())));
            }
            sacp::Error::internal_error().data(message)
        })?;

    if let Some(tx) = init_tx.take() {
        let _ = tx.send(Ok(init_response));
    }

    // Main request loop.
    while let Some(request) = rx.recv().await {
        match request {
            ClientRequest::NewSession { response_tx } => {
                let session = cx
                    .send_request(NewSessionRequest::new(config.work_dir.clone()))
                    .block_task()
                    .await;

                let result = match session {
                    Ok(session) => apply_session_mode(&config, &cx, session).await,
                    Err(err) => Err(anyhow::anyhow!("ACP session/new failed: {err}")),
                };

                let _ = response_tx.send(result);
            }

            ClientRequest::Prompt {
                session_id,
                content,
                response_tx,
            } => {
                // Install the channel so notifications route to this prompt.
                *prompt_response_tx.lock().unwrap() = Some(response_tx.clone());

                let response = cx
                    .send_request(PromptRequest::new(session_id, content))
                    .block_task()
                    .await;

                match response {
                    Ok(r) => {
                        let _ = response_tx.try_send(AcpUpdate::Complete(r.stop_reason));
                    }
                    Err(e) => {
                        let _ = response_tx.try_send(AcpUpdate::Error(e.to_string()));
                    }
                }

                // Clear the channel after the prompt completes.
                *prompt_response_tx.lock().unwrap() = None;
            }

            ClientRequest::Shutdown => break,
        }
    }

    Ok(())
}

/// If the requested session mode differs from the current one, send
/// `session/set_mode` to switch.
async fn apply_session_mode(
    config: &AcpProviderConfig,
    cx: &JrConnectionCx<ClientToAgent>,
    session: NewSessionResponse,
) -> Result<NewSessionResponse> {
    if let Some(modes) = session.modes.as_ref() {
        // Only switch if the current mode doesn't match the requested one.
        if modes.current_mode_id.0.as_ref() != config.session_mode.as_str() {
            let available: Vec<String> = modes
                .available_modes
                .iter()
                .map(|mode| mode.id.0.to_string())
                .collect();

            if !available.iter().any(|id| id == &config.session_mode) {
                return Err(anyhow::anyhow!(
                    "Requested mode '{}' not offered by agent. Available modes: {}",
                    config.session_mode,
                    available.join(", ")
                ));
            }

            cx.send_request(SetSessionModeRequest::new(
                session.session_id.clone(),
                config.session_mode.clone(),
            ))
            .block_task()
            .await
            .map_err(|err| {
                anyhow::anyhow!("ACP agent rejected session/set_mode: {err}")
            })?;
        }
    }

    Ok(session)
}
