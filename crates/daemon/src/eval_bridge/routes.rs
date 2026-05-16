//! Eval bridge route handlers — see spec §6.2.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use futures::StreamExt;
use nevoflux_storage::MessageRole;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::pin::Pin;
use std::str::FromStr;
use super::sse::{to_sse, EvalEvent};
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

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SetupStep {
    /// Insert a prior conversation message into the session history.
    InjectMessage { role: String, content: String },
    /// Pre-populate a memory entry visible to this session.
    SeedMemory { key: String, value: String },
    /// Grant a permission upfront so the agent can use a gated tool.
    GrantPermission { tool: String },
}

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub steps: Vec<SetupStep>,
}

#[derive(Debug, Serialize)]
pub struct SetupResponse {
    pub applied: usize,
}

pub async fn create_session(
    State(state): State<EvalAppState>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, (StatusCode, String)> {
    // Default to "chat" if mode is not specified.
    let mode_str = body.mode.as_deref().unwrap_or("chat");

    if body.llm_backend.is_some() || body.mock_browser.is_some() {
        tracing::warn!(
            llm_backend = ?body.llm_backend,
            mock_browser = ?body.mock_browser,
            "eval-bridge: llm_backend/mock_browser not yet wired; fields ignored (phase-2)"
        );
    }

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
        // TODO(phase-2): "browser" mode deferred — SessionManager has no Browser variant yet.
        // Currently only "chat" and "agent" are supported.
        "browser" => {
            return Err((
                StatusCode::BAD_REQUEST,
                "browser mode not yet supported (phase-2 — SessionManager needs Browser variant)".into(),
            ));
        }
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
    State(state): State<EvalAppState>,
    Path(session_id): Path<String>,
    Json(body): Json<SetupRequest>,
) -> Result<Json<SetupResponse>, (StatusCode, String)> {
    let mut applied = 0usize;

    for step in body.steps {
        match step {
            SetupStep::InjectMessage { role, content } => {
                let message_role = MessageRole::from_str(&role).map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("inject_message: unknown role {role:?}; expected user|assistant|system"),
                    )
                })?;
                state
                    .session_manager
                    .add_message(&session_id, message_role, &content)
                    .await
                    .map_err(|e| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("inject_message: {e}"),
                        )
                    })?;
            }
            SetupStep::SeedMemory { key, value } => {
                // No SessionManager API for memory seeding yet (phase-2).
                tracing::warn!(
                    %session_id, %key, value_len = value.len(),
                    "eval-bridge: SeedMemory step not yet wired (phase-2)"
                );
            }
            SetupStep::GrantPermission { tool } => {
                // No SessionManager API for upfront permission grant yet (phase-2).
                tracing::warn!(
                    %session_id, %tool,
                    "eval-bridge: GrantPermission step not yet wired (phase-2)"
                );
            }
        }
        applied += 1;
    }

    Ok(Json(SetupResponse { applied }))
}

#[derive(Debug, Deserialize)]
pub struct SubmitMessageRequest {
    pub prompt: String,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct SubmitMessageResponse {
    pub accepted: bool,
}

pub async fn submit_message(
    State(state): State<EvalAppState>,
    Path(session_id): Path<String>,
    Json(body): Json<SubmitMessageRequest>,
) -> Result<Json<SubmitMessageResponse>, (StatusCode, String)> {
    // Persist user message so the turn appears in session history / trace replay.
    state
        .session_manager
        .add_message(&session_id, MessageRole::User, &body.prompt)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("add_message: {e}"),
            )
        })?;

    if let Some(t) = body.timeout_secs {
        tracing::debug!(
            session_id = %session_id,
            timeout_secs = t,
            "eval submit timeout (informational; eval client owns timeout)"
        );
    }

    // Dispatch to the daemon's agent loop — non-blocking.
    // The receiver lives in start_server (wired in Task 16) where the full
    // AgentConfig + HostServices machinery is available.
    // In test contexts the sender is None; we still return accepted: true.
    if let Some(ref tx) = state.agent_turn_tx {
        if let Err(e) = tx.send(super::state::AgentTurnRequest {
            session_id: session_id.clone(),
            prompt: body.prompt.clone(),
        }) {
            tracing::error!(
                session_id = %session_id,
                error = %e,
                "eval agent_turn_tx send failed — daemon agent loop may have shut down"
            );
        }
    } else {
        tracing::debug!(
            session_id = %session_id,
            "eval agent_turn_tx not wired (test mode); skipping dispatch"
        );
    }

    tracing::info!(
        run_id = %state.eval_run_id,
        session_id = %session_id,
        prompt_len = body.prompt.len(),
        "eval submitted message"
    );

    Ok(Json(SubmitMessageResponse { accepted: true }))
}

pub async fn stream_events(
    State(state): State<EvalAppState>,
    Path(session_id): Path<String>,
) -> axum::response::Response {
    use crate::event_bus::{BackpressurePolicy, SubscriberIdentity};
    use crate::event_bus::types::TopicPattern;

    let eval_stream: Pin<Box<dyn futures::Stream<Item = EvalEvent> + Send>> =
        match state.event_bus.as_ref() {
            None => {
                // Unit-test / no-bus context: emit a single phase-2 placeholder and close.
                let placeholder = futures::stream::once(async {
                    EvalEvent::Error {
                        message: "event_bus not wired (phase-2): connect Arc<EventBus> via \
                                  EvalAppState::event_bus to enable real event streaming"
                            .into(),
                    }
                });
                Box::pin(placeholder)
            }
            Some(bus) => {
                // Subscribe as Internal so permission checks pass for all topic prefixes.
                // Use a broad wildcard and filter client-side by session_id embedded in
                // the event payload (phase-2: narrow to per-session topics when they land).
                let sid = session_id.clone();
                match bus.subscribe(
                    TopicPattern::double_wildcard(""),  // all topics
                    SubscriberIdentity::Internal,
                    BackpressurePolicy::DropNewest,
                    256,
                ) {
                    Err(e) => {
                        let msg = format!("subscribe failed: {e}");
                        Box::pin(futures::stream::once(async move {
                            EvalEvent::Error { message: msg }
                        }))
                    }
                    Ok(handle) => {
                        // Convert the mpsc::Receiver<BusEvent> into a Stream.
                        let rx_stream =
                            tokio_stream::wrappers::ReceiverStream::new(handle.rx);

                        // Filter events that carry this session_id in their payload,
                        // then map the raw BusEvent to an EvalEvent.
                        //
                        // TODO(phase-2): When dedicated `agent:token`, `agent:tool_call`,
                        // `agent:tool_result`, and `agent:turn_done` topics exist, add
                        // typed variants (Token, ToolCall, ToolResult, Stop) here.
                        // For now we forward all matching events as DaemonEvent so that
                        // the wire format and content-type are correct and SSE consumers
                        // can at least inspect raw bus traffic.
                        let mapped = rx_stream.filter_map(move |bus_evt| {
                            let sid = sid.clone();
                            async move {
                                // Only forward events whose payload contains the
                                // matching session_id (or that have no session_id
                                // field, which we skip to avoid flooding the client).
                                let payload_sid = bus_evt
                                    .payload
                                    .get("session_id")
                                    .and_then(|v| v.as_str());
                                if payload_sid != Some(sid.as_str()) {
                                    return None;
                                }
                                Some(EvalEvent::DaemonEvent {
                                    name: bus_evt.topic,
                                    payload: bus_evt.payload,
                                })
                            }
                        });
                        Box::pin(mapped)
                    }
                }
            }
        };

    to_sse(eval_stream).into_response()
}

pub async fn stream_traces(
    State(state): State<EvalAppState>,
    Path(session_id): Path<String>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::header;

    let spans = match &state.trace_collector {
        Some(tc) => match tc.traces_for_session(&session_id) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("traces: {e}"),
                )
                    .into_response();
            }
        },
        None => {
            // Test-mode or trace collector not yet wired — return empty body.
            tracing::debug!(
                session_id = %session_id,
                "no trace collector wired; returning empty JSONL"
            );
            vec![]
        }
    };

    let mut body = String::with_capacity(spans.len() * 128);
    for span in &spans {
        body.push_str(&serde_json::to_string(span).unwrap_or_else(|_| "{}".into()));
        body.push('\n');
    }

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/jsonl")
        .body(Body::from(body))
        .unwrap()
}

pub async fn delete_session(State(_s): State<EvalAppState>, Path(_id): Path<String>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "delete_session — see Task 15")
}
