//! Eval bridge route handlers — see spec §6.2.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde_json::Value;
use super::state::EvalAppState;

pub async fn create_session(State(_s): State<EvalAppState>, Json(_body): Json<Value>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "create_session — see Task 10")
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
