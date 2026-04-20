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

use std::sync::Arc;

use crate::canvas_video::job::JobState;
use crate::canvas_video::CanvasVideoService;
use crate::error::Result;
use nevoflux_protocol::canvas_video::{RenderStartRequest, RenderStartResponse};

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
            svc_clone
                .jobs()
                .set_error(&job_id_clone, format!("{}", e))
                .await;
        }
    });

    Ok(RenderStartResponse { job_id })
}

#[allow(clippy::too_many_arguments)]
async fn run_render_loop(
    svc: Arc<CanvasVideoService>,
    job_id: String,
    _html: String,
    _width: u32,
    _height: u32,
    _duration_sec: f32,
    _fps: u32,
) -> Result<()> {
    svc.jobs().set_state(&job_id, JobState::Running).await;
    svc.jobs()
        .set_progress(&job_id, 0, "opening render tab".into())
        .await;

    // For test builds with no bridge attached, exit gracefully after
    // registering the Running state. Phase B Task 13 wires the actual
    // bridge interactions.
    if svc.bridge_is_stub() {
        svc.jobs().set_state(&job_id, JobState::Succeeded).await;
        return Ok(());
    }

    // Production path: Task 13 fills in bridge push + loop.
    todo!("render loop bridge wiring — Task 13");
}
