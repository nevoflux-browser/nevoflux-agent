//! `DevInstanceBrowser` — connects to an already-running nevoflux dev
//! instance whose daemon was started with `NEVOFLUX_DEV_INSTANCE_MODE=1`.
//!
//! The dev daemon writes a stable-path lock file (see daemon's
//! `dev_instance::lock_path`). This handle reads it, parses the
//! `DaemonLock`, and exposes the HTTP address + bearer token via the
//! `BrowserHandle::lock()` trait method (so the runner builds its
//! `DaemonHttpClient` the same way as in DaemonOnly mode).

use super::BrowserHandle;
use crate::daemon_client::lock::DaemonLock;
use crate::{EvalError, EvalResult};
use async_trait::async_trait;
use std::path::PathBuf;
use tracing::{debug, info};

#[derive(Debug)]
pub struct DevInstanceBrowser {
    endpoint_hint: String,
    lock: DaemonLock,
}

impl DevInstanceBrowser {
    /// Connect by reading the dev-instance lock file. `endpoint_hint` is
    /// the `--browser-endpoint` CLI value — kept for diagnostic / display
    /// purposes only since discovery actually goes via the lock file.
    pub async fn connect(endpoint_hint: String) -> EvalResult<Self> {
        let lock_path = resolve_lock_path();
        info!(
            ?lock_path,
            %endpoint_hint,
            "connecting to nevoflux dev instance via dev-instance lock"
        );

        if !lock_path.exists() {
            return Err(EvalError::DaemonConnection(format!(
                "dev-instance lock file not found at {}. \
                 Did you run nevoflux dev mode with NEVOFLUX_EVAL_MODE=1 and \
                 NEVOFLUX_DEV_INSTANCE_MODE=1 set? See \
                 eval/README-EXTERNAL-MODE.md for setup.",
                lock_path.display()
            )));
        }

        let raw = tokio::fs::read_to_string(&lock_path).await.map_err(|e| {
            EvalError::DaemonConnection(format!(
                "failed to read dev-instance lock at {}: {}",
                lock_path.display(),
                e
            ))
        })?;
        let lock: DaemonLock = serde_json::from_str(&raw).map_err(|e| {
            EvalError::DaemonConnection(format!(
                "dev-instance lock at {} is malformed: {}",
                lock_path.display(),
                e
            ))
        })?;

        if !is_pid_alive(lock.pid) {
            return Err(EvalError::DaemonConnection(format!(
                "dev-instance lock claims pid {} but that process is no \
                 longer running. The dev nevoflux instance may have exited; \
                 restart it.",
                lock.pid
            )));
        }

        debug!(http_addr = %lock.http_addr, "dev instance lock read OK");
        Ok(Self {
            endpoint_hint,
            lock,
        })
    }
}

fn resolve_lock_path() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("NEVOFLUX_DEV_INSTANCE_STATE_DIR") {
        PathBuf::from(override_dir).join("daemon.lock")
    } else {
        directories::ProjectDirs::from("com", "nevoflux", "nevoflux-dev")
            .map(|d| d.data_local_dir().to_path_buf())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".local/state/nevoflux-dev")
            })
            .join("daemon.lock")
    }
}

fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use std::process::Command;
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[async_trait]
impl BrowserHandle for DevInstanceBrowser {
    async fn ensure_ready(&self) -> EvalResult<()> {
        if !is_pid_alive(self.lock.pid) {
            return Err(EvalError::DaemonConnection(format!(
                "dev instance pid {} no longer alive",
                self.lock.pid
            )));
        }
        Ok(())
    }

    async fn shutdown(&self) -> EvalResult<()> {
        debug!("dev instance shutdown is a no-op (developer owns the lifecycle)");
        Ok(())
    }

    fn version_string(&self) -> String {
        format!(
            "nevoflux-dev-instance ({}, pid={})",
            self.endpoint_hint, self.lock.pid
        )
    }

    fn is_real_browser(&self) -> bool {
        true
    }

    fn lock(&self) -> Option<&DaemonLock> {
        Some(&self.lock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize tests that mutate `NEVOFLUX_DEV_INSTANCE_STATE_DIR` so they
    /// don't interfere when the suite runs with the default multi-threaded
    /// harness.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn sample_lock() -> DaemonLock {
        DaemonLock {
            pid: std::process::id(),
            started_at: "2026-05-16T12:00:00Z".into(),
            http_addr: "127.0.0.1:12345".into(),
            bearer_token: "token-dev".into(),
            daemon_version: "0.2.0".into(),
            eval_run_id: "dev-instance".into(),
        }
    }

    #[tokio::test]
    async fn connect_succeeds_when_lock_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR", tmp.path());
        let lock_path = tmp.path().join("daemon.lock");
        tokio::fs::write(&lock_path, serde_json::to_vec(&sample_lock()).unwrap())
            .await
            .unwrap();
        let h = DevInstanceBrowser::connect("http://localhost:5959".into())
            .await
            .unwrap();
        assert!(h.lock().is_some());
        assert_eq!(h.lock().unwrap().http_addr, "127.0.0.1:12345");
        std::env::remove_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR");
    }

    #[tokio::test]
    async fn connect_fails_with_helpful_message_when_lock_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR", tmp.path());
        let err = DevInstanceBrowser::connect("http://localhost:5959".into())
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("dev-instance lock file not found"),
            "got: {msg}"
        );
        assert!(
            msg.contains("NEVOFLUX_DEV_INSTANCE_MODE"),
            "should suggest env var"
        );
        std::env::remove_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR");
    }

    #[tokio::test]
    async fn connect_detects_stale_pid() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR", tmp.path());
        let mut lock = sample_lock();
        lock.pid = 4_000_000_000;
        tokio::fs::write(
            tmp.path().join("daemon.lock"),
            serde_json::to_vec(&lock).unwrap(),
        )
        .await
        .unwrap();
        let err = DevInstanceBrowser::connect("http://localhost:5959".into())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no longer running"));
        std::env::remove_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR");
    }
}
