/// Per-run state directory layout:
///   <STATE_DIR>/nevoflux-eval/runs/<run_id>/
///     daemon.lock
///     daemon.log
///     traces.db
///
/// STATE_DIR resolution (via `directories` crate):
///   Linux:   $XDG_STATE_HOME or ~/.local/state
///   macOS:   ~/Library/Application Support
///   Windows: %LOCALAPPDATA%
///
/// Override: $NEVOFLUX_EVAL_STATE_DIR (used by tests + CI).
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct EvalRunDirs {
    pub root: PathBuf,
}

impl EvalRunDirs {
    pub fn resolve(run_id: &str) -> std::io::Result<Self> {
        let base = if let Some(override_dir) = std::env::var_os("NEVOFLUX_EVAL_STATE_DIR") {
            PathBuf::from(override_dir)
        } else {
            directories::ProjectDirs::from("com", "nevoflux", "nevoflux-eval")
                .map(|d| d.data_local_dir().to_path_buf())
                .unwrap_or_else(|| {
                    let home = std::env::var_os("HOME").unwrap_or_default();
                    PathBuf::from(home).join(".local/state/nevoflux-eval")
                })
        };
        let root = base.join("runs").join(run_id);
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }

    pub fn log_path(&self) -> PathBuf {
        self.root.join("daemon.log")
    }

    pub fn traces_db_path(&self) -> PathBuf {
        self.root.join("traces.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Shared lock so env-mutating tests don't race each other when run with
    // --test-threads=1 within this module or alongside config tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(*k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for (k, prev_val) in prev {
            match prev_val {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn resolve_creates_run_dir_under_override() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = tmp.path().to_str().unwrap().to_owned();
        with_env(
            &[("NEVOFLUX_EVAL_STATE_DIR", Some(tmp_path.as_str()))],
            || {
                let dirs = EvalRunDirs::resolve("run-test-001").unwrap();
                assert!(dirs.root.is_dir());
                assert!(dirs.root.ends_with("runs/run-test-001"));
                assert_eq!(dirs.lock_path(), dirs.root.join("daemon.lock"));
                assert_eq!(dirs.log_path(), dirs.root.join("daemon.log"));
                assert_eq!(dirs.traces_db_path(), dirs.root.join("traces.db"));
            },
        );
    }
}
