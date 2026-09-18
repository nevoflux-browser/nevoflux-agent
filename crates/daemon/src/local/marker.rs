//! The on-disk marker that records a completed engine install, plus the
//! upgrade/GC policy decided purely from it.
//!
//! `crate::local::install::install` writes one of these into an install
//! directory once every archive has downloaded, verified, and extracted
//! successfully: its release tag, install kind, the sha256 of every archive
//! that went into it, and a per-file manifest of the extracted contents. A
//! later run (the engine supervisor, Task 2.9) reads it back to decide
//! whether an already-installed directory can be trusted without
//! redownloading anything.
//!
//! Task 2.5 adds the policy functions that read a [`Marker`] without
//! writing one: [`tag_status`] classifies a release tag against
//! `crate::local::release::ENGINE_PINNED`/`ENGINE_COMPATIBLE`, and
//! [`should_gc`] decides whether an old install directory is safe to
//! delete. Neither touches disk -- both are pure decisions over data the
//! caller (Task 2.9) already has in hand, so they can be unit tested
//! without a filesystem.

use std::path::{Path, PathBuf};

use crate::local::release;

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

/// How a release tag relates to what this daemon build currently knows how
/// to run, per [`tag_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagStatus {
    /// `crate::local::release::ENGINE_PINNED`'s own tag -- the release this
    /// daemon build installs fresh today.
    Pinned,
    /// One of `crate::local::release::ENGINE_COMPATIBLE`'s tags -- an older
    /// pinned release this build still knows how to run, kept so a machine
    /// already running it isn't forced to redownload on every daemon
    /// update.
    Compatible,
    /// Neither -- a tag this build has no archive table entry for at all
    /// (e.g. left over from a much older daemon version, or a foreign
    /// directory). [`should_gc`] treats this the same as a stale
    /// `Compatible` install: eligible once old and unused, never protected
    /// just because it happens to still be on disk.
    Unsupported,
}

/// Classifies `tag` against this build's known releases. `ENGINE_COMPATIBLE`
/// is empty today (`b10909-mix-bea84f7` is the first pinned tag, per its own
/// doc comment) -- that isn't special-cased here; an empty list simply never
/// matches, and `tag_status` returns [`TagStatus::Unsupported`] for
/// anything but the pinned tag until a future daemon update adds compatible
/// entries.
pub fn tag_status(tag: &str) -> TagStatus {
    tag_status_in(tag, &release::ENGINE_PINNED, release::ENGINE_COMPATIBLE)
}

/// [`tag_status`], parameterized over the pinned/compatible release lists
/// instead of reading `release::ENGINE_PINNED`/`release::ENGINE_COMPATIBLE`
/// directly -- so a test can exercise the [`TagStatus::Compatible`] branch
/// against a synthetic list without waiting for the real, currently-empty
/// `ENGINE_COMPATIBLE` to gain an entry (Task 2.5 review finding 3). Mirrors
/// the same shape `release::upstream_tag_for(tag, pinned, compatible)`
/// already uses in this codebase for the identical problem.
fn tag_status_in(
    tag: &str,
    pinned: &release::EngineRelease,
    compatible: &[release::EngineRelease],
) -> TagStatus {
    if tag == pinned.tag {
        TagStatus::Pinned
    } else if compatible.iter().any(|r| r.tag == tag) {
        TagStatus::Compatible
    } else {
        TagStatus::Unsupported
    }
}

/// How long an unpinned, unused install is kept around before it becomes
/// eligible for garbage collection.
const GC_AGE_SECS: i64 = 30 * 24 * 60 * 60;

/// Whether the install directory `dir` (recorded by `m`) may be deleted:
/// `m`'s tag is neither [`TagStatus::Pinned`] nor [`TagStatus::Compatible`]
/// (both are kept forever, however old or idle), `now - m.last_used_at` is
/// STRICTLY greater than 30 days (exactly 30 days is not yet eligible), and
/// `dir` is not one of `in_use_dirs` (e.g. the install an already-running
/// engine process has open) -- a directory a live process still has files
/// open in must never be deleted regardless of how old its marker's
/// `last_used_at` is.
pub fn should_gc(m: &Marker, now: i64, in_use_dirs: &[PathBuf], dir: &Path) -> bool {
    if matches!(
        tag_status(&m.tag),
        TagStatus::Pinned | TagStatus::Compatible
    ) {
        return false;
    }
    if now.saturating_sub(m.last_used_at) <= GC_AGE_SECS {
        return false;
    }
    if in_use_dirs.iter().any(|d| d == dir) {
        return false;
    }
    true
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

    // --- tag_status --------------------------------------------------------

    #[test]
    fn tag_status_recognizes_the_pinned_tag() {
        // sample_marker()'s tag ("b10909-mix-bea84f7") is deliberately the
        // real ENGINE_PINNED tag, not a fixture string -- see sample_marker.
        assert_eq!(
            tag_status(crate::local::release::ENGINE_PINNED.tag),
            TagStatus::Pinned
        );
    }

    #[test]
    fn tag_status_is_unsupported_for_an_unrecognized_tag() {
        // ENGINE_COMPATIBLE is empty today (the pinned tag is the first
        // ever pinned release), so every non-pinned tag currently falls
        // through to Unsupported -- not a special case, just what an empty
        // list naturally produces.
        assert_eq!(tag_status("totally-unknown-tag"), TagStatus::Unsupported);
    }

    /// `tag_status` itself can't exercise `TagStatus::Compatible` today --
    /// `release::ENGINE_COMPATIBLE` is `&[]` (the pinned tag is the first
    /// ever pinned release). `tag_status_in` (Task 2.5 review finding 3)
    /// makes the same classification testable against a synthetic list, the
    /// same way `release::upstream_tag_for`'s own tests already do for an
    /// analogous pinned/compatible lookup.
    #[test]
    fn tag_status_in_recognizes_a_synthetic_compatible_tag() {
        use crate::local::release::{EngineAsset, EngineRelease};

        fn synthetic(tag: &'static str) -> EngineRelease {
            EngineRelease {
                tag,
                upstream_tag: "irrelevant",
                archives: &[],
                cudart: &[],
                source: EngineAsset {
                    name: "x",
                    bytes: 0,
                    sha256: "x",
                },
            }
        }

        let pinned = synthetic("pinned-tag");
        let compatible = [synthetic("old-compatible-tag")];

        assert_eq!(
            tag_status_in("old-compatible-tag", &pinned, &compatible),
            TagStatus::Compatible
        );
        assert_eq!(
            tag_status_in("pinned-tag", &pinned, &compatible),
            TagStatus::Pinned
        );
        assert_eq!(
            tag_status_in("neither-tag", &pinned, &compatible),
            TagStatus::Unsupported
        );
    }

    // --- should_gc -----------------------------------------------------------

    /// A marker with `tag`/`last_used_at` overridden from `sample_marker()`,
    /// so each `should_gc` test only has to state what it's actually
    /// varying.
    fn marker_with(tag: &str, last_used_at: i64) -> Marker {
        let mut m = sample_marker();
        m.tag = tag.to_string();
        m.last_used_at = last_used_at;
        m
    }

    #[test]
    fn should_gc_protects_the_pinned_tag_even_when_old_and_unused() {
        let now = 2_000_000_000;
        let m = marker_with(release::ENGINE_PINNED.tag, now - GC_AGE_SECS - 1);
        assert!(!should_gc(&m, now, &[], Path::new("/installs/x")));
    }

    #[test]
    fn should_gc_protects_a_directory_that_is_in_use_regardless_of_age() {
        let now = 2_000_000_000;
        let m = marker_with("some-other-tag", now - GC_AGE_SECS - 1);
        let dir = PathBuf::from("/installs/x");
        assert!(!should_gc(&m, now, &[dir.clone()], &dir));
    }

    #[test]
    fn should_gc_is_false_exactly_at_the_30_day_boundary() {
        let now = 2_000_000_000;
        let m = marker_with("some-other-tag", now - GC_AGE_SECS);
        assert!(
            !should_gc(&m, now, &[], Path::new("/installs/x")),
            "exactly 30 days old must not yet be eligible"
        );
    }

    #[test]
    fn should_gc_is_true_one_second_past_the_30_day_boundary() {
        let now = 2_000_000_000;
        let m = marker_with("some-other-tag", now - GC_AGE_SECS - 1);
        assert!(should_gc(&m, now, &[], Path::new("/installs/x")));
    }

    #[test]
    fn should_gc_is_false_for_a_recently_used_unpinned_tag() {
        let now = 2_000_000_000;
        let m = marker_with("some-other-tag", now - 1_000);
        assert!(!should_gc(&m, now, &[], Path::new("/installs/x")));
    }

    #[test]
    fn should_gc_is_false_for_an_unsupported_but_still_in_use_directory_even_when_ancient() {
        // Unsupported (not just Compatible) still isn't enough to override
        // the in-use guard.
        let now = 2_000_000_000;
        let m = marker_with("ancient-unknown-tag", 0);
        let dir = PathBuf::from("/installs/ancient");
        assert!(!should_gc(&m, now, &[dir.clone()], &dir));
    }
}
