//! Eval bridge route handlers — see spec §6.2.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use super::state::EvalAppState;

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    /// "chat" | "agent"
    pub mode: Option<String>,
    pub llm_backend: Option<String>,
    pub mock_browser: Option<bool>,
    /// Echoed in handler logs for traceability; runner uses to scope state.
    pub eval_run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
}

pub async fn create_session(
    State(state): State<EvalAppState>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, (StatusCode, String)> {
    // Default to "chat" if mode is not specified.
    let mode_str = body.mode.as_deref().unwrap_or("chat");

    let session = match mode_str {
        "agent" => state
            .session_manager
            .create_agent_session(None, None)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("create_session: {e}")))?,
        "chat" => state
            .session_manager
            .create_session(None, None)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("create_session: {e}")))?,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown mode: {other:?}; expected \"chat\" or \"agent\""),
            ));
        }
    };

    let session_id = session.id.clone();

    tracing::info!(
        run_id = %state.eval_run_id,
        session_id = %session_id,
        mode = %mode_str,
        "eval created session"
    );

    Ok(Json(CreateSessionResponse { session_id }))
}

pub async fn setup_session(
    State(_s): State<EvalAppState>,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "setup_session — see Task 11")
}

pub async fn submit_message(
    State(_s): State<EvalAppState>,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "submit_message — see Task 12")
}

pub async fn stream_events(State(_s): State<EvalAppState>, Path(_id): Path<String>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "stream_events — see Task 13")
}

pub async fn stream_traces(State(_s): State<EvalAppState>, Path(_id): Path<String>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "stream_traces — see Task 14")
}

pub async fn delete_session(State(_s): State<EvalAppState>, Path(_id): Path<String>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "delete_session — see Task 15")
}
