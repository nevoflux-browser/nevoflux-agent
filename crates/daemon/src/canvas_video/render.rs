//! canvas.video.render.start orchestration — page-driven flow.
//!
//! After the 2026-04-20 actor rework, the daemon no longer pushes seek
//! commands to the render page. The page drives the loop itself: it fetches
//! the composition spec, iterates `for i in 0..total_frames`, captures each
//! frame via `NevofluxBridge.canvasVideo.drawFrame`, and forwards PNG bytes
//! back as `canvas_video_frame_chunk`. When the last frame is sent it emits
//! `canvas_video_render_done`; on error it emits `canvas_video_render_failed`.
//!
//! This function's job reduces to:
//!   1. Validate composition exists.
//!   2. Allocate + track a JobId.
//!   3. Spawn an ffmpeg image2pipe subprocess.
//!   4. Drain a per-job signal channel, piping each complete PNG to ffmpeg
//!      in arrival order until a `Done` or `Failed` signal arrives (or the
//!      job is cancelled).
//!   5. Finalize the MP4 and publish progress / terminal events.

use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::canvas_video::ffmpeg::{image2pipe_cmd, resolve_ffmpeg};
use crate::canvas_video::job::JobState;
use crate::canvas_video::service::FrameSignal;
use crate::canvas_video::CanvasVideoService;
use crate::error::{DaemonError, Result};
use nevoflux_protocol::canvas_video::{RenderStartRequest, RenderStartResponse};

/// If the page goes silent for this long without completing the job, we
/// bail and mark the job failed. Pull-model render should never have a
/// frame-to-frame gap longer than a few seconds at 1080p.
const PAGE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn render_start(
    svc: &Arc<CanvasVideoService>,
    req: RenderStartRequest,
) -> Result<RenderStartResponse> {
    // Look up composition HTML + spec up front so a bad composition_id fails
    // the caller synchronously.
    let _html = svc.read_composition_html(&req.composition_id).await?;
    let (width, height, duration_sec, fps) = svc.composition_spec(&req.composition_id).await?;

    let job_id = svc
        .jobs()
        .create(req.composition_id.clone(), width, height, duration_sec, fps)
        .await;

    let svc_clone = svc.clone();
    let job_id_clone = job_id.clone();
    tokio::spawn(async move {
        if let Err(e) = run_render_loop(svc_clone.clone(), job_id_clone.clone(), fps).await {
            let err_msg = format!("{}", e);
            svc_clone
                .jobs()
                .set_error(&job_id_clone, err_msg.clone())
                .await;
            svc_clone.emit_failed(&job_id_clone, &err_msg).await;
        }
        svc_clone.cleanup_job_channels(&job_id_clone).await;
    });

    Ok(RenderStartResponse { job_id })
}

async fn run_render_loop(
    svc: Arc<CanvasVideoService>,
    job_id: String,
    fps: u32,
) -> Result<()> {
    svc.jobs().set_state(&job_id, JobState::Running).await;
    svc.jobs()
        .set_progress(&job_id, 0, "awaiting frames from render page".into())
        .await;

    // Register the signal channel BEFORE any frame chunk can arrive so we
    // never drop a chunk for a live job.
    let mut rx = svc.register_job_signal_channel(&job_id).await;

    // Stub mode: tests exercise state transitions without ffmpeg or a real
    // render page. Short-circuit straight to Succeeded as before.
    if svc.bridge_is_stub() {
        svc.jobs().set_state(&job_id, JobState::Succeeded).await;
        return Ok(());
    }

    // Output path: ~/.cache/nevoflux/render/<job_id>.mp4
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

    // Resolve ffmpeg binary (auto-downloads a static build if absent).
    let _ffmpeg_path = resolve_ffmpeg()?;

    let mut ffmpeg_cmd = image2pipe_cmd(&output_path, fps);
    let mut ffmpeg_child = ffmpeg_cmd
        .spawn()
        .map_err(|e| DaemonError::InternalError(format!("spawn ffmpeg: {}", e)))?;

    let mut ffmpeg_stdin = ffmpeg_child
        .take_stdin()
        .ok_or_else(|| DaemonError::InternalError("ffmpeg stdin not available".into()))?;

    // Expected frame count is derived from the composition spec so we can
    // surface progress ratios even though the page drives the loop.
    let snap = svc
        .jobs()
        .snapshot(&job_id)
        .await
        .ok_or_else(|| DaemonError::InternalError("job vanished from registry".into()))?;
    let total_frames = snap.total_frames;

    let mut frames_written: u32 = 0;

    loop {
        if let Some(snap) = svc.jobs().snapshot(&job_id).await {
            if snap.state == JobState::Cancelled {
                let _ = ffmpeg_child.kill();
                return Ok(());
            }
        }

        let sig = match tokio::time::timeout(PAGE_IDLE_TIMEOUT, rx.recv()).await {
            Ok(Some(sig)) => sig,
            Ok(None) => {
                // Sender dropped unexpectedly — treat as failure.
                let _ = ffmpeg_child.kill();
                return Err(DaemonError::InternalError(
                    "render signal channel closed before Done".into(),
                ));
            }
            Err(_) => {
                let _ = ffmpeg_child.kill();
                return Err(DaemonError::InternalError(format!(
                    "render page idle > {:?} (frames_written={})",
                    PAGE_IDLE_TIMEOUT, frames_written
                )));
            }
        };

        match sig {
            FrameSignal::Frame { frame_idx: _, png } => {
                ffmpeg_stdin.write_all(&png).map_err(|e| {
                    DaemonError::InternalError(format!("write frame to ffmpeg: {}", e))
                })?;
                frames_written += 1;
                svc.jobs()
                    .set_progress(
                        &job_id,
                        frames_written,
                        format!("frame {}/{}", frames_written, total_frames),
                    )
                    .await;
                svc.emit_progress(&job_id, frames_written, total_frames).await;
            }
            FrameSignal::Done { frames_emitted: _ } => {
                break;
            }
            FrameSignal::Failed(error) => {
                let _ = ffmpeg_child.kill();
                return Err(DaemonError::InternalError(format!(
                    "render page reported failure: {}",
                    error
                )));
            }
        }
    }

    // EOF ffmpeg stdin and wait for the encoder to finish.
    drop(ffmpeg_stdin);
    ffmpeg_child
        .wait()
        .map_err(|e| DaemonError::InternalError(format!("ffmpeg wait: {}", e)))?;

    let size_bytes = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    svc.jobs().set_state(&job_id, JobState::Succeeded).await;
    svc.jobs()
        .set_progress(&job_id, frames_written, "done".into())
        .await;

    let path_str = output_path.to_string_lossy().into_owned();
    svc.emit_succeeded(&job_id, &path_str, size_bytes).await;

    Ok(())
}
