//! Per-task profile management (P5): clone a named base-profile into an
//! ephemeral copy (login state carried), inject the headless automation pref,
//! and clean up after the task.
//!
//! The base profile is a first-class credential resource (a human logs in once
//! into `base-profiles/<name>/`); each task runs on a throwaway clone so tasks
//! sharing a base share only what that base already implies (login + tenant
//! brain), and never pollute the base.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic sequence for unique clone directory names (no time/rand — keeps
/// behavior deterministic and replay-safe).
static CLONE_SEQ: AtomicU64 = AtomicU64::new(0);

/// The exact pref line the extension reads pre-connection (see plan P1).
const AUTOMATION_PREF_LINE: &str = "user_pref(\"nevoflux.headless.automation\", true);\n";

/// Error managing profiles.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// Filesystem error while copying/injecting/cleaning.
    #[error("profile io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Clones base-profiles into ephemeral per-task copies.
pub struct ProfileManager {
    /// Directory containing `base-profiles/<name>/`.
    pub base_dir: PathBuf,
    /// Directory under which ephemeral clones are created.
    pub work_dir: PathBuf,
}

impl ProfileManager {
    /// Clone `base_name` from `base_dir` into a fresh dir under `work_dir`.
    /// A missing base (blank profile) yields an empty clone dir (allowed).
    pub fn clone_base(&self, base_name: &str) -> Result<PathBuf, ProfileError> {
        let seq = CLONE_SEQ.fetch_add(1, Ordering::Relaxed);
        let clone = self.work_dir.join(format!("{base_name}-{seq}"));
        std::fs::create_dir_all(&clone)?;
        let base = self.base_dir.join(base_name);
        if base.is_dir() {
            copy_dir_all(&base, &clone)?;
        }
        Ok(clone)
    }

    /// Append the automation pref to `<clone>/user.js` (creating it if absent).
    pub fn inject_automation_pref(&self, clone: &Path) -> Result<(), ProfileError> {
        use std::io::Write;
        let user_js = clone.join("user.js");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(user_js)?;
        f.write_all(AUTOMATION_PREF_LINE.as_bytes())?;
        Ok(())
    }

    /// Remove an ephemeral clone (best-effort; ignores if already gone).
    pub fn cleanup(&self, clone: &Path) {
        let _ = std::fs::remove_dir_all(clone);
    }
}

/// Recursively copy `src` into `dst` (dirs + files).
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_copies_base_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base-profiles/acme");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("cookies.sqlite"), b"x").unwrap();
        let pm = ProfileManager {
            base_dir: tmp.path().join("base-profiles"),
            work_dir: tmp.path().join("work"),
        };
        let clone = pm.clone_base("acme").unwrap();
        assert!(clone.join("cookies.sqlite").exists());
        assert_ne!(clone, base);
    }

    #[test]
    fn user_js_gets_automation_pref() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("clone");
        std::fs::create_dir_all(&clone).unwrap();
        let pm = ProfileManager {
            base_dir: tmp.path().into(),
            work_dir: tmp.path().into(),
        };
        pm.inject_automation_pref(&clone).unwrap();
        let s = std::fs::read_to_string(clone.join("user.js")).unwrap();
        assert!(s.contains(r#"user_pref("nevoflux.headless.automation", true);"#));
    }

    #[test]
    fn blank_base_yields_empty_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let pm = ProfileManager {
            base_dir: tmp.path().join("base-profiles"),
            work_dir: tmp.path().join("work"),
        };
        let clone = pm.clone_base("does-not-exist").unwrap();
        assert!(clone.is_dir());
    }
}
