//! Daemon HTTP client.
//!
//! Bridges from the eval runner to the daemon's `NEVOFLUX_EVAL_MODE` HTTP
//! bridge (see spec §6.2 + Phase 1 commit `5ddf63e`).

pub mod lock;

// http and sse modules land in Tasks 6 and 7 — declarations added then.

pub use lock::{wait_for_lock, DaemonLock, LockError};
