//! Daemon HTTP client.
//!
//! Bridges from the eval runner to the daemon's `NEVOFLUX_EVAL_MODE` HTTP
//! bridge (see spec §6.2 + Phase 1 commit `5ddf63e`).

pub mod lock;

pub mod http;

pub mod sse;
pub use sse::{stream_events, SseError};

pub mod traces;
pub use traces::{event_names, parse_jsonl, TraceEntry, TracesParseError};

pub use http::{DaemonHttpClient, HttpError};
pub use lock::{wait_for_lock, DaemonLock, LockError};
