//! Shared error type for on-device (local) inference.
//!
//! Task 2.8's brief ("Local state, errors and events") owns the full shape
//! this grows into: `LocalState`, the `system:local:*` event topics, and
//! `publish_state`, alongside a `LocalError` with eight more variants
//! (`BackendUnavailable`, `EngineCrash`, `EngineInsecure`, `EngineCorrupt`,
//! `EngineUpdateRequired`, `ModelTooLargeForMemory`, `Busy`, `Offline`) that
//! only later engine-supervisor tasks produce. This file exists ahead of
//! that task because Task 2.4's `install()` (`crate::local::install`) needs
//! *a* shared error type to return today — it defines only the five
//! variants installation itself can produce, using the exact shape Task
//! 2.8's brief already specifies for them, so that task only needs to add
//! the rest rather than reconcile a conflicting definition.
//!
//! (Cross-task note left for whoever dispatches Task 2.8: this file already
//! exists, so that task's "Create `crates/daemon/src/local/state.rs`" is a
//! Modify, not a Create.)

/// Something that stopped on-device inference from becoming ready.
#[derive(Debug, Clone, PartialEq, serde::Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum LocalError {
    #[error("download failed")]
    DownloadFailed { detail: String },
    #[error("checksum mismatch")]
    ChecksumMismatch,
    #[error("archive corrupt")]
    ArchiveCorrupt,
    #[error("engine files incomplete")]
    SentinelMissing { missing: String },
    #[error("not enough disk space")]
    NoSpace { needed: u64, available: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_a_code_tag_matching_task_2_8s_shape() {
        let v = serde_json::to_value(LocalError::NoSpace {
            needed: 10,
            available: 3,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"code": "no_space", "needed": 10, "available": 3})
        );

        let v = serde_json::to_value(LocalError::ChecksumMismatch).unwrap();
        assert_eq!(v, serde_json::json!({"code": "checksum_mismatch"}));
    }
}
