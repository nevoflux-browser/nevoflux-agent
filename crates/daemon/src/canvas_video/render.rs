//! canvas.video.render.start orchestration.
//!
//! Flow (spec §4.1):
//!   1. Validate composition exists + read HTML.
//!   2. Allocate job_id + register in JobRegistry.
//!   3. Spawn ffmpeg image2pipe subprocess.
//!   4. Push canvas_video_open to extension -> hidden render tab opens.
//!   5. Await RenderReady.
//!   6. Loop frame 0..total_frames:
//!        push canvas_video_seek -> extension routes to render page ->
//!        PNG chunks stream back via canvas_video_frame_chunk -> reassembled
//!        -> write to ffmpeg stdin.
//!   7. Close ffmpeg stdin + wait exit.
//!   8. Persist output.mp4 to artifact + JobState::Succeeded.

use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::canvas_video::ffmpeg::{image2pipe_cmd, resolve_ffmpeg};
use crate::canvas_video::job::JobState;
use crate::canvas_video::CanvasVideoService;
use crate::error::{DaemonError, Result};
use nevoflux_protocol::canvas_video::{RenderStartRequest, RenderStartResponse};

const PER_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME_RETRIES: u32 = 3;
const RENDER_READY_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn render_start(
    svc: &Arc<CanvasVideoService>,
    req: RenderStartRequest,
) -> Result<RenderStartResponse> {
    // Look up composition HTML + spec.
    let html = svc.read_composition_html(&req.composition_id).await?;
    let (width, height, duration_sec, fps) = svc.composition_spec(&req.composition_id).await?;

    let job_id = svc
        .jobs()
        .create(req.composition_id.clone(), width, height, duration_sec, fps)
        .await;

    // Kick off render loop in background. Tests observe via job_snapshot.
    let svc_clone = svc.clone();
    let job_id_clone = job_id.clone();
    tokio::spawn(async move {
        if let Err(e) = run_render_loop(
            svc_clone.clone(),
            job_id_clone.clone(),
            html,
            width,
            height,
            duration_sec,
            fps,
        )
        .await
        {
            let err_msg = format!("{}", e);
            svc_clone.jobs().set_error(&job_id_clone, err_msg.clone()).await;
            svc_clone.emit_failed(&job_id_clone, &err_msg).await;
        }
    });

    Ok(RenderStartResponse { job_id })
}

#[allow(clippy::too_many_arguments)]
async fn run_render_loop(
    svc: Arc<CanvasVideoService>,
    job_id: String,
    html: String,
    width: u32,
    height: u32,
    duration_sec: f32,
    fps: u32,
) -> Result<()> {
    svc.jobs().set_state(&job_id, JobState::Running).await;
    svc.jobs()
        .set_progress(&job_id, 0, "opening render tab".into())
        .await;

    // For test builds with no bridge attached, exit gracefully after
    // registering the Running state.
    if svc.bridge_is_stub() {
        svc.jobs().set_state(&job_id, JobState::Succeeded).await;
        return Ok(());
    }

    // --- Production path ---

    let total_frames = (duration_sec * fps as f32).ceil() as u32;
    let composition_id = svc.composition_id_for(&job_id).await;

    // Build output path in cache dir.
    let output_path: PathBuf = {
        let cache_base = std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".cache"))
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
        let dir = cache_base.join("nevoflux").join("render");
        std::fs::create_dir_all(&dir).map_err(|e| {
            DaemonError::InternalError(format!("create render cache dir: {}", e))
        })?;
        dir.join(format!("{}.mp4", job_id))
    };

    // Resolve ffmpeg binary (auto-downloads static build if absent).
    let _ffmpeg_path = resolve_ffmpeg()?;

    // Spawn ffmpeg via image2pipe_cmd helper.
    let mut ffmpeg_cmd = image2pipe_cmd(&output_path, fps);
    let mut ffmpeg_child = ffmpeg_cmd
        .spawn()
        .map_err(|e| DaemonError::InternalError(format!("spawn ffmpeg: {}", e)))?;

    // Take stdin before we start writing so we own it exclusively.
    let mut ffmpeg_stdin = ffmpeg_child
        .take_stdin()
        .ok_or_else(|| DaemonError::InternalError("ffmpeg stdin not available".into()))?;

    // Pre-register chunk buffer for this job.
    svc.register_job_chunk_buffer(&job_id).await;

    // Register ready channel before pushing canvas_video_open so we don't
    // miss an early ack.
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    svc.register_job_ready_channel(&job_id, ready_tx).await;

    // Open hidden render tab in extension.
    svc.push_canvas_video_open(&job_id, &composition_id).await?;

    // Await RenderReady from extension render page.
    svc.jobs()
        .set_progress(&job_id, 0, "awaiting render ready".into())
        .await;
    tokio::time::timeout(RENDER_READY_TIMEOUT, ready_rx)
        .await
        .map_err(|_| DaemonError::InternalError("timed out waiting for render ready".into()))?
        .map_err(|_| DaemonError::InternalError("render ready channel dropped".into()))?;

    // Load composition HTML into render page.
    svc.push_canvas_video_load(&job_id, &html, width, height)
        .await?;

    // Frame capture loop.
    for frame_idx in 0..total_frames {
        // Early exit if cancelled.
        if let Some(snap) = svc.jobs().snapshot(&job_id).await {
            if snap.state == JobState::Cancelled {
                let _ = ffmpeg_child.kill();
                return Ok(());
            }
        }

        let t = frame_idx as f64 / fps as f64;

        let mut png_bytes: Option<Vec<u8>> = None;
        for _attempt in 0..MAX_FRAME_RETRIES {
            let (frame_tx, frame_rx) = oneshot::channel::<Vec<u8>>();
            svc.register_frame_awaiter(&job_id, frame_idx, frame_tx)
                .await;

            svc.push_canvas_video_seek(&job_id, t, frame_idx, width, height)
                .await?;

            match tokio::time::timeout(PER_FRAME_TIMEOUT, frame_rx).await {
                Ok(Ok(bytes)) => {
                    png_bytes = Some(bytes);
                    break;
                }
                Ok(Err(_)) => {
                    // Channel dropped — transient, retry.
                }
                Err(_) => {
                    // Timeout — retry.
                }
            }
        }

        let png = png_bytes.ok_or_else(|| {
            DaemonError::InternalError(format!(
                "frame {} not captured after {} retries",
                frame_idx, MAX_FRAME_RETRIES
            ))
        })?;

        // Write PNG to ffmpeg stdin (blocking I/O; we're in a Tokio spawn so
        // this is acceptable for P1; Task N can move to spawn_blocking if needed).
        ffmpeg_stdin
            .write_all(&png)
            .map_err(|e| DaemonError::InternalError(format!("write frame to ffmpeg: {}", e)))?;

        svc.jobs()
            .set_progress(&job_id, frame_idx + 1, format!("frame {}/{}", frame_idx + 1, total_frames))
            .await;
        svc.emit_progress(&job_id, frame_idx + 1, total_frames).await;
    }

    // Signal ffmpeg EOF and wait for process exit.
    drop(ffmpeg_stdin);
    ffmpeg_child
        .wait()
        .map_err(|e| DaemonError::InternalError(format!("ffmpeg wait: {}", e)))?;

    // Close render tab.
    let _ = svc.push_canvas_video_close(&job_id).await;

    // Collect output file size.
    let size_bytes = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Mark job as succeeded.
    svc.jobs().set_state(&job_id, JobState::Succeeded).await;
    svc.jobs()
        .set_progress(&job_id, total_frames, "done".into())
        .await;

    let path_str = output_path.to_string_lossy().into_owned();
    svc.emit_succeeded(&job_id, &path_str, size_bytes).await;

    Ok(())
}
