//! Delegation mode (M3): a paired agent hands NevoFlux a whole task.
//!
//! The task runs as an ordinary Agent-mode turn in a background session of
//! the desktop sidebar (decision C4), bounded by the pairing's execution tier
//! instead of the channel whitelist (C1). What is specific to that lives here:
//! which sessions a pairing started and what they may do (`origin`), and the
//! numbers it all runs on (`limits`).

pub mod answer;
pub mod ceiling;
pub mod desktop;
pub mod engine;
pub mod host;
pub mod limits;
pub mod origin;
pub mod record;
pub mod screenshot;
pub mod tools;

/// `params.purpose` on an `ask_user` request that asks permission for an
/// action (tier gate, Jev flag, site/pack rule, ACP permission request), as
/// opposed to the model's own question (contract R13). The extension ignores
/// it; the task engine reads it to tell approval kinds apart.
pub const PERMISSION_PURPOSE: &str = "permission";

/// `params.must_approve: true` on the permission prompt Task 5's stage raises
/// for a must-approve tool in a delegated turn (contract v1.2 §2.1). It says
/// what the question is, fixed when it is asked; who may answer it is read
/// from the pairing at each answer (R39). The extension ignores it.
pub const MUST_APPROVE_KEY: &str = "must_approve";

use std::sync::{Arc, OnceLock};

static ENGINE: OnceLock<Arc<engine::TaskEngine>> = OnceLock::new();

/// Publish the daemon's task engine. Called once at startup; later calls are
/// ignored.
pub fn install(engine: Arc<engine::TaskEngine>) {
    let _ = ENGINE.set(engine);
}

/// The daemon's task engine, once startup has published it.
pub fn engine() -> Option<Arc<engine::TaskEngine>> {
    ENGINE.get().cloned()
}

/// The chat tap's hook (`server.rs` writer task).
pub fn observe(payload: &serde_json::Value) {
    if let Some(engine) = ENGINE.get() {
        engine.observe(payload);
    }
}

/// The `browser_tool_response` hook (`server.rs` message loop).
pub fn observe_browser_response(session_id: &str, result: Option<&serde_json::Value>) {
    if let Some(engine) = ENGINE.get() {
        engine.observe_browser_response(session_id, result);
    }
}

/// Called as a chat turn begins (`handle_chat_message_streaming`).
pub fn begin_turn(session_id: &str) -> engine::TurnStart {
    match ENGINE.get() {
        Some(engine) => engine.begin_turn(session_id),
        None => engine::TurnStart::Run(engine::TurnGuard::untracked()),
    }
}

/// The hold for questions from `session_id`, when a delegated task runs there.
pub fn hold_for(session_id: &str) -> Option<std::time::Duration> {
    ENGINE.get().and_then(|engine| engine.hold_for(session_id))
}

/// Cancel a pairing's running task (pause, unpair).
pub async fn cancel_for_pairing(pairing: &str) {
    if let Some(engine) = engine() {
        engine.cancel_for_pairing(pairing).await;
    }
}
