//! The task's result picture: one system-origin screenshot (contract R22).
//!
//! Sent as an ordinary `browser_tool_request` with `origin: "system"`, which
//! the extension handles itself (never the sidebar) and which bypasses the
//! agent tab gate — the agent channel's own calls cannot reach `nevoflux://`
//! Canvas tabs, and that stays so (spec §5). The daemon does not resize or
//! re-encode anything; it asks for a bounded JPEG and checks what came back.

use nevoflux_protocol::BrowserToolAction;
use serde_json::{json, Value};

use super::limits::{
    SCREENSHOT_MAX_BYTES, SCREENSHOT_MAX_HEIGHT, SCREENSHOT_MAX_WIDTH, SCREENSHOT_QUALITY,
    SCREENSHOT_REQUEST_MS, SCREENSHOT_RETRY_QUALITY,
};
use super::record::Target;
use crate::wasm::services::{BrowserRequest, BrowserResponse};

/// `origin` of the request: the daemon itself, not the turn or an agent.
pub const SYSTEM_ORIGIN: &str = "system";

/// The failure messages the extension may answer with (R22).
pub const ERRORS: &[&str] = &["tab_discarded", "tab_closed", "too_large", "capture_failed"];

/// What to photograph: what the task opened or generated, else the tab it
/// last acted on, else nothing (spec §3.3).
pub fn choose(produced: Option<&Target>, last_tab: Option<i64>) -> Option<Target> {
    produced.cloned().or_else(|| last_tab.map(Target::Tab))
}

/// The request (R22), addressed to the sidebar connection `proxy_id`.
pub fn request(target: &Target, session_id: &str, proxy_id: &str) -> BrowserRequest {
    let mut params = json!({
        "purpose": "task_result",
        "format": "jpeg",
        "quality": SCREENSHOT_QUALITY,
        "retry_quality": SCREENSHOT_RETRY_QUALITY,
        "max_width": SCREENSHOT_MAX_WIDTH,
        "max_height": SCREENSHOT_MAX_HEIGHT,
        "max_bytes": SCREENSHOT_MAX_BYTES,
    });
    let tab_id = match target {
        Target::Tab(id) => Some(*id),
        Target::Artifact(id) => {
            params["artifact_id"] = json!(id);
            None
        }
    };
    BrowserRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.to_string(),
        tab_id,
        action: BrowserToolAction::Screenshot,
        params,
        timeout_ms: SCREENSHOT_REQUEST_MS,
        client_identity: proxy_id.as_bytes().to_vec(),
        proxy_id: proxy_id.to_string(),
        origin: Some(SYSTEM_ORIGIN.to_string()),
    }
}

/// The base64 JPEG, or the `screenshot_error` to report. `None` means the
/// browser did not answer in time.
pub fn outcome(response: Option<BrowserResponse>) -> Result<String, &'static str> {
    let Some(response) = response else {
        return Err("timeout");
    };
    if !response.success {
        let message = response.error.map(|e| e.message).unwrap_or_default();
        return Err(ERRORS
            .iter()
            .copied()
            .find(|e| *e == message)
            .unwrap_or("capture_failed"));
    }
    let result = response.result.unwrap_or(Value::Null);
    let Some(data) = result
        .get("data")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    else {
        return Err("capture_failed");
    };
    if result
        .get("mime_type")
        .and_then(Value::as_str)
        .is_some_and(|m| m != "image/jpeg")
    {
        return Err("capture_failed");
    }
    // Either measure can be off; neither may exceed the cap.
    let claimed = result.get("bytes").and_then(Value::as_u64).unwrap_or(0);
    let max_base64 = SCREENSHOT_MAX_BYTES.div_ceil(3) * 4;
    if claimed > SCREENSHOT_MAX_BYTES || data.len() as u64 > max_base64 {
        return Err("too_large");
    }
    Ok(data.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_protocol::BrowserToolError;

    #[test]
    fn a_canvas_is_named_by_its_artifact_and_a_web_tab_by_its_id() {
        let canvas = request(&Target::Artifact("calc-1".into()), "s1", "sidebar-1");
        assert_eq!(canvas.action, BrowserToolAction::Screenshot);
        assert_eq!(canvas.tab_id, None);
        assert_eq!(canvas.origin.as_deref(), Some("system"));
        assert_eq!(canvas.session_id, "s1");
        assert_eq!(canvas.proxy_id, "sidebar-1");
        assert_eq!(canvas.client_identity, b"sidebar-1".to_vec());
        assert_eq!(canvas.timeout_ms, 30_000);
        assert_eq!(
            canvas.params,
            json!({"purpose": "task_result", "artifact_id": "calc-1", "format": "jpeg", "quality": 60,
                   "retry_quality": 40, "max_width": 1280, "max_height": 2400, "max_bytes": 307200})
        );
        let web = request(&Target::Tab(41), "s1", "sidebar-1");
        assert_eq!(web.tab_id, Some(41));
        assert!(web.params.get("artifact_id").is_none());
    }

    #[test]
    fn the_target_is_what_the_task_made_then_what_it_touched() {
        let made = Target::Artifact("calc-1".into());
        assert_eq!(choose(Some(&made), Some(41)), Some(made));
        assert_eq!(choose(None, Some(41)), Some(Target::Tab(41)));
        assert_eq!(choose(None, None), None);
    }

    fn ok(result: Value) -> Option<BrowserResponse> {
        Some(BrowserResponse {
            request_id: "r".into(),
            success: true,
            result: Some(result),
            error: None,
        })
    }

    fn failed(message: &str) -> Option<BrowserResponse> {
        Some(BrowserResponse {
            request_id: "r".into(),
            success: false,
            result: None,
            error: Some(BrowserToolError {
                code: -1,
                message: message.into(),
                recoverable: false,
            }),
        })
    }

    #[test]
    fn an_answer_is_an_image_or_one_of_the_contracts_reasons() {
        let jpeg = json!({"mime_type": "image/jpeg", "data": "/9j/4AAQ", "width": 1280,
                          "height": 800, "bytes": 6, "tab_kind": "canvas"});
        assert_eq!(outcome(ok(jpeg)), Ok("/9j/4AAQ".to_string()));
        for reason in ["tab_discarded", "tab_closed", "too_large", "capture_failed"] {
            assert_eq!(outcome(failed(reason)), Err(reason));
        }
        assert_eq!(outcome(failed("Tab not found")), Err("capture_failed"));
        assert_eq!(outcome(None), Err("timeout"));
        assert_eq!(
            outcome(ok(json!({"mime_type": "image/jpeg", "data": ""}))),
            Err("capture_failed")
        );
        assert_eq!(
            outcome(ok(json!({"mime_type": "image/png", "data": "iVBO"}))),
            Err("capture_failed")
        );
        assert_eq!(
            outcome(ok(
                json!({"mime_type": "image/jpeg", "data": "/9j/", "bytes": 307_201})
            )),
            Err("too_large")
        );
        assert_eq!(
            outcome(ok(
                json!({"mime_type": "image/jpeg", "data": "A".repeat(409_604)})
            )),
            Err("too_large"),
            "an under-reported size is caught by the length"
        );
    }
}
