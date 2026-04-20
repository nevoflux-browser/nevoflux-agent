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

#[tokio::test]
async fn test_job_registry_create_and_lookup() {
    use nevoflux_daemon::canvas_video::job::{JobRegistry, JobState};

    let reg = JobRegistry::new();
    let job_id = reg.create("comp-xyz".into(), 1920, 1080, 5.0, 30).await;
    assert!(job_id.starts_with("job-"));

    let snapshot = reg.snapshot(&job_id).await.expect("job exists");
    assert_eq!(snapshot.state, JobState::Queued);
    assert_eq!(snapshot.composition_id, "comp-xyz");
    assert_eq!(snapshot.total_frames, 150); // 5s × 30fps
}

#[tokio::test]
async fn test_job_state_transitions() {
    use nevoflux_daemon::canvas_video::job::{JobRegistry, JobState};

    let reg = JobRegistry::new();
    let job_id = reg.create("comp-a".into(), 640, 360, 1.0, 30).await;

    reg.set_state(&job_id, JobState::Running).await;
    assert_eq!(reg.snapshot(&job_id).await.unwrap().state, JobState::Running);

    reg.set_progress(&job_id, 15, "encoding frame 15/30".into()).await;
    let s = reg.snapshot(&job_id).await.unwrap();
    assert_eq!(s.current_frame, 15);
    assert_eq!(s.step, "encoding frame 15/30");

    reg.set_state(&job_id, JobState::Succeeded).await;
    assert_eq!(reg.snapshot(&job_id).await.unwrap().state, JobState::Succeeded);
}

#[tokio::test]
async fn test_job_cancel_transitions_to_cancelled() {
    use nevoflux_daemon::canvas_video::job::{JobRegistry, JobState};

    let reg = JobRegistry::new();
    let job_id = reg.create("comp-a".into(), 640, 360, 1.0, 30).await;
    reg.set_state(&job_id, JobState::Running).await;
    let cancelled = reg.cancel(&job_id).await;
    assert!(cancelled);
    assert_eq!(reg.snapshot(&job_id).await.unwrap().state, JobState::Cancelled);
}
