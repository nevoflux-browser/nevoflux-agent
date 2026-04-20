//! End-to-end tests for canvas.video.* bridge handlers.

use std::sync::Arc;

use nevoflux_daemon::canvas_video::CanvasVideoService;
use nevoflux_protocol::canvas_video::{CreateCompositionRequest, CreateCompositionResponse};

fn fresh_service() -> Arc<CanvasVideoService> {
    // Test service uses in-memory artifact repo + temp dir for outputs.
    Arc::new(CanvasVideoService::new_for_tests())
}

#[tokio::test]
async fn test_create_composition_returns_artifact_id() {
    let svc = fresh_service();
    let req = CreateCompositionRequest {
        title: "demo".into(),
        width: 1920,
        height: 1080,
        duration_sec: 5.0,
        fps: 30,
        bg: None,
        html: None,
    };
    let resp: CreateCompositionResponse = svc.create_composition(req).await.unwrap();
    assert!(!resp.artifact_id.is_empty());
    assert!(resp.artifact_id.starts_with("comp-"));
}

#[tokio::test]
async fn test_create_composition_rejects_invalid_fps() {
    let svc = fresh_service();
    let req = CreateCompositionRequest {
        title: "bad".into(),
        width: 1920,
        height: 1080,
        duration_sec: 5.0,
        fps: 60, // invalid
        bg: None,
        html: None,
    };
    let err = svc.create_composition(req).await.unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("fps") || msg.contains("60"),
        "error should mention fps: {}",
        msg
    );
}

#[tokio::test]
async fn test_create_composition_rejects_duration_over_60s() {
    let svc = fresh_service();
    let req = CreateCompositionRequest {
        title: "bad".into(),
        width: 1920,
        height: 1080,
        duration_sec: 120.0,
        fps: 30,
        bg: None,
        html: None,
    };
    let err = svc.create_composition(req).await.unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("duration") || msg.contains("60"),
        "error should mention duration: {}",
        msg
    );
}
