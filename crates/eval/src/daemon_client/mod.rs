//! Daemon HTTP client.
//!
//! Bridges from the eval runner to the daemon's `NEVOFLUX_EVAL_MODE` HTTP
//! bridge (see spec §6.2 + Phase 1 commit `5ddf63e`).

pub mod lock;

pub mod http;

pub use http::{DaemonHttpClient, HttpError};
pub use lock::{wait_for_lock, DaemonLock, LockError};
