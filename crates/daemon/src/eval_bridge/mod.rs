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
        }
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
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        // Stub handler returns 501 NOT_IMPLEMENTED; that proves bearer passed through middleware.
        assert_eq!(resp.status().as_u16(), 501);
    }
}
