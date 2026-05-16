//! HTTP eval bridge — see spec §6.2.
//!
//! Bound to `127.0.0.1:0` (OS-assigned port), bearer-token authed.
//! Path prefix: `/_eval/`.

pub mod routes;
pub mod sse;
pub mod state;

pub use state::EvalAppState;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
    Router,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;

pub fn build_router(state: EvalAppState) -> Router {
    Router::new()
        .route("/_eval/sessions", post(routes::create_session))
        .route("/_eval/sessions/:id/setup", post(routes::setup_session))
        .route("/_eval/sessions/:id/messages", post(routes::submit_message))
        .route("/_eval/sessions/:id/events", get(routes::stream_events))
        .route("/_eval/sessions/:id/traces", get(routes::stream_traces))
        .route("/_eval/sessions/:id", delete(routes::delete_session))
        .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
        .with_state(state)
}

/// Spawn the bridge listener. Returns the bound `SocketAddr` so the caller
/// can write it into daemon.lock.
pub async fn spawn(state: EvalAppState) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let router = build_router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(error = %e, "eval bridge listener stopped");
        }
    });
    Ok(addr)
}

async fn bearer_auth(
    State(state): State<EvalAppState>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let expected = format!("Bearer {}", state.bearer_token);
    if auth == Some(expected.as_str()) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use std::sync::Arc;

    fn test_state() -> EvalAppState {
        EvalAppState {
            session_manager: Arc::new(SessionManager::in_memory().expect("in-memory SM")),
            bearer_token: Arc::from("secret-test-token"),
            eval_run_id: Arc::from("run-test"),
            // No real agent loop in unit-test context; submit_message still returns accepted: true.
            agent_turn_tx: None,
            // No EventBus in unit-test context; stream_events emits a phase-2 placeholder.
            event_bus: None,
            // No TraceCollector in unit-test context; stream_traces returns empty JSONL.
            trace_collector: None,
        }
    }

    /// Create a session and return its ID. Used by multiple tests to avoid
    /// repeating the inline boilerplate.
    async fn create_test_session(client: &reqwest::Client, addr: std::net::SocketAddr) -> String {
        client
            .post(format!("http://{}/_eval/sessions", addr))
            .bearer_auth("secret-test-token")
            .json(&serde_json::json!({ "mode": "chat" }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
            .get("session_id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn bridge_rejects_missing_bearer() {
        let state = test_state();
        let addr = spawn(state).await.unwrap();
        let url = format!("http://{}/_eval/sessions", addr);
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn bridge_accepts_valid_bearer_and_reaches_handler() {
        let state = test_state();
        let addr = spawn(state).await.unwrap();
        let url = format!("http://{}/_eval/sessions", addr);
        let resp = reqwest::Client::new()
            .post(&url)
            .bearer_auth("secret-test-token")
            .json(&serde_json::json!({ "mode": "chat" }))
            .send()
            .await
            .unwrap();
        // Handler is now implemented — 200 proves bearer passed through middleware.
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn create_session_returns_id() {
        let state = test_state();
        let addr = spawn(state).await.unwrap();
        let client = reqwest::Client::new();
        let body: serde_json::Value = client
            .post(format!("http://{}/_eval/sessions", addr))
            .bearer_auth("secret-test-token")
            .json(&serde_json::json!({ "mode": "chat" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(body.get("session_id").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn setup_returns_applied_count() {
        let state = test_state();
        let addr = spawn(state).await.unwrap();
        let client = reqwest::Client::new();
        let sid = create_test_session(&client, addr).await;

        let resp = client
            .post(format!("http://{}/_eval/sessions/{}/setup", addr, sid))
            .bearer_auth("secret-test-token")
            .json(&serde_json::json!({
                "steps": [
                    { "type": "inject_message", "role": "user", "content": "hi" }
                ]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["applied"], 1);
    }

    #[tokio::test]
    async fn submit_message_returns_accepted() {
        let state = test_state();
        let addr = spawn(state).await.unwrap();
        let client = reqwest::Client::new();
        let sid = create_test_session(&client, addr).await;

        let resp = client
            .post(format!("http://{}/_eval/sessions/{}/messages", addr, sid))
            .bearer_auth("secret-test-token")
            .json(&serde_json::json!({ "prompt": "hello" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["accepted"], true);
    }

    #[tokio::test]
    async fn events_endpoint_returns_sse_content_type() {
        let state = test_state();
        let addr = spawn(state).await.unwrap();
        let client = reqwest::Client::new();
        let sid = create_test_session(&client, addr).await;
        let resp = client
            .get(format!("http://{}/_eval/sessions/{}/events", addr, sid))
            .bearer_auth("secret-test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/event-stream"),
            "expected SSE content-type, got: {ct}"
        );
    }

    #[tokio::test]
    async fn traces_endpoint_returns_jsonl() {
        let state = test_state();
        let addr = spawn(state).await.unwrap();
        let client = reqwest::Client::new();
        let sid = create_test_session(&client, addr).await;
        let resp = client
            .get(format!("http://{}/_eval/sessions/{}/traces", addr, sid))
            .bearer_auth("secret-test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("application/jsonl"),
            "expected application/jsonl content-type, got: {ct}"
        );
        // Empty session: empty body or trailing newline only.
        let body = resp.text().await.unwrap();
        assert!(
            body.is_empty() || body.ends_with('\n'),
            "unexpected body for empty trace: {body:?}"
        );
    }
}
