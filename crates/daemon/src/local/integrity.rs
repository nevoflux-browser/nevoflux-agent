//! Cold-start integrity check for an already-installed engine directory.
//!
//! `crate::local::install::install`'s own `sentinels_ok` (Task 2.4) checks
//! only that the files a given [`crate::local::hardware::InstallKind`]
//! needs to run are *present*, as a cheap sanity check right after
//! extraction. [`verify_manifest`] is the stronger, slower check this
//! module adds: re-hashing every file [`crate::local::marker::Marker::files`]
//! recorded at install time and comparing size + sha256 against what is on
//! disk *now*. A later task (the engine supervisor, Task 2.9) runs this
//! once per daemon start against an existing install before trusting it --
//! catching a partially-overwritten file, a crash mid-upgrade that a marker
//! alone would not reveal, or on-disk tampering -- at the cost of a full
//! read-and-hash of every extracted file, which `sentinels_ok` deliberately
//! avoids paying on every single install-completion check.

use std::path::{Path, PathBuf};

use crate::local::install;
use crate::local::marker::FileEntry;

/// Why [`verify_manifest`] rejected an installed directory: which
/// manifest-recorded file was missing, the wrong size, or hashed to
/// something other than what was recorded at install time. Carries the
/// file's manifest-relative path (`FileEntry::path`, always `/`-separated)
/// so a caller can log or surface exactly what changed.
#[derive(Debug, PartialEq)]
pub enum IntegrityError {
    Missing(String),
    SizeMismatch(String),
    HashMismatch(String),
}

/// Re-verifies every file in `files` (normally
/// `crate::local::marker::Marker::files`, read back via
/// `crate::local::marker::read_marker`) against what is actually on disk
/// under `dir`: present, the recorded size, and the recorded sha256, in
/// that order -- so a large file that is merely the wrong size fails fast
/// as [`IntegrityError::SizeMismatch`] without paying for a full hash.
/// Stops at the first failure (manifest order, which
/// `crate::local::install::build_manifest` writes sorted by path) rather
/// than collecting every mismatch -- one failure is already enough to
/// distrust the whole install.
pub fn verify_manifest(dir: &Path, files: &[FileEntry]) -> Result<(), IntegrityError> {
    for f in files {
        let path = resolve(dir, &f.path);

        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => return Err(IntegrityError::Missing(f.path.clone())),
        };
        if !metadata.is_file() {
            return Err(IntegrityError::Missing(f.path.clone()));
        }
        if metadata.len() != f.size {
            return Err(IntegrityError::SizeMismatch(f.path.clone()));
        }

        let sha256 = match install::sha256_file_sync(&path) {
            Ok(h) => h,
            Err(_) => return Err(IntegrityError::Missing(f.path.clone())),
        };
        if sha256 != f.sha256 {
            return Err(IntegrityError::HashMismatch(f.path.clone()));
        }
    }
    Ok(())
}

/// Turns a manifest's always-`/`-separated relative path
/// (`FileEntry::path`'s own doc comment) into a platform path under `dir`,
/// joining component-by-component rather than
/// `dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))` so this
/// works identically regardless of what separator the current platform
/// uses.
fn resolve(dir: &Path, rel: &str) -> PathBuf {
    let mut out = dir.to_path_buf();
    for part in rel.split('/') {
        out.push(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    #[test]
    fn passes_when_every_file_matches_size_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server.exe"), b"binary-content").unwrap();
        let files = vec![FileEntry {
            path: "llama-server.exe".to_string(),
            size: b"binary-content".len() as u64,
            sha256: sha256_hex(b"binary-content"),
        }];
        assert_eq!(verify_manifest(dir.path(), &files), Ok(()));
    }

    #[test]
    fn passes_on_an_empty_manifest() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(verify_manifest(dir.path(), &[]), Ok(()));
    }

    #[test]
    fn reports_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![FileEntry {
            path: "llama-server.exe".to_string(),
            size: 10,
            sha256: "a".repeat(64),
        }];
        assert_eq!(
            verify_manifest(dir.path(), &files),
            Err(IntegrityError::Missing("llama-server.exe".to_string()))
        );
    }

    #[test]
    fn reports_a_size_mismatch_before_ever_hashing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"short").unwrap();
        let files = vec![FileEntry {
            path: "f".to_string(),
            size: 999,
            sha256: "a".repeat(64), // deliberately not a real hash of "short"
        }];
        assert_eq!(
            verify_manifest(dir.path(), &files),
            Err(IntegrityError::SizeMismatch("f".to_string()))
        );
    }

    /// The scenario the task brief names explicitly: a file tampered (or
    /// corrupted) to all-zero bytes but left at the SAME size the manifest
    /// recorded -- `SizeMismatch` alone would miss this; only the hash
    /// check catches it.
    #[test]
    fn detects_a_zeroed_file_of_equal_size_as_a_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let original: &[u8] = b"real-weights-not-actually-zero";
        let files = vec![FileEntry {
            path: "model.bin".to_string(),
            size: original.len() as u64,
            sha256: sha256_hex(original),
        }];
        std::fs::write(dir.path().join("model.bin"), vec![0u8; original.len()]).unwrap();
        assert_eq!(
            verify_manifest(dir.path(), &files),
            Err(IntegrityError::HashMismatch("model.bin".to_string()))
        );
    }

    #[test]
    fn resolves_forward_slash_manifest_paths_into_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("f"), b"x").unwrap();
        let files = vec![FileEntry {
            path: "sub/f".to_string(),
            size: 1,
            sha256: sha256_hex(b"x"),
        }];
        assert_eq!(verify_manifest(dir.path(), &files), Ok(()));
    }

    #[test]
    fn stops_at_the_first_failure_in_manifest_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"a-content").unwrap();
        // "b" is never written -- missing entirely.
        let files = vec![
            FileEntry {
                path: "a".to_string(),
                size: b"a-content".len() as u64,
                sha256: sha256_hex(b"a-content"),
            },
            FileEntry {
                path: "b".to_string(),
                size: 1,
                sha256: "a".repeat(64),
            },
        ];
        assert_eq!(
            verify_manifest(dir.path(), &files),
            Err(IntegrityError::Missing("b".to_string())),
            "the first (and only) failure in manifest order must be reported"
        );
    }
}
