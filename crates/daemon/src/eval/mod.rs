//! NEVOFLUX_EVAL_MODE support — see docs/superpowers/specs/2026-05-15-browser-use-eval-design.md §7.2

pub mod config;
pub mod run_dir;

pub use config::{from_env, EvalConfig, EvalConfigError};
pub use run_dir::EvalRunDirs;

pub mod lock;
pub use lock::{read as read_lock, write_atomic as write_lock_atomic, DaemonLock, LockError};
