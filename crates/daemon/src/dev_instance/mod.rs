//! Dev-instance lock contract for eval `--browser-mode external`.
//!
//! When env var `NEVOFLUX_DEV_INSTANCE_MODE=1` is set at daemon startup, the
//! daemon writes a `daemon.lock` file at a STABLE, well-known path
//! (`<state_dir>/nevoflux-dev/daemon.lock`). The eval crate's
//! `ExternalDevInstance::connect` reads that file to obtain the HTTP bridge
//! address + bearer token.
//!
//! This is a strict subset of `NEVOFLUX_EVAL_MODE` — only the lock file is
//! written. Console logging, normal traces DB, and the learning system all
//! remain active. The developer continues their work uninterrupted while
//! eval clients can connect on the side.

use crate::eval::lock::{write_atomic, DaemonLock};
use std::path::PathBuf;

pub const ENV_VAR: &str = "NEVOFLUX_DEV_INSTANCE_MODE";

pub fn is_enabled() -> bool {
    std::env::var(ENV_VAR).as_deref() == Ok("1")
}

/// Stable lock-file location used by `--browser-mode external`.
///
/// Layout: `<state_dir>/nevoflux-dev/daemon.lock`
/// Override via `NEVOFLUX_DEV_INSTANCE_STATE_DIR` env var (tests use this).
pub fn lock_path() -> PathBuf {
    let base = if let Some(override_dir) = std::env::var_os("NEVOFLUX_DEV_INSTANCE_STATE_DIR") {
        PathBuf::from(override_dir)
    } else {
        directories::ProjectDirs::from("com", "nevoflux", "nevoflux-dev")
            .map(|d| d.data_local_dir().to_path_buf())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".local/state/nevoflux-dev")
            })
    };
    base.join("daemon.lock")
}

/// Write the dev-instance lock atomically. Called from daemon boot when
/// `NEVOFLUX_DEV_INSTANCE_MODE=1`.
pub fn write_lock(http_addr: &str, bearer_token: &str) -> std::io::Result<()> {
    let lock = DaemonLock {
        pid: std::process::id(),
        started_at: chrono::Utc::now().to_rfc3339(),
        http_addr: http_addr.to_string(),
        bearer_token: bearer_token.to_string(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        eval_run_id: "dev-instance".to_string(),
    };
    let path = lock_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&path, &lock).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_uses_override() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR", tmp.path());
        let path = lock_path();
        assert!(path.starts_with(tmp.path()));
        assert!(path.ends_with("daemon.lock"));
        std::env::remove_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR");
    }

    #[test]
    fn write_and_read_lock_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR", tmp.path());
        write_lock("127.0.0.1:12345", "secret").unwrap();
        let raw = std::fs::read_to_string(lock_path()).unwrap();
        let lock: DaemonLock = serde_json::from_str(&raw).unwrap();
        assert_eq!(lock.http_addr, "127.0.0.1:12345");
        assert_eq!(lock.bearer_token, "secret");
        assert_eq!(lock.eval_run_id, "dev-instance");
        std::env::remove_var("NEVOFLUX_DEV_INSTANCE_STATE_DIR");
    }
}
