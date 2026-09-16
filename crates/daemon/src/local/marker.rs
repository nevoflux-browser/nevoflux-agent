//! The on-disk marker that records a completed engine install.
//!
//! `crate::local::install::install` writes one of these into an install
//! directory once every archive has downloaded, verified, and extracted
//! successfully: its release tag, install kind, the sha256 of every archive
//! that went into it, and a per-file manifest of the extracted contents. A
//! later run (the engine supervisor, Task 2.9) reads it back to decide
//! whether an already-installed directory can be trusted without
//! redownloading anything.

use std::path::Path;

use crate::local::hardware::InstallKind;

pub const MARKER_FILE: &str = "marker.json";

/// One file inside an installed engine directory, as recorded at install
/// time -- so a later integrity check has something to compare against
/// without re-deriving what "correct" looks like.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FileEntry {
    /// Path relative to the install directory, always `/`-separated
    /// regardless of platform, so a marker written on one OS reads
    /// identically on another.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Why a completed install was marked unusable after the fact (e.g. by a
/// later engine-supervisor task's security self-check), and by which
/// daemon build -- so a newer build can decide whether the mark still
/// applies to it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct BadMark {
    pub by_build: String,
    pub reason: String,
}

/// Records a completed, verified engine install.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Marker {
    pub tag: String,
    pub kind: InstallKind,
    /// sha256 of every archive that went into this install: the main
    /// platform/backend archive, plus a Windows CUDA cudart archive when
    /// one was paired in by `release::archive_for`. Distinct from `files`
    /// below, which covers the *extracted* contents instead.
    pub archive_sha256: Vec<String>,
    pub files: Vec<FileEntry>,
    pub installed_at: i64,
    pub last_used_at: i64,
    pub bad: Option<BadMark>,
}

/// Reads `dir/marker.json`. `None` for anything short of a valid,
/// parseable marker -- missing file, truncated write, a foreign directory
/// that happens to occupy the path -- since every caller treats "no
/// marker" and "unreadable marker" the same way: not a trusted install.
pub fn read_marker(dir: &Path) -> Option<Marker> {
    let bytes = std::fs::read(dir.join(MARKER_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes `m` to `dir/marker.json`, atomically: a temp file in the same
/// directory (so the rename is same-filesystem, not a copy) is written and
/// closed first, then renamed over the real name. A reader can never
/// observe a half-written marker.
pub fn write_marker(dir: &Path, m: &Marker) -> std::io::Result<()> {
    let final_path = dir.join(MARKER_FILE);
    let tmp_path = dir.join(format!("{MARKER_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(m)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::hardware::Backend;

    fn sample_marker() -> Marker {
        Marker {
            tag: "b10909-mix-bea84f7".to_string(),
            kind: InstallKind {
                platform: "windows-x64".to_string(),
                backend: Backend::Cpu,
                variant: None,
                cudart: None,
            },
            archive_sha256: vec!["a".repeat(64)],
            files: vec![FileEntry {
                path: "llama-server.exe".to_string(),
                size: 123,
                sha256: "b".repeat(64),
            }],
            installed_at: 1_700_000_000,
            last_used_at: 1_700_000_000,
            bad: None,
        }
    }

    #[test]
    fn round_trips_through_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let m = sample_marker();
        write_marker(dir.path(), &m).unwrap();
        assert_eq!(read_marker(dir.path()), Some(m));
    }

    #[test]
    fn missing_marker_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_marker(dir.path()), None);
    }

    #[test]
    fn a_truncated_marker_reads_as_none_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MARKER_FILE), b"{not json").unwrap();
        assert_eq!(read_marker(dir.path()), None);
    }

    #[test]
    fn write_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), &sample_marker()).unwrap();
        assert!(!dir.path().join(format!("{MARKER_FILE}.tmp")).exists());
    }

    #[test]
    fn a_bad_mark_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = sample_marker();
        m.bad = Some(BadMark {
            by_build: "0.4.0".to_string(),
            reason: "security self-check failed".to_string(),
        });
        write_marker(dir.path(), &m).unwrap();
        assert_eq!(read_marker(dir.path()), Some(m));
    }
}
