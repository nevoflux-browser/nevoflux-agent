//! Browser-facing `brain.*` RPC handlers for the M4-4 `nevoflux://brain`
//! page (and any other consumer that wants direct, non-agent-path access
//! to the knowledge base).
//!
//! Each handler:
//!   1. Reads the active [`crate::init_brain::SharedBrainSlot`] via
//!      [`crate::init_brain::CURRENT_BRAIN_SLOT`].
//!   2. If the slot is empty (`brain.enabled = false` OR the boot path
//!      hasn't run yet), returns `BRAIN_DISABLED` (or `ok=false` for
//!      `brain.health`, so the UI can render a disabled state without an
//!      error toast).
//!   3. Otherwise dispatches to the [`nevoflux_brain::BrainEngine`] trait
//!      method (or, for `brain.stats`, to the gbrain `get_stats` MCP tool
//!      via the live supervisor) and wraps the result in the
//!      `system_response` envelope.
//!
//! These handlers complement (but do not replace) the M3-4 brain tools
//! dispatched on the agent path; they are the minimum surface the
//! M4-4b `nevoflux://brain` page needs.

use std::sync::Arc;

use nevoflux_brain::{BrainEngine, BrainError};
use serde_json::Value;

use crate::gbrain::GbrainSupervisor;
use crate::init_brain::CURRENT_BRAIN_SLOT;
use crate::kb_wizard::{err_response, ok_response};

/// Look up the live brain engine handle. Returns `None` when the brain
/// slot is empty (either not initialized yet or
/// `knowledge_base.brain.enabled = false`).
async fn current_engine() -> Option<Arc<dyn BrainEngine>> {
    let slot = CURRENT_BRAIN_SLOT.get()?;
    let guard = slot.read().await;
    guard.as_ref().map(|boot| boot.engine.clone())
}

/// Look up the live supervisor handle. Used by `brain.stats` to dispatch
/// gbrain MCP tools that the [`BrainEngine`] trait doesn't expose
/// directly.
async fn current_supervisor() -> Option<Arc<GbrainSupervisor>> {
    let slot = CURRENT_BRAIN_SLOT.get()?;
    let guard = slot.read().await;
    guard.as_ref().map(|boot| boot.supervisor.clone())
}

/// Standard error envelope returned by `brain.stats` / `brain.list` /
/// `brain.get` when the brain slot is empty.
fn brain_disabled_err(request_id: &str, command: &str) -> Value {
    err_response(
        request_id,
        command,
        "BRAIN_DISABLED",
        "knowledge_base.brain is not enabled or the daemon hasn't \
         finished initializing the brain yet",
    )
}

/// `brain.health` — one-shot readiness probe.
///
/// Returns `ok=true` when the brain slot holds a live engine; `ok=false`
/// (still a *success* envelope) when the slot is empty so the UI can
/// render a "disabled" badge instead of an error toast.
pub async fn handle_health(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let Some(_engine) = current_engine().await else {
        return ok_response(
            request_id,
            "brain.health",
            serde_json::json!({
                "ok": false,
                "brain_dir": null,
                "reason": "brain_disabled",
                "last_check_ts": chrono::Utc::now().timestamp(),
            }),
        );
    };

    // Engine is live. We don't ping it here (a real probe would race
    // with the supervisor's restart watchdog); presence-in-slot is
    // sufficient signal for the M4-4 UI.
    let brain_dir = dirs::home_dir()
        .map(|h| h.join(".gbrain").display().to_string())
        .unwrap_or_default();

    ok_response(
        request_id,
        "brain.health",
        serde_json::json!({
            "ok": true,
            "brain_dir": brain_dir,
            "last_check_ts": chrono::Utc::now().timestamp(),
        }),
    )
}

/// `brain.stats` — page / chunk / embedded counts.
///
/// The [`BrainEngine`] trait doesn't expose a dedicated stats method, so
/// this handler reaches into the supervisor and calls gbrain's
/// `get_stats` MCP tool directly. The tool returns a JSON-stringified
/// payload inside `result.content[0].text`; we parse it back to a JSON
/// object before returning to the browser.
pub async fn handle_stats(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let Some(supervisor) = current_supervisor().await else {
        return brain_disabled_err(request_id, "brain.stats");
    };

    let result = supervisor
        .call_tool("get_stats", serde_json::json!({}))
        .await;

    match result {
        Ok(envelope) => {
            // gbrain returns the MCP `tools/call` shape:
            // { result: { content: [{ type: "text", text: "<json>" }], ... } }
            // where `text` is a JSON-stringified stats object. We parse
            // it back so the browser sees structured data instead of a
            // string.
            let stats = envelope
                .get("result")
                .and_then(|r| r.get("content"))
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|first| {
                    first
                        .get("data")
                        .cloned()
                        .or_else(|| {
                            first
                                .get("text")
                                .and_then(|t| t.as_str())
                                .and_then(|s| serde_json::from_str(s).ok())
                        })
                })
                .unwrap_or_else(|| serde_json::json!({}));
            ok_response(request_id, "brain.stats", stats)
        }
        Err(e) => err_response(
            request_id,
            "brain.stats",
            "BRAIN_BACKEND_ERROR",
            format!("get_stats failed: {e}"),
        ),
    }
}

/// `brain.list` — list page metadata for the sidebar.
///
/// `dir` defaults to the brain root (`""`).
pub async fn handle_list(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let dir = params.get("dir").and_then(|v| v.as_str()).unwrap_or("");

    let Some(engine) = current_engine().await else {
        return brain_disabled_err(request_id, "brain.list");
    };

    match engine.list(dir).await {
        Ok(pages) => ok_response(
            request_id,
            "brain.list",
            serde_json::json!({ "pages": pages }),
        ),
        Err(e) => err_response(
            request_id,
            "brain.list",
            "BRAIN_BACKEND_ERROR",
            format!("list failed: {e}"),
        ),
    }
}

/// `brain.get` — fetch a single page's compiled markdown by slug.
pub async fn handle_get(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let slug = match params.get("slug").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => {
            return err_response(
                request_id,
                "brain.get",
                "BAD_REQUEST",
                "missing or empty `slug`",
            );
        }
    };

    let Some(engine) = current_engine().await else {
        return brain_disabled_err(request_id, "brain.get");
    };

    match engine.get(slug).await {
        Ok(page) => ok_response(
            request_id,
            "brain.get",
            serde_json::to_value(&page).unwrap_or_else(|_| serde_json::json!({})),
        ),
        Err(BrainError::NotFound(_)) => err_response(
            request_id,
            "brain.get",
            "BRAIN_NOT_FOUND",
            format!("page `{slug}` not found"),
        ),
        Err(e) => err_response(
            request_id,
            "brain.get",
            "BRAIN_BACKEND_ERROR",
            format!("get failed: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: CURRENT_BRAIN_SLOT is a process-global `OnceLock`. In the
    // test binary nothing initializes it, so `current_engine()` /
    // `current_supervisor()` both return `None`. These tests verify the
    // disabled-path branches — the happy-path branches require a live
    // gbrain subprocess and are deferred to manual end-to-end smoke.

    #[tokio::test]
    async fn health_returns_disabled_when_brain_slot_empty() {
        let resp = handle_health(&serde_json::json!({ "request_id": "test-1" })).await;
        assert_eq!(resp["type"], "system_response");
        assert_eq!(resp["payload"]["success"], true);
        // ok=false reflects the brain_disabled status.
        assert_eq!(resp["payload"]["data"]["ok"], false);
        assert_eq!(resp["payload"]["data"]["reason"], "brain_disabled");
    }

    #[tokio::test]
    async fn stats_returns_error_envelope_when_brain_disabled() {
        let resp = handle_stats(&serde_json::json!({ "request_id": "test-2" })).await;
        assert_eq!(resp["type"], "system_response");
        assert_eq!(resp["payload"]["success"], false);
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[tokio::test]
    async fn list_returns_error_envelope_when_brain_disabled() {
        let resp = handle_list(&serde_json::json!({ "request_id": "test-3" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[tokio::test]
    async fn get_returns_bad_request_when_slug_missing() {
        let resp = handle_get(&serde_json::json!({ "request_id": "test-4" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BAD_REQUEST");
    }

    #[tokio::test]
    async fn get_returns_disabled_when_slug_present_but_brain_off() {
        let resp = handle_get(&serde_json::json!({
            "request_id": "test-5",
            "slug": "test-page"
        }))
        .await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }
}
