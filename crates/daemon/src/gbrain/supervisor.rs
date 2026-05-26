//! Cross-platform gbrain subprocess supervisor.
//!
//! This is the production version of the Windows-only spike under
//! `spike/supervisor/`. It spawns `bun run <cli.ts> serve`, holds the
//! resulting child's stdin open via [`super::mcp_client::McpClient`],
//! and restarts the subprocess with exponential backoff under a bounded
//! restart budget when it dies.
//!
//! ## What the spike validated (relied on here)
//!
//! - `bunx gbrain` does NOT work with gbrain 0.40.8.1 — invoke
//!   `bun run <node_modules>/gbrain/src/cli.ts serve` instead.
//! - gbrain graceful-exits when its stdin is closed; the supervisor
//!   uses this for clean shutdown.
//! - MCP framing is line-delimited JSON-RPC over stdio (see
//!   [`super::mcp_client`]).
//! - gbrain reads `OPENROUTER_BASE_URL` / `OPENROUTER_API_KEY`, NOT
//!   `OPENAI_*` (spike 附录 B operational quirk #2).
//! - `--brain-dir` flag is ignored; `$GBRAIN_BRAIN_DIR` is honored
//!   (spike 附录 B operational quirk #1).
//!
//! ## Process lifetime / orphan handling
//!
//! The daemon binary already binds the entire `nevoflux` process into
//! an anonymous Windows Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! at the top of `run_daemon` (commit `47cd21b`). Any child the daemon
//! spawns — including `bun run gbrain serve` — inherits the Job
//! automatically, so when the daemon process exits the kernel terminates
//! every descendant.
//!
//! On Windows we additionally pass `CREATE_NEW_PROCESS_GROUP` so a
//! Ctrl-C delivered to a console attached to the daemon does NOT
//! propagate to gbrain. On Unix we rely on `kill_on_drop` from
//! `tokio::process::Command`, which sends `SIGKILL` to the child when
//! the `Child` value is dropped without an explicit wait.
//!
//! Spike S2b verified gbrain 0.40.8.1 doesn't fork worker subprocesses,
//! so the inherited Job + `kill_on_drop` is sufficient. If a future
//! gbrain version fans out workers we'd revisit and add per-supervisor
//! Job Object nesting on Windows / `setsid()` + process-group SIGTERM
//! on Unix.

use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::config::GbrainConfig;
use super::mcp_client::{McpClient, McpError};

/// Errors that can occur during supervisor lifecycle operations.
#[derive(Debug, Error)]
pub enum SupervisorError {
    /// The supervisor is not currently in the `Running` state, so the
    /// request cannot be dispatched. Callers should consult
    /// [`GbrainSupervisor::state`] before retrying.
    #[error("gbrain supervisor is not running (current state: {state:?})")]
    NotRunning {
        /// Snapshot of the supervisor state at the time of the request.
        state: SupervisorState,
    },

    /// A failure originating from the underlying MCP stdio client
    /// (timeout, JSON-RPC error envelope, transport closed).
    #[error("MCP error: {0}")]
    Mcp(#[from] McpError),

    /// Spawning the subprocess failed (path lookup, permissions, etc.).
    #[error("spawn gbrain serve failed: {0}")]
    Spawn(#[source] std::io::Error),

    /// Reading from the spawned child's pipes failed.
    #[error("child IO error: {0}")]
    ChildIo(#[from] std::io::Error),
}

/// Result alias for fallible supervisor operations.
pub type SupervisorResult<T> = std::result::Result<T, SupervisorError>;

/// Error type returned by [`McpToolCaller`] implementations.
///
/// Boxed `dyn Error + Send + Sync` so the trait stays generic over the
/// underlying transport's error type (the production transport returns
/// [`SupervisorError`]; test stubs can return anything). Callers
/// typically convert these to [`nevoflux_brain::BrainError::Backend`].
pub type McpToolCallerError = Box<dyn std::error::Error + Send + Sync>;

/// Abstraction over an MCP transport that can dispatch `tools/call`
/// requests. Production callers use [`GbrainSupervisor`] (which spawns
/// gbrain serve and tracks its lifecycle); tests can substitute an
/// in-memory stub to avoid spawning the real subprocess.
#[async_trait::async_trait]
pub trait McpToolCaller: Send + Sync {
    /// Dispatch an MCP `tools/call` and return the full JSON-RPC
    /// response value (the outer envelope including `result`, `id`,
    /// etc. — callers are responsible for pulling out `result.content`).
    async fn call_tool_dyn(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, McpToolCallerError>;
}

#[async_trait::async_trait]
impl McpToolCaller for GbrainSupervisor {
    async fn call_tool_dyn(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, McpToolCallerError> {
        // Delegate to the inherent method that already exists on
        // GbrainSupervisor (defined below). Boxing SupervisorError as a
        // dyn Error preserves the underlying chain via downcast.
        self.call_tool(name, arguments)
            .await
            .map_err(|e| Box::new(e) as McpToolCallerError)
    }
}

/// High-level supervisor lifecycle state.
///
/// Transitions:
/// `Starting -> Running -> Restarting -> Starting -> ... -> Failed | Shutdown`
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorState {
    /// Subprocess spawn in flight (or about to start). Not yet ready
    /// for requests.
    Starting,

    /// Subprocess is alive and the MCP client is connected. Requests
    /// can be dispatched.
    Running {
        /// Monotonic timestamp captured when the supervisor entered the
        /// `Running` state for this generation of the subprocess.
        initialized_at_elapsed_ms: u128,
    },

    /// Subprocess died; supervisor is sleeping its backoff before the
    /// next spawn attempt.
    Restarting {
        /// 1-based attempt counter within the current restart window.
        attempt: u32,
    },

    /// Restart budget exhausted within the configured window; the
    /// supervisor task has returned and no further requests will
    /// succeed.
    Failed {
        /// Human-readable summary of why the supervisor gave up.
        reason: String,
    },

    /// Graceful shutdown completed. Terminal.
    Shutdown,
}

/// Handle to a running gbrain supervisor.
///
/// The supervisor task runs in the background; this handle owns the
/// shutdown channel and the most-recent [`McpClient`] (which is
/// replaced on every restart). Drop semantics: dropping the handle does
/// NOT shut down the supervisor; call [`Self::shutdown`] for that.
pub struct GbrainSupervisor {
    config: GbrainConfig,
    state: Arc<RwLock<SupervisorState>>,
    client: Arc<RwLock<Option<McpClient>>>,
    /// Sender retained so callers using `watch::Receiver` (via
    /// [`Self::subscribe_state`]) can be notified of transitions.
    state_tx: watch::Sender<SupervisorState>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    main_handle: Option<JoinHandle<()>>,
}

impl GbrainSupervisor {
    /// Spawn the supervisor task. Returns immediately; the supervisor
    /// runs in the background and starts its first child on the next
    /// poll.
    ///
    /// To wait until the child is ready for requests, await
    /// [`Self::wait_running`] (which observes the watch channel).
    pub async fn spawn(config: GbrainConfig) -> Self {
        let (state_tx, _state_rx_initial) = watch::channel(SupervisorState::Starting);
        let state = Arc::new(RwLock::new(SupervisorState::Starting));
        let client = Arc::new(RwLock::new(None));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let task_config = config.clone();
        let task_state = Arc::clone(&state);
        let task_client = Arc::clone(&client);
        let task_state_tx = state_tx.clone();

        let main_handle = tokio::spawn(async move {
            run_supervisor_loop(
                task_config,
                task_state,
                task_client,
                task_state_tx,
                shutdown_rx,
            )
            .await;
        });

        Self {
            config,
            state,
            client,
            state_tx,
            shutdown_tx: Some(shutdown_tx),
            main_handle: Some(main_handle),
        }
    }

    /// Snapshot of current supervisor state.
    pub async fn state(&self) -> SupervisorState {
        self.state.read().await.clone()
    }

    /// Subscribe to state transitions. The returned receiver is
    /// guaranteed to observe at least the next state change after the
    /// call.
    pub fn subscribe_state(&self) -> watch::Receiver<SupervisorState> {
        self.state_tx.subscribe()
    }

    /// Send an arbitrary JSON-RPC request to the live gbrain MCP
    /// client. Returns [`SupervisorError::NotRunning`] if the
    /// supervisor isn't in [`SupervisorState::Running`] right now.
    pub async fn request(&self, method: &str, params: Value) -> SupervisorResult<Value> {
        let client_guard = self.client.read().await;
        let client = match client_guard.as_ref() {
            Some(c) => c,
            None => {
                return Err(SupervisorError::NotRunning {
                    state: self.state.read().await.clone(),
                });
            }
        };
        let resp = client
            .request(method, params, self.config.request_timeout)
            .await?;
        Ok(resp)
    }

    /// MCP `initialize` handshake + `notifications/initialized` ping.
    /// Convenience wrapper used by [`crate::gbrain`] callers to confirm
    /// the subprocess is responsive after a fresh spawn.
    pub async fn initialize(&self) -> SupervisorResult<Value> {
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "nevoflux-daemon",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let resp = self.request("initialize", init_params).await?;
        // Best-effort notification ping; MCP spec recommends sending
        // `notifications/initialized` after a successful `initialize`.
        // gbrain 0.40.8.1 didn't require it in the spike but it's
        // harmless.
        let client_guard = self.client.read().await;
        if let Some(client) = client_guard.as_ref() {
            let _ = client
                .notify("notifications/initialized", serde_json::json!({}))
                .await;
        }
        Ok(resp)
    }

    /// MCP `tools/list` convenience wrapper.
    pub async fn list_tools(&self) -> SupervisorResult<Value> {
        self.request("tools/list", serde_json::json!({})).await
    }

    /// MCP `tools/call` convenience wrapper.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> SupervisorResult<Value> {
        let params = serde_json::json!({"name": name, "arguments": arguments});
        self.request("tools/call", params).await
    }

    /// Graceful shutdown: signals the supervisor task to stop
    /// respawning, closes stdin on the current client (so gbrain sees
    /// EOF and exits cleanly), then waits up to 5 s for the supervisor
    /// task to exit.
    ///
    /// Idempotent — calling twice is fine; subsequent calls are no-ops.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // Take the current client (if any) and gracefully close it; the
        // supervisor task's select! arm will additionally observe the
        // shutdown signal and stop respawning.
        let client_opt = self.client.write().await.take();
        if let Some(client) = client_opt {
            client.shutdown().await;
        }
        if let Some(handle) = self.main_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
    }
}

/// Background task entry point — runs the spawn/restart/shutdown loop.
async fn run_supervisor_loop(
    config: GbrainConfig,
    state: Arc<RwLock<SupervisorState>>,
    client: Arc<RwLock<Option<McpClient>>>,
    state_tx: watch::Sender<SupervisorState>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let mut backoff = config.initial_restart_backoff;
    let mut restart_count = 0u32;
    let mut window_start = Instant::now();

    loop {
        // Check for shutdown signal between restart attempts.
        match shutdown_rx.try_recv() {
            Ok(()) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                info!("gbrain supervisor received shutdown signal");
                set_state(&state, &state_tx, SupervisorState::Shutdown).await;
                return;
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
        }

        // Roll the restart window if enough time has passed.
        if window_start.elapsed() > config.restart_window {
            window_start = Instant::now();
            restart_count = 0;
            backoff = config.initial_restart_backoff;
        }

        if restart_count >= config.max_restarts_within_window {
            let reason = format!(
                "restart budget exceeded ({} restarts within {:?})",
                restart_count, config.restart_window
            );
            error!(
                restart_count,
                window = ?config.restart_window,
                "gbrain restart budget exceeded; giving up"
            );
            set_state(
                &state,
                &state_tx,
                SupervisorState::Failed { reason },
            )
            .await;
            return;
        }

        set_state(&state, &state_tx, SupervisorState::Starting).await;
        match spawn_and_supervise(&config, &client, &state, &state_tx, &mut shutdown_rx)
            .await
        {
            Ok(ChildExit::Graceful) => {
                info!("gbrain serve exited cleanly; supervisor returning");
                set_state(&state, &state_tx, SupervisorState::Shutdown).await;
                return;
            }
            Ok(ChildExit::ShutdownRequested) => {
                info!("gbrain supervisor shutdown completed");
                set_state(&state, &state_tx, SupervisorState::Shutdown).await;
                return;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "gbrain serve died unexpectedly; will restart after backoff"
                );
                restart_count = restart_count.saturating_add(1);
                set_state(
                    &state,
                    &state_tx,
                    SupervisorState::Restarting {
                        attempt: restart_count,
                    },
                )
                .await;
                // Sleep backoff or shortcut on shutdown.
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = &mut shutdown_rx => {
                        info!("shutdown received during restart backoff");
                        set_state(&state, &state_tx, SupervisorState::Shutdown).await;
                        return;
                    }
                }
                backoff = (backoff.saturating_mul(2)).min(config.max_restart_backoff);
            }
        }
    }
}

/// Reason `spawn_and_supervise` returned Ok.
enum ChildExit {
    /// Child exited on its own with status code 0.
    Graceful,
    /// We told the child to shut down (because the supervisor was
    /// asked to shut down). Don't restart.
    ShutdownRequested,
}

async fn spawn_and_supervise(
    config: &GbrainConfig,
    client_slot: &Arc<RwLock<Option<McpClient>>>,
    state: &Arc<RwLock<SupervisorState>>,
    state_tx: &watch::Sender<SupervisorState>,
    shutdown_rx: &mut tokio::sync::oneshot::Receiver<()>,
) -> Result<ChildExit, SupervisorError> {
    let mut cmd = Command::new(&config.bun_path);
    cmd.arg("run")
        .arg(&config.gbrain_cli_path)
        .arg("serve")
        .env("OPENROUTER_BASE_URL", &config.upstream_base_url)
        .env("OPENROUTER_API_KEY", &config.upstream_api_key)
        .env("GBRAIN_BRAIN_DIR", &config.brain_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    apply_platform_isolation(&mut cmd);

    let mut child: Child = cmd.spawn().map_err(SupervisorError::Spawn)?;
    let pid = child.id();
    info!(pid, "spawned gbrain serve");

    // (No explicit Job Object assignment here — the daemon binary
    // already binds the entire process into a kill-on-close Job, which
    // children inherit. See module-level docs.)

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| SupervisorError::ChildIo(std::io::Error::other("no stdin")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SupervisorError::ChildIo(std::io::Error::other("no stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SupervisorError::ChildIo(std::io::Error::other("no stderr")))?;

    spawn_stderr_forwarder(stderr);

    let mcp_client = McpClient::new(stdin, stdout);
    *client_slot.write().await = Some(mcp_client);

    let initialized_at = Instant::now();
    set_state(
        state,
        state_tx,
        SupervisorState::Running {
            initialized_at_elapsed_ms: initialized_at.elapsed().as_millis(),
        },
    )
    .await;

    // Wait for either child exit or shutdown signal.
    let outcome = tokio::select! {
        exit = child.wait() => {
            // The child exited on its own — pull out the latest client
            // and shut it down to avoid leaking the reader/writer tasks.
            let client_opt = client_slot.write().await.take();
            if let Some(client) = client_opt {
                client.shutdown().await;
            }
            let status = exit?;
            if status.success() {
                Ok(ChildExit::Graceful)
            } else {
                let msg = format!("gbrain serve exited with non-zero status: {status:?}");
                Err(SupervisorError::ChildIo(std::io::Error::other(msg)))
            }
        }
        _ = shutdown_rx => {
            info!("supervisor shutdown signal received during run; tearing down child");
            // Graceful first: take the client and drop its stdin so
            // gbrain sees EOF and exits cleanly.
            let client_opt = client_slot.write().await.take();
            if let Some(client) = client_opt {
                client.shutdown().await;
            }
            // Give gbrain a brief window to exit on its own before we
            // force-kill it (kill_on_drop also handles this on drop,
            // but explicit is clearer in logs).
            match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                Ok(Ok(_status)) => {
                    debug!("gbrain exited gracefully after stdin close");
                }
                _ => {
                    warn!("gbrain didn't exit within 3s of stdin close; force-killing");
                    let _ = child.kill().await;
                }
            }
            Ok(ChildExit::ShutdownRequested)
        }
    };

    outcome
}

/// Forward each child stderr line to `tracing`. gbrain logs operational
/// info to stderr at info-ish severity; only lines that look like
/// errors get bumped to `warn!`, everything else lands at `debug!` so
/// daemon logs aren't drowned out under default filtering.
fn spawn_stderr_forwarder(stderr: ChildStderr) {
    use tokio::io::AsyncBufReadExt;
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if line.to_lowercase().contains("error") {
                warn!(target: "gbrain", "{line}");
            } else {
                debug!(target: "gbrain", "{line}");
            }
        }
    });
}

async fn set_state(
    state: &Arc<RwLock<SupervisorState>>,
    tx: &watch::Sender<SupervisorState>,
    new_state: SupervisorState,
) {
    *state.write().await = new_state.clone();
    // Send error means there are no receivers; that's fine, the
    // canonical state is still in the RwLock for `state()` callers.
    let _ = tx.send(new_state);
}

#[cfg(windows)]
fn apply_platform_isolation(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: don't pop a console window when the daemon is
    // run as a windowed binary.
    // CREATE_NEW_PROCESS_GROUP: detach gbrain from any console attached
    // to the daemon, so a Ctrl-C on that console doesn't propagate to
    // gbrain. (The daemon's own Ctrl-C handler is responsible for
    // shutting gbrain down cleanly via the supervisor.)
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn apply_platform_isolation(_cmd: &mut Command) {
    // No-op on Unix for M3-1. `kill_on_drop(true)` covers the common
    // teardown path; if a future gbrain version starts forking workers
    // we'd reach for `pre_exec(libc::setsid)` and group-kill on
    // shutdown.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_failing_config() -> GbrainConfig {
        // Tight budgets so the test transitions Starting -> Restarting
        // -> Failed in well under a second.
        let mut cfg = GbrainConfig::test_default();
        cfg.max_restarts_within_window = 2;
        cfg.restart_window = Duration::from_secs(60);
        cfg.initial_restart_backoff = Duration::from_millis(10);
        cfg.max_restart_backoff = Duration::from_millis(50);
        cfg
    }

    #[tokio::test]
    async fn test_spawn_failure_transitions_to_failed() {
        // Paths in test_default are nonexistent so every spawn attempt
        // will fail. After max_restarts_within_window attempts the
        // supervisor must reach Failed and stop.
        let supervisor = GbrainSupervisor::spawn(fast_failing_config()).await;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Instant::now() > deadline {
                panic!(
                    "supervisor did not reach Failed within 5s; current state: {:?}",
                    supervisor.state().await
                );
            }
            if matches!(supervisor.state().await, SupervisorState::Failed { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn test_request_when_not_running_returns_error() {
        // Right after spawn, before any child is ready, request() must
        // return NotRunning.
        let supervisor = GbrainSupervisor::spawn(fast_failing_config()).await;
        let result = supervisor
            .request("ping", serde_json::json!({}))
            .await;
        match result {
            Err(SupervisorError::NotRunning { .. }) => {}
            other => panic!("expected NotRunning, got {other:?}"),
        }
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn test_shutdown_from_starting_state_transitions_to_shutdown() {
        // Spawn with a normal config (spawn will fail because paths are
        // bogus, but we shut down before/during the first attempt).
        let supervisor = GbrainSupervisor::spawn(GbrainConfig::test_default()).await;
        // Immediately ask the supervisor to shut down.
        supervisor.shutdown().await;
        // After shutdown completes, the supervisor handle is consumed;
        // we can't query its state directly, but the assertion is that
        // shutdown completed within the 5s timeout baked into shutdown().
        // If it hadn't, this test would have hung past test timeout.
    }

    #[tokio::test]
    async fn test_subscribe_state_observes_transitions() {
        let supervisor = GbrainSupervisor::spawn(fast_failing_config()).await;
        let mut rx = supervisor.subscribe_state();

        // Wait for any state transition; the supervisor will at minimum
        // emit Starting -> Restarting (or Starting -> Failed if the
        // budget is tiny enough).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_transition = false;
        while Instant::now() < deadline {
            // changed() resolves when a NEW value is sent on the watch.
            // Use a short timeout so the test doesn't hang if no
            // transition happens.
            match tokio::time::timeout(Duration::from_millis(200), rx.changed()).await {
                Ok(Ok(())) => {
                    let s = rx.borrow().clone();
                    if !matches!(s, SupervisorState::Starting) {
                        saw_transition = true;
                        break;
                    }
                }
                Ok(Err(_)) => break, // sender dropped
                Err(_) => continue,  // timeout; keep waiting
            }
        }
        assert!(
            saw_transition,
            "expected at least one non-Starting state transition"
        );

        supervisor.shutdown().await;
    }

    #[test]
    fn test_state_equality_for_running() {
        // Two Running states with different elapsed_ms ARE equal under
        // PartialEq if their elapsed fields match; assert the field is
        // part of the equality check (so external watchers can detect a
        // genuine new run).
        let a = SupervisorState::Running {
            initialized_at_elapsed_ms: 100,
        };
        let b = SupervisorState::Running {
            initialized_at_elapsed_ms: 100,
        };
        let c = SupervisorState::Running {
            initialized_at_elapsed_ms: 200,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_state_failed_carries_reason() {
        let s = SupervisorState::Failed {
            reason: "test reason".into(),
        };
        if let SupervisorState::Failed { reason } = s {
            assert_eq!(reason, "test reason");
        } else {
            panic!("expected Failed");
        }
    }
}
