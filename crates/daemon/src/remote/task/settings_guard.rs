//! A delegated turn cannot write NevoFlux's own settings (final review I3,
//! user decision 2026-10-17).
//!
//! In a session a paired agent started, the model's path-writing tools
//! (`write_file`, `edit_file`) refuse any path inside the daemon's config
//! directory (`config.toml`, skills, packs) or data directory
//! (`agent-pairings.json`, the database), resolved the way the daemon
//! resolves them ([`crate::paths::resolve_from_daemon`]). Otherwise a
//! full-auto pairing could raise its own tier or turn on its own R39 toggle
//! by editing the file the desktop reads them from. `bash` / `run_command`
//! are deliberately not restricted: full-auto is full access, and the
//! contract says so; this closes the obvious door, not every door.
//!
//! Paths are compared after resolving them component by component — each
//! existing prefix canonicalised (symlinks, junctions, 8.3 names) before the
//! next `..` is applied — and, on Windows, without regard to case.

use std::path::{Component, Path, PathBuf};

use nevoflux_storage::Database;

/// The error a refused write gets (exact).
pub const REFUSAL: &str =
    "A task delegated by a paired agent cannot change NevoFlux's own settings or pairings.";

/// The directories a delegated turn may not write into.
pub fn protected_dirs() -> Vec<PathBuf> {
    let paths = crate::paths::resolve_from_daemon();
    vec![paths.config_dir, paths.data_dir]
}

/// Refuse `path` when `session_id` is a delegated session and the path is
/// inside the daemon's own directories.
pub fn check_write(db: &Database, session_id: &str, path: &Path) -> Result<(), String> {
    check_write_in(db, session_id, path, &protected_dirs())
}

/// [`check_write`] against `dirs`.
pub fn check_write_in(
    db: &Database,
    session_id: &str,
    path: &Path,
    dirs: &[PathBuf],
) -> Result<(), String> {
    if super::origin::delegated_pairing(db, session_id).is_none() {
        return Ok(());
    }
    if is_inside_any(path, dirs) {
        return Err(REFUSAL.to_string());
    }
    Ok(())
}

/// Whether `path` resolves inside one of `dirs`.
pub fn is_inside_any(path: &Path, dirs: &[PathBuf]) -> bool {
    let target = comparable(&resolve(path));
    dirs.iter()
        .any(|dir| target.starts_with(comparable(&resolve(dir))))
}

/// `path` made absolute and resolved: every existing prefix canonicalised
/// before the next component is applied, so `link/..` means what the OS
/// means by it; the part that does not exist yet is applied lexically.
fn resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => {
                out.push(part);
                if let Ok(real) = std::fs::canonicalize(&out) {
                    out = strip_verbatim(real);
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\h\s` → `\\h\s` (what `canonicalize` returns
/// on Windows), so canonical and lexical paths compare.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

/// The form two resolved paths are compared in: case-folded on Windows.
fn comparable(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delegated_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        super::super::origin::create_delegated(&db, "s-del", "chan-1").unwrap();
        db
    }

    #[test]
    fn a_delegated_write_to_the_pairings_file_is_refused() {
        let db = delegated_db();
        let data = tempfile::tempdir().unwrap();
        let dirs = vec![data.path().to_path_buf()];
        let pairings = data.path().join("agent-pairings.json");
        assert_eq!(
            check_write_in(&db, "s-del", &pairings, &dirs).unwrap_err(),
            "A task delegated by a paired agent cannot change NevoFlux's own settings or pairings."
        );
    }

    #[test]
    fn a_delegated_write_elsewhere_and_a_local_write_are_left_alone() {
        let db = delegated_db();
        let data = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let dirs = vec![data.path().to_path_buf()];
        assert!(check_write_in(&db, "s-del", &elsewhere.path().join("notes.txt"), &dirs).is_ok());
        let pairings = data.path().join("agent-pairings.json");
        assert!(
            check_write_in(&db, "s-own", &pairings, &dirs).is_ok(),
            "a local session is not judged by a pairing"
        );
    }

    #[test]
    fn dot_dot_and_case_do_not_get_around_it() {
        let data = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let dirs = vec![data.path().join("cfg")];
        std::fs::create_dir_all(&dirs[0]).unwrap();
        let sneaky = elsewhere
            .path()
            .join("..")
            .join(data.path().file_name().unwrap())
            .join("x")
            .join("..")
            .join("cfg")
            .join("config.toml");
        assert!(is_inside_any(&sneaky, &dirs), "{sneaky:?}");
        assert!(!is_inside_any(
            &data.path().join("cfg-other").join("a"),
            &dirs
        ));
        if cfg!(windows) {
            let upper = PathBuf::from(dirs[0].to_string_lossy().to_uppercase()).join("config.toml");
            assert!(is_inside_any(&upper, &dirs), "{upper:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_the_directory_is_followed() {
        let data = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = elsewhere.path().join("link");
        std::os::unix::fs::symlink(data.path(), &link).unwrap();
        assert!(is_inside_any(
            &link.join("agent-pairings.json"),
            &[data.path().to_path_buf()]
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_into_the_directory_is_followed() {
        let data = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = elsewhere.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(data.path())
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        assert!(is_inside_any(
            &link.join("agent-pairings.json"),
            &[data.path().to_path_buf()]
        ));
    }
}
