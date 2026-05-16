//! Shared state for eval bridge handlers.

use crate::session::SessionManager;
use std::sync::Arc;

/// A request to run one agent turn, sent through the eval-bridge dispatch channel.
///
/// The channel receiver lives inside `start_server` (wired in Task 16) where the
/// full `AgentConfig` + `HostServices` machinery is available. The eval HTTP handler
/// only builds and posts this lightweight envelope; it never blocks on the turn.
#[derive(Debug)]
pub struct AgentTurnRequest {
    /// Session to run the turn on.
    pub session_id: String,
    /// User prompt for this turn.
    pub prompt: String,
}

#[derive(Clone)]
pub struct EvalAppState {
    pub session_manager: Arc<SessionManager>,
    pub bearer_token: Arc<str>,
    pub eval_run_id: Arc<str>,
    /// Channel into the daemon's main agent loop. `None` in unit-test contexts
    /// where the full daemon machinery is not available; in that case
    /// `submit_message` still returns `accepted: true` without dispatching.
    pub agent_turn_tx: Option<tokio::sync::mpsc::UnboundedSender<AgentTurnRequest>>,
}
