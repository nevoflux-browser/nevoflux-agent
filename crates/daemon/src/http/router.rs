//! axum HTTP router + server for the headless task API (P4).
//!
//! Wires routes to the in-daemon [`TaskQueue`] + [`Metrics`]. The task `Runner`
//! (the P3 automation session runner) is injected into the queue; the router is
//! agnostic to it. Its end-to-end behavior (submit → run → browser → result) is
//! verified only with a live browser (phase gate); the route wiring compiles
//! against the already-tested queue/metrics.

use crate::http::metrics::Metrics;
use crate::http::queue::TaskQueue;
use crate::http::types::{TaskRequest, TaskStatus};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Shared state for the HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    /// The task queue (submit / status / cancel).
    pub queue: Arc<TaskQueue>,
    /// Process metrics.
    pub metrics: Arc<Metrics>,
}

/// Build the task-API router (task submit/status/cancel/events, metrics, and the
/// OpenAI-compatible chat endpoint on the same port).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/tasks", post(submit_task))
        .route("/tasks/:id", get(get_task).delete(cancel_task))
        .route("/tasks/:id/events", get(task_events))
        .route("/metrics", get(metrics_handler))
        .merge(openai_routes())
        .with_state(state)
}

/// OpenAI-compatible routes, unstated so the caller applies state once. For a
/// dedicated port: `openai_routes().with_state(state)`.
pub fn openai_routes() -> Router<AppState> {
    Router::new().route("/v1/chat/completions", post(chat_completions))
}

/// Bind `addr` and serve `app` until the process exits.
pub async fn serve(addr: SocketAddr, app: Router) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await
}

async fn submit_task(
    State(s): State<AppState>,
    Json(req): Json<TaskRequest>,
) -> impl IntoResponse {
    let id = s.queue.submit(req);
    (StatusCode::OK, Json(serde_json::json!({ "id": id })))
}

async fn get_task(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match s.queue.status(&id) {
        Some(r) => (StatusCode::OK, Json(r)).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown task").into_response(),
    }
}

async fn cancel_task(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    // Queue-level cancel (marks the task Failed). Cooperative interrupt of a
    // running attempt is delivered by the session runner (P3 Task 6).
    if s.queue.cancel(&id) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn metrics_handler(State(s): State<AppState>) -> impl IntoResponse {
    s.metrics.render()
}

/// SSE: stream a task's status snapshots until it reaches a terminal state.
/// Emits a `status` event on each change (and the terminal one), keep-alive
/// comments in between. `GET /tasks/:id/events`.
async fn task_events(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let queue = s.queue.clone();
    let stream = futures::stream::unfold(
        (queue, id, false, None::<TaskStatus>),
        |(queue, id, done, last)| async move {
            if done {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            match queue.status(&id) {
                None => {
                    let ev = Event::default().event("error").data("unknown task");
                    Some((Ok(ev), (queue, id, true, last)))
                }
                Some(r) => {
                    let terminal = matches!(r.status, TaskStatus::Succeeded | TaskStatus::Failed);
                    if last != Some(r.status) || terminal {
                        let data = serde_json::to_string(&r).unwrap_or_default();
                        let ev = Event::default().event("status").data(data);
                        Some((Ok(ev), (queue, id, terminal, Some(r.status))))
                    } else {
                        let ev = Event::default().comment("waiting");
                        Some((Ok(ev), (queue, id, false, Some(r.status))))
                    }
                }
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---- OpenAI-compatible chat completions -------------------------------------

#[derive(Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatCompletionRequest {
    #[serde(default)]
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    #[allow(dead_code)]
    stream: bool,
}

/// OpenAI-compatible `POST /v1/chat/completions`. The last `user` message becomes
/// a browser task (mode/profile/policy from env via [`TaskRequest::from_env`]);
/// the agent runs it and its answer is returned as the assistant message.
/// Non-streaming.
async fn chat_completions(
    State(s): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    let task = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    if task.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": {"message": "no user message"}})),
        )
            .into_response();
    }
    let treq = TaskRequest::from_env(task);
    let resp = s
        .queue
        .submit_and_wait(treq, Duration::from_secs(600))
        .await;
    let content = resp
        .output
        .clone()
        .or_else(|| resp.error.clone())
        .unwrap_or_default();
    let finish = if resp.status == TaskStatus::Succeeded {
        "stop"
    } else {
        "error"
    };
    let body = serde_json::json!({
        "id": format!("chatcmpl-{}", resp.id),
        "object": "chat.completion",
        "model": if req.model.is_empty() { "nevoflux-headless".to_string() } else { req.model },
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": finish
        }]
    });
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::queue::Runner;
    use crate::http::types::{TaskResponse, TaskStatus};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let runner: Runner = Arc::new(|id, _req| {
            Box::pin(async move {
                TaskResponse {
                    id,
                    status: TaskStatus::Succeeded,
                    attempts: 1,
                    output: Some("ok".into()),
                    error: None,
                    artifacts: vec![],
                }
            })
        });
        AppState {
            queue: Arc::new(TaskQueue::new(runner)),
            metrics: Arc::new(Metrics::default()),
        }
    }

    #[tokio::test]
    async fn post_task_get_unknown_and_metrics() {
        let app = router(test_state());

        // POST /tasks → 200 + { id: "task-N" }
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"task":"open example.com"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["id"].as_str().unwrap().starts_with("task-"));

        // GET /tasks/unknown → 404
        let r404 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tasks/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r404.status(), StatusCode::NOT_FOUND);

        // GET /metrics → 200 + Prometheus text
        let rm = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rm.status(), StatusCode::OK);
        let mbytes = rm.into_body().collect().await.unwrap().to_bytes();
        let mtext = String::from_utf8(mbytes.to_vec()).unwrap();
        assert!(mtext.contains("nevoflux_tasks_total"));
    }
}
