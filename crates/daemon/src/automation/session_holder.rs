//! Process-global holder for the ONE reused browser session used by
//! `NEVOFLUX_SESSION_MODE`. Mirrors `crate::registry::CURRENT_BROWSER_REGISTRY`:
//! a single `OnceLock`-initialised handle both the task runner and the
//! `/session/close` HTTP handler share. The inner `tokio::sync::Mutex`
//! serializes task execution and teardown so they never race.

use crate::browser_launch::BrowserHandle;
use crate::profile::ProfileManager;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

/// The live, reused browser session (present only in session mode, between the
/// first task and the flow's end signal).
pub struct LiveSession {
    /// The launcher child handle (for teardown).
    pub handle: BrowserHandle,
    /// The cloned profile dir shared by every task in this flow.
    pub clone_dir: PathBuf,
    /// Base-profile name this flow was cloned from.
    pub base_profile: String,
}

/// Process-global slot for the current session. `None` = no live session.
#[derive(Clone)]
pub struct SessionHolder {
    pub inner: Arc<Mutex<Option<LiveSession>>>,
}

static CURRENT_SESSION: OnceLock<SessionHolder> = OnceLock::new();

impl SessionHolder {
    /// The process-global holder, created once on first access.
    pub fn global() -> &'static SessionHolder {
        CURRENT_SESSION.get_or_init(|| SessionHolder {
            inner: Arc::new(Mutex::new(None)),
        })
    }
}

/// Tear the live session down (if any) and clear the slot: reap the child, kill
/// every process still holding the clone profile, then delete the clone dir.
/// Caller holds the `inner` lock and passes the guard.
pub async fn teardown_locked(guard: &mut Option<LiveSession>, pm: &ProfileManager) {
    if let Some(mut s) = guard.take() {
        s.handle.terminate().await;
        crate::browser_launch::kill_profile_processes(&s.clone_dir).await;
        pm.cleanup(&s.clone_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_is_stable_and_starts_empty() {
        let a = SessionHolder::global();
        let b = SessionHolder::global();
        // Same process-global instance (same Arc).
        assert!(std::sync::Arc::ptr_eq(&a.inner, &b.inner));
        // Starts with no live session.
        assert!(a.inner.try_lock().unwrap().is_none());
    }
}
