//! Atomic write/read of `daemon.lock` for eval-client discovery.
//!
//! See spec §7.3.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonLock {
    pub pid: u32,
    pub started_at: String, // RFC3339
    pub http_addr: String,  // e.g. "127.0.0.1:39847"
    pub bearer_token: String,
    pub daemon_version: String,
    pub eval_run_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde_json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Atomic write: serialise to a temp file in the same directory, then rename.
/// Avoids eval clients reading half-written JSON.
pub fn write_atomic(path: &Path, lock: &DaemonLock) -> Result<(), LockError> {
    let parent = path.parent().expect("lock path must have parent");
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".daemon.lock.tmp.{}", std::process::id()));
    let json = serde_json::to_vec_pretty(lock)?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read(path: &Path) -> Result<DaemonLock, LockError> {
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DaemonLock {
        DaemonLock {
            pid: 12345,
            started_at: "2026-05-16T14:30:22Z".into(),
            http_addr: "127.0.0.1:39847".into(),
            bearer_token: "a1b2c3d4".into(),
            daemon_version: "0.1.11".into(),
            eval_run_id: "run-test".into(),
        }
    }

    #[test]
    fn round_trip_atomic_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.lock");
        let lock = sample();
        write_atomic(&path, &lock).unwrap();
        let read_back = read(&path).unwrap();
        assert_eq!(lock, read_back);
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.lock");
        write_atomic(&path, &sample()).unwrap();
        let mut updated = sample();
        updated.pid = 99999;
        write_atomic(&path, &updated).unwrap();
        let read_back = read(&path).unwrap();
        assert_eq!(read_back.pid, 99999);
    }
}
