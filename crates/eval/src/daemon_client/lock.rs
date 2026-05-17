//! Lock-file discovery: poll `<state_dir>/runs/<run_id>/daemon.lock` until
//! present or timeout, then read + validate.
//!
//! Lock-file shape matches `daemon::eval::lock::DaemonLock`. We re-declare
//! here (not depending on nevoflux-daemon as a lib dep) to keep the eval
//! crate independent of daemon internals.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonLock {
    pub pid: u32,
    pub started_at: String,
    pub http_addr: String,
    pub bearer_token: String,
    pub daemon_version: String,
    pub eval_run_id: String,
}

#[derive(Debug, Error)]
pub enum LockError {
    #[error("timeout waiting for daemon.lock at {path} after {seconds}s")]
    Timeout { path: String, seconds: u64 },
    #[error("lock file is malformed at {path}: {reason}")]
    Malformed { path: String, reason: String },
    #[error("daemon pid {pid} in lock file is no longer running")]
    StaleLock { pid: u32 },
    #[error("io error reading lock file: {0}")]
    Io(#[from] std::io::Error),
}

/// Poll for `<state_dir>/runs/<run_id>/daemon.lock` until present, then read
/// and validate it. Returns the parsed lock or errors on timeout / stale pid.
pub async fn wait_for_lock(
    state_dir: &Path,
    run_id: &str,
    timeout: Duration,
) -> Result<DaemonLock, LockError> {
    let lock_path = state_dir.join("runs").join(run_id).join("daemon.lock");
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if lock_path.exists() {
            match try_read(&lock_path).await {
                Ok(lock) => {
                    if !is_pid_alive(lock.pid) {
                        return Err(LockError::StaleLock { pid: lock.pid });
                    }
                    return Ok(lock);
                }
                Err(LockError::Malformed { .. }) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err(LockError::Timeout {
        path: lock_path.to_string_lossy().into_owned(),
        seconds: timeout.as_secs(),
    })
}

async fn try_read(path: &Path) -> Result<DaemonLock, LockError> {
    let bytes = tokio::fs::read(path).await?;
    if bytes.is_empty() {
        return Err(LockError::Malformed {
            path: path.to_string_lossy().into_owned(),
            reason: "empty file".into(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| LockError::Malformed {
        path: path.to_string_lossy().into_owned(),
        reason: e.to_string(),
    })
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
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: OpenProcess + CloseHandle are sound when given a valid
        // PID and matching handle; a zero handle indicates the PID is
        // unknown (or access denied) — either way we treat as "not alive".
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle == 0 {
                return false;
            }
            CloseHandle(handle);
            true
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        true
    }
}

// Note: PathBuf is intentionally unused at the moment but kept for future
// helpers; remove this line if clippy complains.
#[allow(dead_code)]
fn _unused_pathbuf_helper() -> PathBuf {
    PathBuf::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DaemonLock {
        DaemonLock {
            pid: std::process::id(),
            started_at: "2026-05-16T14:30:22Z".into(),
            http_addr: "127.0.0.1:39847".into(),
            bearer_token: "token".into(),
            daemon_version: "0.1.11".into(),
            eval_run_id: "run-test".into(),
        }
    }

    #[tokio::test]
    async fn wait_for_lock_finds_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("runs/run-test");
        tokio::fs::create_dir_all(&run_dir).await.unwrap();
        let lock_path = run_dir.join("daemon.lock");
        tokio::fs::write(&lock_path, serde_json::to_vec(&sample()).unwrap())
            .await
            .unwrap();

        let lock = wait_for_lock(tmp.path(), "run-test", Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(lock.eval_run_id, "run-test");
    }

    #[tokio::test]
    async fn wait_for_lock_times_out_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let result = wait_for_lock(tmp.path(), "run-test", Duration::from_millis(200)).await;
        assert!(matches!(result, Err(LockError::Timeout { .. })));
    }

    #[tokio::test]
    async fn wait_for_lock_detects_stale_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("runs/run-test");
        tokio::fs::create_dir_all(&run_dir).await.unwrap();
        let mut s = sample();
        // Pick a PID far above the max for the system so it's surely dead.
        s.pid = 4_000_000_000;
        tokio::fs::write(run_dir.join("daemon.lock"), serde_json::to_vec(&s).unwrap())
            .await
            .unwrap();
        let result = wait_for_lock(tmp.path(), "run-test", Duration::from_secs(1)).await;
        assert!(matches!(result, Err(LockError::StaleLock { .. })));
    }
}
