//! `DaemonOnlyBrowser` — spawns the daemon binary as a subprocess (with
//! `NEVOFLUX_EVAL_MODE=1`), waits for `daemon.lock`, and kills the
//! subprocess on shutdown.
//!
//! Despite the name "browser", this mode does NOT launch a browser — it
//! launches just the daemon. The handle implements `BrowserHandle` so the
//! runner can use the same interface across all three modes.

use super::BrowserHandle;
use crate::daemon_client::lock::{wait_for_lock, DaemonLock};
use crate::{EvalError, EvalResult};
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info};

pub struct DaemonOnlyBrowser {
    run_id: String,
    state_dir: PathBuf,
    lock: DaemonLock,
    child: Arc<Mutex<Option<Child>>>,
}

impl DaemonOnlyBrowser {
    /// Spawn the daemon and wait for lock file.
    ///
    /// `daemon_binary`: path to the daemon binary, e.g. `target/release/nevoflux-agent`.
    /// `state_dir`: where the daemon writes its state (`<state_dir>/runs/<run_id>/...`).
    pub async fn spawn(daemon_binary: PathBuf, state_dir: PathBuf) -> EvalResult<Self> {
        let run_id = format!(
            "run-{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S-%f")
        );
        Self::spawn_with_run_id(daemon_binary, state_dir, run_id).await
    }

    pub async fn spawn_with_run_id(
        daemon_binary: PathBuf,
        state_dir: PathBuf,
        run_id: String,
    ) -> EvalResult<Self> {
        info!(
            binary = ?daemon_binary,
            state_dir = ?state_dir,
            run_id = %run_id,
            "spawning daemon for daemon-only eval"
        );

        if !daemon_binary.exists() {
            return Err(EvalError::Other(format!(
                "daemon binary not found at {}; build with `cargo build --release -p nevoflux-agent`",
                daemon_binary.display()
            )));
        }

        let child = Command::new(&daemon_binary)
            .arg("--daemon")
            .env("NEVOFLUX_EVAL_MODE", "1")
            .env("NEVOFLUX_EVAL_RUN_ID", &run_id)
            .env("NEVOFLUX_EVAL_STATE_DIR", &state_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| EvalError::Other(format!("spawn daemon: {e}")))?;

        let lock = wait_for_lock(
            &state_dir,
            &run_id,
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|e| EvalError::DaemonConnection(format!("lock wait: {e}")))?;

        info!(
            http_addr = %lock.http_addr,
            run_id = %run_id,
            "daemon ready"
        );

        Ok(Self {
            run_id,
            state_dir,
            lock,
            child: Arc::new(Mutex::new(Some(child))),
        })
    }

    pub fn lock_inner(&self) -> &DaemonLock {
        &self.lock
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn state_dir(&self) -> &std::path::Path {
        &self.state_dir
    }
}

#[async_trait]
impl BrowserHandle for DaemonOnlyBrowser {
    async fn ensure_ready(&self) -> EvalResult<()> {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => Ok(()),
                Ok(Some(status)) => Err(EvalError::DaemonConnection(format!(
                    "daemon exited unexpectedly with status {status}"
                ))),
                Err(e) => Err(EvalError::DaemonConnection(format!(
                    "wait failed: {e}"
                ))),
            }
        } else {
            Err(EvalError::DaemonConnection("daemon already shut down".into()))
        }
    }

    async fn shutdown(&self) -> EvalResult<()> {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            debug!("killing daemon subprocess");
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        Ok(())
    }

    fn version_string(&self) -> String {
        format!("nevoflux-daemon-only (run={})", self.run_id)
    }

    fn is_real_browser(&self) -> bool {
        false
    }

    fn lock(&self) -> Option<&DaemonLock> {
        Some(&self.lock)
    }
}
