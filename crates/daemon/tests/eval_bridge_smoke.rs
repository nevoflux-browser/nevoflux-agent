//! End-to-end smoke test for the eval HTTP bridge.
//!
//! Drives an in-process daemon eval bridge with reqwest. Asserts the happy
//! path through: create → setup → submit → events → traces → delete + bearer.

use nevoflux_daemon::eval_bridge::{spawn, EvalAppState};
use nevoflux_daemon::session::SessionManager;
use std::sync::Arc;

#[tokio::test]
async fn end_to_end_eval_bridge_happy_path() {
    let state = EvalAppState {
        session_manager: Arc::new(SessionManager::in_memory().unwrap()),
        bearer_token: Arc::from("smoke-token"),
        eval_run_id: Arc::from("run-smoke"),
        agent_turn_tx: None,
        event_bus: None,
        trace_collector: None,
        agent_dispatch: None,
    };
    let addr = spawn(state).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}/_eval", addr);

    // Create
    let r = client
        .post(format!("{}/sessions", base))
        .bearer_auth("smoke-token")
        .json(&serde_json::json!({ "mode": "chat" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "create session");
    let sid = r.json::<serde_json::Value>().await.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Setup
    let r = client
        .post(format!("{}/sessions/{}/setup", base, sid))
        .bearer_auth("smoke-token")
        .json(&serde_json::json!({
            "steps": [
                { "type": "inject_message", "role": "user", "content": "warm-up" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "setup");

    // Submit
    let r = client
        .post(format!("{}/sessions/{}/messages", base, sid))
        .bearer_auth("smoke-token")
        .json(&serde_json::json!({ "prompt": "hello" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "submit");

    // Events (just verify content-type; no real model running here)
    let r = client
        .get(format!("{}/sessions/{}/events", base, sid))
        .bearer_auth("smoke-token")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "events");
    let ct = r
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/event-stream"), "events ct: {}", ct);
    drop(r); // close stream

    // Traces (empty body acceptable)
    let r = client
        .get(format!("{}/sessions/{}/traces", base, sid))
        .bearer_auth("smoke-token")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "traces");
    let ct = r
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("application/jsonl"), "traces ct: {}", ct);

    // Delete
    let r = client
        .delete(format!("{}/sessions/{}", base, sid))
        .bearer_auth("smoke-token")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204, "delete");

    // Bearer rejection sanity
    let r = client
        .post(format!("{}/sessions", base))
        .json(&serde_json::json!({ "mode": "chat" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401, "no-bearer rejection");
}
