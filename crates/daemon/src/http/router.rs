//! axum HTTP router + server for the headless task API (P4).
//!
//! Wires routes to the in-daemon [`TaskQueue`] + [`Metrics`]. The task `Runner`
//! (the P3 automation session runner) is injected into the queue; the router is
//! agnostic to it. Its end-to-end behavior (submit → run → browser → result) is
//! verified only with a live browser (phase gate); the route wiring compiles
//! against the already-tested queue/metrics.

use crate::http::metrics::Metrics;
use crate::http::queue::TaskQueue;
use crate::http::types::TaskRequest;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use std::net::SocketAddr;
use std::sync::Arc;

/// Shared state for the HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    /// The task queue (submit / status / cancel).
    pub queue: Arc<TaskQueue>,
    /// Process metrics.
    pub metrics: Arc<Metrics>,
}

/// Build the task-API router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/tasks", post(submit_task))
        .route("/tasks/:id", get(get_task).delete(cancel_task))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
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
