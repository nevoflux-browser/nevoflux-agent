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
    /// The install directory's own `<platform>-<backend>[-<variant>]`
    /// suffix (`crate::local::install::kind_suffix`), NOT a serialized
    /// `InstallKind` object -- controller ruling on Task 2.4 review finding
    /// M4: v3 §6 shows a concrete string value here
    /// (`"windows-x64-cuda13-older"`), which is binding, and a string
    /// buys a real integrity property an object form could not:
    /// [`read_marker`] rejects a marker whose `kind` does not match the
    /// suffix of the directory it was found in, catching a marker copied
    /// or restored into the wrong directory.
    pub kind: String,
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
/// trustworthy marker: missing file, truncated write, a foreign directory
/// that happens to occupy the path, OR (Task 2.4 review finding M4) a
/// marker whose `kind` does not match `dir`'s own `<tag>-<kind>` name --
/// which would mean this marker was copied or restored into the wrong
/// directory and must not be trusted just because the JSON parses.
pub fn read_marker(dir: &Path) -> Option<Marker> {
    let bytes = std::fs::read(dir.join(MARKER_FILE)).ok()?;
    let marker: Marker = serde_json::from_slice(&bytes).ok()?;

    let dir_name = dir.file_name()?.to_str()?;
    let expected_prefix = format!("{}-", marker.tag);
    let actual_kind = dir_name.strip_prefix(expected_prefix.as_str())?;
    if actual_kind != marker.kind {
        return None;
    }

    Some(marker)
}

/// Writes `m` to `dir/marker.json`, atomically: a temp file in the same
/// directory (so the rename is same-filesystem, not a copy) is written,
/// `fsync`'d, and closed first, then renamed over the real name. The
/// `fsync` (Task 2.4 review finding m6) means a power loss can only ever
/// leave the OLD marker (or none) in place, never a zero-length new one --
/// `rename` alone only orders the directory-entry update, not the data
/// actually reaching disk.
pub fn write_marker(dir: &Path, m: &Marker) -> std::io::Result<()> {
    use std::io::Write;

    let final_path = dir.join(MARKER_FILE);
    let tmp_path = dir.join(format!("{MARKER_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(m)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_marker() -> Marker {
        Marker {
            tag: "b10909-mix-bea84f7".to_string(),
            kind: "windows-x64-cpu".to_string(),
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

    /// A directory named `<tag>-<kind>`, matching `sample_marker()` --
    /// what `read_marker`'s integrity check requires. Returns the outer
    /// `TempDir` too so it isn't dropped (and the directory deleted) out
    /// from under the caller.
    fn sample_dir() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("b10909-mix-bea84f7-windows-x64-cpu");
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    #[test]
    fn round_trips_through_write_and_read() {
        let (_root, dir) = sample_dir();
        let m = sample_marker();
        write_marker(&dir, &m).unwrap();
        assert_eq!(read_marker(&dir), Some(m));
    }

    #[test]
    fn missing_marker_reads_as_none() {
        let (_root, dir) = sample_dir();
        assert_eq!(read_marker(&dir), None);
    }

    #[test]
    fn a_truncated_marker_reads_as_none_not_a_panic() {
        let (_root, dir) = sample_dir();
        std::fs::write(dir.join(MARKER_FILE), b"{not json").unwrap();
        assert_eq!(read_marker(&dir), None);
    }

    #[test]
    fn write_leaves_no_tmp_file_behind() {
        let (_root, dir) = sample_dir();
        write_marker(&dir, &sample_marker()).unwrap();
        assert!(!dir.join(format!("{MARKER_FILE}.tmp")).exists());
    }

    #[test]
    fn a_bad_mark_round_trips() {
        let (_root, dir) = sample_dir();
        let mut m = sample_marker();
        m.bad = Some(BadMark {
            by_build: "0.4.0".to_string(),
            reason: "security self-check failed".to_string(),
        });
        write_marker(&dir, &m).unwrap();
        assert_eq!(read_marker(&dir), Some(m));
    }

    /// Task 2.4 review finding M4's whole point: a marker whose `kind`
    /// does not match the directory it sits in (e.g. copied from another
    /// install, or the directory renamed after the fact) must not be
    /// trusted just because the JSON parses cleanly.
    #[test]
    fn read_marker_rejects_a_marker_whose_kind_does_not_match_its_directory() {
        let (_root, dir) = sample_dir(); // named "...-windows-x64-cpu"
        let mut m = sample_marker();
        m.kind = "windows-x64-cuda-cuda13-older".to_string(); // wrong on purpose
        write_marker(&dir, &m).unwrap();
        assert_eq!(
            read_marker(&dir),
            None,
            "a marker copied into the wrong directory must not be trusted"
        );
    }

    #[test]
    fn read_marker_rejects_a_marker_whose_tag_does_not_match_its_directory() {
        let (_root, dir) = sample_dir();
        let mut m = sample_marker();
        m.tag = "some-other-tag".to_string(); // the dir name no longer starts with "{tag}-"
        write_marker(&dir, &m).unwrap();
        assert_eq!(read_marker(&dir), None);
    }
}
