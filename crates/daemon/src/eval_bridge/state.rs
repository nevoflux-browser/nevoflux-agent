//! Shared state for eval bridge handlers.

use crate::session::SessionManager;
use std::sync::Arc;

#[derive(Clone)]
pub struct EvalAppState {
    pub session_manager: Arc<SessionManager>,
    pub bearer_token: Arc<str>,
    pub eval_run_id: Arc<str>,
}
