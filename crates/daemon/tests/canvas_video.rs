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

#[tokio::test]
async fn test_render_start_creates_job_and_returns_id() {
    use nevoflux_daemon::canvas_video::job::JobState;
    use nevoflux_protocol::canvas_video::RenderStartRequest;

    let svc = fresh_service();
    let create_resp = svc
        .create_composition(CreateCompositionRequest {
            title: "demo".into(),
            width: 640,
            height: 360,
            duration_sec: 1.0,
            fps: 30,
            bg: None,
            html: Some(
                r#"<!doctype html><div id="stage" data-width="640" data-height="360" data-duration="1" data-fps="30"></div>"#
                    .into(),
            ),
        })
        .await
        .unwrap();

    let start_resp = svc
        .render_start(RenderStartRequest {
            composition_id: create_resp.artifact_id.clone(),
        })
        .await
        .unwrap();

    assert!(start_resp.job_id.starts_with("job-"));

    // Job is now tracked; state is Queued, Running, or Succeeded (stub
    // bridge short-circuits to Succeeded but the spawned task may run
    // before or after this observation).
    let snap = svc.job_snapshot(&start_resp.job_id).await.unwrap();
    assert!(matches!(
        snap.state,
        JobState::Queued | JobState::Running | JobState::Succeeded
    ));
    assert_eq!(snap.composition_id, create_resp.artifact_id);
    assert_eq!(snap.total_frames, 30);
}

#[tokio::test]
async fn test_render_start_rejects_unknown_composition() {
    use nevoflux_protocol::canvas_video::RenderStartRequest;

    let svc = fresh_service();
    let err = svc
        .render_start(RenderStartRequest {
            composition_id: "comp-does-not-exist".into(),
        })
        .await
        .unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("not found") || msg.contains("unknown"),
        "unexpected err: {}",
        msg
    );
}

/// End-to-end integration: drive the page-driven signal channel with fake frame
/// chunks + render_done, and assert ffmpeg produced a non-empty MP4.
///
/// Skipped silently when ffmpeg is unavailable so CI without ffmpeg on PATH
/// doesn't flake.
#[tokio::test]
async fn test_render_loop_reassembles_and_encodes() {
    use std::time::Duration;

    use ffmpeg_sidecar::command::ffmpeg_is_installed;
    use image::{ImageBuffer, Rgba};
    use nevoflux_daemon::canvas_video::job::JobState;
    use nevoflux_protocol::canvas_video::{RenderFrameChunk, RenderStartRequest};

    if !ffmpeg_is_installed() {
        eprintln!("ffmpeg not on PATH — skipping test_render_loop_reassembles_and_encodes");
        return;
    }

    // Production service (no stub short-circuit) so the full render loop runs.
    let svc = Arc::new(CanvasVideoService::new());

    // Keep the render tiny so the test finishes in well under a second:
    // 12 frames at 24 fps = 0.5 s composition (the configured minimum).
    let fps: u32 = 24;
    let duration_sec: f32 = 0.5;
    let total_frames: u32 = (duration_sec * fps as f32).ceil() as u32;

    let create_resp = svc
        .create_composition(CreateCompositionRequest {
            title: "integration".into(),
            width: 16,
            height: 16,
            duration_sec,
            fps,
            bg: None,
            html: Some("<!doctype html><body></body>".into()),
        })
        .await
        .unwrap();

    let start_resp = svc
        .render_start(RenderStartRequest {
            composition_id: create_resp.artifact_id.clone(),
        })
        .await
        .unwrap();
    let job_id = start_resp.job_id.clone();

    // Build a 16x16 PNG with varying color per frame to avoid degenerate encoder paths.
    let make_png = |frame: u32| -> Vec<u8> {
        let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(16, 16);
        let r = (frame * 40) as u8;
        for (_, _, px) in img.enumerate_pixels_mut() {
            *px = Rgba([r, 128, 255 - r, 255]);
        }
        let mut buf: Vec<u8> = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Png,
        )
        .unwrap();
        buf
    };

    // Give the render loop a tick to register its signal channel before we
    // start pushing frames. The channel is registered inside the spawned task.
    tokio::time::sleep(Duration::from_millis(50)).await;

    for frame_idx in 0..total_frames {
        let png = make_png(frame_idx);
        // Send as a single chunk (is_last=true, total_chunks=1).
        svc.on_frame_chunk(RenderFrameChunk {
            job_id: job_id.clone(),
            frame_idx,
            chunk_idx: 0,
            total_chunks: 1,
            is_last: true,
            bytes: png,
        })
        .await
        .unwrap();
    }

    svc.on_render_done(&job_id, total_frames).await;

    // Poll until the job is Succeeded or Failed (max ~15s — ffmpeg startup
    // + 5 tiny PNGs should finish in under a second).
    let mut final_state = JobState::Running;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(snap) = svc.job_snapshot(&job_id).await {
            match snap.state {
                JobState::Succeeded | JobState::Failed | JobState::Cancelled => {
                    final_state = snap.state;
                    break;
                }
                _ => {}
            }
        }
    }

    assert_eq!(
        final_state,
        JobState::Succeeded,
        "job did not reach Succeeded (final={:?})",
        final_state
    );

    // Verify the MP4 was written and is non-trivially sized.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let output_path = std::path::PathBuf::from(home)
        .join(".cache")
        .join("nevoflux")
        .join("render")
        .join(format!("{}.mp4", job_id));
    let metadata = std::fs::metadata(&output_path)
        .unwrap_or_else(|e| panic!("output not found at {:?}: {}", output_path, e));
    assert!(
        metadata.len() > 0,
        "output MP4 is empty at {:?}",
        output_path
    );

    // Best-effort cleanup; don't fail the test if removal fails.
    let _ = std::fs::remove_file(&output_path);
}
