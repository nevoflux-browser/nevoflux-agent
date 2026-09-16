//! Cold-start integrity check for an already-installed engine directory.
//!
//! `crate::local::install::install`'s own `sentinels_ok` (Task 2.4) checks
//! only that the files a given [`crate::local::hardware::InstallKind`]
//! needs to run are *present*, as a cheap sanity check right after
//! extraction. [`verify_manifest`] is the stronger, slower check this
//! module adds: re-hashing every file [`crate::local::marker::Marker::files`]
//! recorded at install time and comparing size + sha256 against what is on
//! disk *now*, AND confirming the directory holds no file the manifest
//! never recorded (Task 2.5 review finding 5) -- exact-SET verification,
//! not just per-entry verification. That second half matters because a
//! spawned engine's working directory is the install directory itself
//! (`harden::SpawnSpec::cwd`), and Windows resolves DLLs from the working
//! directory first: a file planted there after install is both loadable by
//! the engine and, without the exact-set check, invisible to a
//! subset-only verify. `marker.json` itself is the one file exempt from
//! this (see [`verify_manifest`]'s doc comment for why).
//!
//! A later task (the engine supervisor, Task 2.9) runs this once per
//! daemon start against an existing install before trusting it -- catching
//! a partially-overwritten file, a crash mid-upgrade that a marker alone
//! would not reveal, on-disk tampering, or a planted extra file -- at the
//! cost of a full read-and-hash of every extracted file plus a directory
//! walk, which `sentinels_ok` deliberately avoids paying on every single
//! install-completion check.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::local::install;
use crate::local::marker::{self, FileEntry};

/// Why [`verify_manifest`] rejected an installed directory. Carries the
/// file's manifest-relative path (`FileEntry::path`, always `/`-separated)
/// so a caller can log or surface exactly what changed, except `Io`, which
/// carries a path-prefixed description of the underlying I/O failure since
/// there is no [`std::io::Error`] to attach directly (this type derives
/// `PartialEq`, which `std::io::Error` does not).
#[derive(Debug, PartialEq)]
pub enum IntegrityError {
    /// A manifest-recorded file is absent (or the path names something
    /// that is not a regular file, e.g. a directory now occupies it).
    Missing(String),
    /// A manifest-recorded file is present but not the recorded size --
    /// reported before ever hashing, so a wrong-size multi-hundred-MB file
    /// fails fast.
    SizeMismatch(String),
    /// A manifest-recorded file is present, the recorded size, but hashes
    /// to something else -- e.g. a file left at the same size but zeroed
    /// out by a crash mid-write, which `SizeMismatch` alone would miss.
    HashMismatch(String),
    /// A file exists in the install directory that the manifest never
    /// recorded -- added post-install (e.g. a planted DLL), not merely
    /// missing or altered. Also reported for a symlink found anywhere in
    /// the tree (never followed): Task 2.4's extractor never creates one,
    /// so its mere presence means the directory was tampered with after
    /// install (Task 2.5 review round 2, finding 2).
    Unexpected(String),
    /// The check itself could not complete: a real I/O error (permissions,
    /// a failing disk, a directory that could not be listed) distinct from
    /// "the file is not there". Reported separately from [`Self::Missing`]
    /// so a caller does not steer a user toward a reinstall for a file
    /// that is actually present but unreadable, which a reinstall may not
    /// fix (Task 2.5 review finding 6).
    Io(String),
}

/// Re-verifies `dir` against `files` (normally
/// `crate::local::marker::Marker::files`, read back via
/// `crate::local::marker::read_marker`) in two passes:
///
/// 1. Every entry in `files` is checked present -> the recorded size -> the
///    recorded sha256, in that order, stopping at the first failure
///    (manifest order, which `crate::local::install::build_manifest` writes
///    sorted by path) -- one failure is already enough to distrust the
///    whole install.
/// 2. Every regular file actually present under `dir` is checked against
///    the same manifest, exempting only `crate::local::marker::MARKER_FILE`
///    -- the on-disk marker itself, written by
///    `crate::local::install::install` (via
///    `crate::local::marker::write_marker`) strictly AFTER
///    `crate::local::install::build_manifest` runs, so it is legitimately
///    not among its own entries. No other file is written to an install
///    directory after the manifest is built (checked against `install.rs`
///    at the time this was written); if a future change adds one, it must
///    be exempted here explicitly by name, not by loosening this check to
///    a subset comparison.
pub fn verify_manifest(dir: &Path, files: &[FileEntry]) -> Result<(), IntegrityError> {
    for f in files {
        let path = resolve(dir, &f.path);

        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(IntegrityError::Missing(f.path.clone()))
            }
            Err(e) => return Err(IntegrityError::Io(format!("{}: {e}", f.path))),
        };
        if !metadata.is_file() {
            return Err(IntegrityError::Missing(f.path.clone()));
        }
        if metadata.len() != f.size {
            return Err(IntegrityError::SizeMismatch(f.path.clone()));
        }

        let sha256 = match install::sha256_file_sync(&path) {
            Ok(h) => h,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(IntegrityError::Missing(f.path.clone()))
            }
            Err(e) => return Err(IntegrityError::Io(format!("{}: {e}", f.path))),
        };
        if sha256 != f.sha256 {
            return Err(IntegrityError::HashMismatch(f.path.clone()));
        }
    }

    let manifest_paths: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut on_disk = Vec::new();
    list_files_relative(dir, dir, &mut on_disk)?;
    for rel in on_disk {
        if rel == marker::MARKER_FILE {
            continue;
        }
        if !manifest_paths.contains(rel.as_str()) {
            return Err(IntegrityError::Unexpected(rel));
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

/// Recursively lists every regular file under `current` as a `/`-separated
/// path relative to `root`, appending to `out` -- the same convention
/// `FileEntry::path` and `crate::local::install::build_manifest` use, so
/// the result is directly comparable to a manifest's path set. Fails with
/// [`IntegrityError::Io`] (not `Missing`/`Unexpected`) if the directory
/// itself cannot be walked -- a check that could not even enumerate the
/// directory has learned nothing, which is not the same as "nothing extra
/// was found".
///
/// A symlink (to a file OR a directory) fails immediately as
/// [`IntegrityError::Unexpected`] rather than being silently skipped or
/// followed (Task 2.5 review round 2, finding 2). `DirEntry::file_type`
/// does not follow symlinks -- it reports the entry's OWN type -- so
/// without an explicit `is_symlink()` check, a symlink is neither `is_dir()`
/// nor `is_file()` and the original version of this function fell through
/// both branches and simply never saw it: invisible to the exact-set check
/// this function exists to run. `crate::local::install::extract_tar_gz`
/// already documents that Task 2.4's extractor skips symlink/hardlink tar
/// entries outright ("Whitelist, not blacklist... no engine archive needs
/// one"), so a correctly-extracted install can never legitimately contain
/// one; any symlink found here was planted after the fact. It must not be
/// followed either: a symlinked directory recursed into could point
/// anywhere else on disk, and a symlinked file's target is exactly the
/// "malicious DLL elsewhere, loaded via `cwd`-relative resolution" attack
/// this whole check exists to catch.
fn list_files_relative(
    root: &Path,
    current: &Path,
    out: &mut Vec<String>,
) -> Result<(), IntegrityError> {
    let entries = std::fs::read_dir(current)
        .map_err(|e| IntegrityError::Io(format!("{}: {e}", current.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| IntegrityError::Io(format!("{}: {e}", current.display())))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| IntegrityError::Io(format!("{}: {e}", path.display())))?;
        let rel = || {
            path.strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/")
        };
        if file_type.is_symlink() {
            return Err(IntegrityError::Unexpected(rel()));
        } else if file_type.is_dir() {
            list_files_relative(root, &path, out)?;
        } else if file_type.is_file() {
            out.push(rel());
        }
    }
    Ok(())
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
    fn passes_on_an_empty_manifest_and_empty_directory() {
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

    // --- exact-set verification (Task 2.5 review finding 5) -----------------

    #[test]
    fn detects_a_file_added_to_the_install_directory_that_the_manifest_never_recorded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server.exe"), b"binary-content").unwrap();
        // Planted after install: a file the manifest never recorded.
        std::fs::write(dir.path().join("evil.dll"), b"payload").unwrap();
        let files = vec![FileEntry {
            path: "llama-server.exe".to_string(),
            size: b"binary-content".len() as u64,
            sha256: sha256_hex(b"binary-content"),
        }];
        assert_eq!(
            verify_manifest(dir.path(), &files),
            Err(IntegrityError::Unexpected("evil.dll".to_string()))
        );
    }

    #[test]
    fn detects_an_added_file_inside_a_nested_directory_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("f"), b"x").unwrap();
        std::fs::write(dir.path().join("sub").join("planted"), b"y").unwrap();
        let files = vec![FileEntry {
            path: "sub/f".to_string(),
            size: 1,
            sha256: sha256_hex(b"x"),
        }];
        assert_eq!(
            verify_manifest(dir.path(), &files),
            Err(IntegrityError::Unexpected("sub/planted".to_string()))
        );
    }

    #[test]
    fn marker_json_itself_is_exempt_from_the_exact_set_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server.exe"), b"binary-content").unwrap();
        // marker.json is written by install::write_marker AFTER
        // install::build_manifest runs, so it is legitimately absent from
        // the manifest it describes.
        std::fs::write(dir.path().join(marker::MARKER_FILE), b"{}").unwrap();
        let files = vec![FileEntry {
            path: "llama-server.exe".to_string(),
            size: b"binary-content".len() as u64,
            sha256: sha256_hex(b"binary-content"),
        }];
        assert_eq!(verify_manifest(dir.path(), &files), Ok(()));
    }

    // --- symlinks (Task 2.5 review round 2, finding 2) -----------------------
    //
    // Symlink creation on Windows normally requires elevated privileges or
    // Developer Mode, so these are `#[cfg(unix)]`-only and do not run on
    // this development machine -- the production code path
    // (`FileType::is_symlink()`) is cross-platform and applies identically
    // on Windows, but is exercised here only on Unix. This joins the set of
    // platform-gated tests Task 5.4 must declare as not run on this host
    // (alongside the pre-existing Linux/macOS-only tests in `install.rs`).

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_inside_the_install_directory_is_flagged_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server"), b"binary-content").unwrap();

        // Points at a file entirely outside the install directory -- the
        // real attack shape (a malicious .so elsewhere on disk), not merely
        // a symlink to something harmless inside the same directory.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("evil.so"), b"payload").unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("evil.so"),
            dir.path().join("libggml-cuda.so"),
        )
        .unwrap();

        let files = vec![FileEntry {
            path: "llama-server".to_string(),
            size: b"binary-content".len() as u64,
            sha256: sha256_hex(b"binary-content"),
        }];
        assert_eq!(
            verify_manifest(dir.path(), &files),
            Err(IntegrityError::Unexpected("libggml-cuda.so".to_string()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_inside_the_install_directory_is_flagged_not_recursed_into() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llama-server"), b"binary-content").unwrap();

        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("inner"), b"z").unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("linked")).unwrap();

        let files = vec![FileEntry {
            path: "llama-server".to_string(),
            size: b"binary-content".len() as u64,
            sha256: sha256_hex(b"binary-content"),
        }];
        // Reports the symlink itself, "linked" -- never descends into it to
        // find (or miss) "inner".
        assert_eq!(
            verify_manifest(dir.path(), &files),
            Err(IntegrityError::Unexpected("linked".to_string()))
        );
    }

    // --- Io vs Missing (Task 2.5 review finding 6) ---------------------------

    #[cfg(windows)]
    #[test]
    fn a_file_that_exists_but_cannot_be_read_reports_io_not_missing() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.bin");
        std::fs::write(&path, b"content").unwrap();

        // share_mode(0): no other handle -- including our own subsequent
        // read inside verify_manifest -- may open this file while this one
        // is held. Windows fails that second open with
        // ERROR_SHARING_VIOLATION, a real I/O error distinct from "not
        // found".
        let _locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("should be able to open with an exclusive share mode");

        let files = vec![FileEntry {
            path: "locked.bin".to_string(),
            size: 7,
            sha256: sha256_hex(b"content"),
        }];
        let err = verify_manifest(dir.path(), &files).unwrap_err();
        assert!(
            matches!(err, IntegrityError::Io(_)),
            "a present-but-unreadable file must report Io, not Missing: {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_exists_but_cannot_be_read_reports_io_not_missing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.bin");
        std::fs::write(&path, b"content").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let files = vec![FileEntry {
            path: "locked.bin".to_string(),
            size: 7,
            sha256: sha256_hex(b"content"),
        }];
        let result = verify_manifest(dir.path(), &files);

        // Restore permissions so the tempdir can clean itself up.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));

        let err = result.unwrap_err();
        assert!(
            matches!(err, IntegrityError::Io(_)),
            "a present-but-unreadable file must report Io, not Missing: {err:?}"
        );
    }
}
