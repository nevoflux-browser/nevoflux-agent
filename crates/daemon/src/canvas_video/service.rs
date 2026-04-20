//! CanvasVideoService — dependency bag + method surface.
//!
//! The actor-rework (2026-04-20) moved `canvas.video.*` transport from the
//! WebExtension background script onto the `NevofluxParent`/`NevofluxChild`
//! JSActor pair. The daemon no longer pushes seek commands to the page: the
//! render page drives the loop itself (single-threaded JS `for` over frame
//! indices) and streams chunks back. Accordingly, `BridgeSender` and the
//! `push_canvas_video_*` helpers have been removed.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::canvas_video::{
    create,
    frame_chunks::ChunkBuffer,
    job::{JobRegistry, JobSnapshot},
    render,
};
use crate::error::{DaemonError, Result};
use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};
use nevoflux_protocol::canvas_video::{
    CreateCompositionRequest, CreateCompositionResponse, RenderFrameChunk, RenderStartRequest,
    RenderStartResponse,
};

/// Single-stream signal the render loop consumes for a given job.
#[derive(Debug)]
pub enum FrameSignal {
    /// A complete PNG frame arrived from the page.
    Frame { frame_idx: u32, png: Vec<u8> },
    /// Page reports it has emitted the final frame; finalize the encode.
    Done { frames_emitted: u32 },
    /// Page reports an unrecoverable error; abort the job.
    Failed(String),
}

pub struct CanvasVideoService {
    jobs: JobRegistry,
    /// Maps artifact_id -> composition spec + HTML.
    /// Phase B replaces with real artifact repo.
    test_compositions: Mutex<HashMap<String, TestComposition>>,
    /// If true, render_start returns immediately without page-bridge interaction.
    bridge_stub: bool,

    // --- bridge-side async coordination ---

    /// job_id -> ChunkBuffer accumulating incoming frame chunk pieces.
    chunk_buffers: Mutex<HashMap<String, ChunkBuffer>>,

    /// job_id -> unbounded sender into the render loop. One channel per active
    /// job, fed by `on_frame_chunk` / `on_render_done` / `on_render_failed`.
    signal_senders: Mutex<HashMap<String, mpsc::UnboundedSender<FrameSignal>>>,

    /// EventBus handle for jobs.render.{id} progress + terminal events.
    /// None in stub / test mode; emits become no-ops.
    event_bus: Option<Arc<EventBus>>,
}

#[derive(Clone)]
struct TestComposition {
    html: String,
    width: u32,
    height: u32,
    duration_sec: f32,
    fps: u32,
}

impl CanvasVideoService {
    pub fn new() -> Self {
        Self {
            jobs: JobRegistry::new(),
            test_compositions: Mutex::new(Default::default()),
            bridge_stub: false,
            chunk_buffers: Mutex::new(Default::default()),
            signal_senders: Mutex::new(Default::default()),
            event_bus: None,
        }
    }

    pub fn new_for_tests() -> Self {
        Self {
            jobs: JobRegistry::new(),
            test_compositions: Mutex::new(Default::default()),
            bridge_stub: true,
            chunk_buffers: Mutex::new(Default::default()),
            signal_senders: Mutex::new(Default::default()),
            event_bus: None,
        }
    }

    /// Builder: attach an EventBus so emit_* methods publish progress and
    /// terminal events on `jobs.render.{job_id}`.
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn bridge_is_stub(&self) -> bool {
        self.bridge_stub
    }

    pub fn jobs(&self) -> &JobRegistry {
        &self.jobs
    }

    // --- Composition management ---

    pub async fn create_composition(
        self: &Arc<Self>,
        req: CreateCompositionRequest,
    ) -> Result<CreateCompositionResponse> {
        let resp = create::create(self, req.clone()).await?;
        let html = req
            .html
            .clone()
            .unwrap_or_else(|| create::default_scaffold_for(&req));
        self.test_compositions.lock().await.insert(
            resp.artifact_id.clone(),
            TestComposition {
                html,
                width: req.width,
                height: req.height,
                duration_sec: req.duration_sec,
                fps: req.fps,
            },
        );
        Ok(resp)
    }

    pub async fn read_composition_html(&self, id: &str) -> Result<String> {
        self.test_compositions
            .lock()
            .await
            .get(id)
            .map(|c| c.html.clone())
            .ok_or_else(|| {
                DaemonError::InvalidRequest(format!("composition not found: {}", id))
            })
    }

    pub async fn composition_spec(&self, id: &str) -> Result<(u32, u32, f32, u32)> {
        self.test_compositions
            .lock()
            .await
            .get(id)
            .map(|c| (c.width, c.height, c.duration_sec, c.fps))
            .ok_or_else(|| {
                DaemonError::InvalidRequest(format!("composition not found: {}", id))
            })
    }

    pub async fn render_start(
        self: &Arc<Self>,
        req: RenderStartRequest,
    ) -> Result<RenderStartResponse> {
        render::render_start(self, req).await
    }

    pub async fn job_snapshot(&self, job_id: &str) -> Option<JobSnapshot> {
        self.jobs.snapshot(job_id).await
    }

    // --- Bridge coordination helpers ---

    /// Register the render loop's signal channel for this job. Returns the
    /// receiver for the loop to consume. Overwrites any previous sender for
    /// the same job_id (callers must not double-register).
    pub async fn register_job_signal_channel(
        &self,
        job_id: &str,
    ) -> mpsc::UnboundedReceiver<FrameSignal> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.signal_senders
            .lock()
            .await
            .insert(job_id.to_string(), tx);
        // Pre-create the chunk buffer too so chunks arriving before the
        // render loop finishes setup don't race.
        self.chunk_buffers
            .lock()
            .await
            .entry(job_id.to_string())
            .or_insert_with(ChunkBuffer::new);
        rx
    }

    /// Called when the render loop exits, to free per-job state.
    pub async fn cleanup_job_channels(&self, job_id: &str) {
        self.signal_senders.lock().await.remove(job_id);
        self.chunk_buffers.lock().await.remove(job_id);
    }

    /// Called when canvas_video_frame_chunk arrives from extension.
    /// Accumulates chunks; pushes a `FrameSignal::Frame` into the job's signal
    /// channel once a frame is fully assembled.
    pub async fn on_frame_chunk(&self, chunk: RenderFrameChunk) -> Result<()> {
        let complete = {
            let mut bufs = self.chunk_buffers.lock().await;
            let buf = bufs
                .entry(chunk.job_id.clone())
                .or_insert_with(ChunkBuffer::new);
            buf.add_chunk(
                chunk.frame_idx,
                chunk.chunk_idx,
                chunk.total_chunks,
                chunk.is_last,
                chunk.bytes,
            )
        };
        if let Some(png) = complete {
            self.send_signal(
                &chunk.job_id,
                FrameSignal::Frame {
                    frame_idx: chunk.frame_idx,
                    png,
                },
            )
            .await;
        }
        Ok(())
    }

    /// Called when canvas_video_render_done arrives from extension (page-driven
    /// completion signal).
    pub async fn on_render_done(&self, job_id: &str, frames_emitted: u32) {
        self.send_signal(job_id, FrameSignal::Done { frames_emitted })
            .await;
    }

    /// Called when canvas_video_render_failed arrives from extension.
    pub async fn on_render_failed(&self, job_id: &str, error: &str) {
        self.send_signal(job_id, FrameSignal::Failed(error.to_string()))
            .await;
    }

    async fn send_signal(&self, job_id: &str, sig: FrameSignal) {
        if let Some(tx) = self.signal_senders.lock().await.get(job_id) {
            // Channel closed means the render loop already exited; dropping
            // the signal is the correct behavior.
            let _ = tx.send(sig);
        }
    }

    /// Return composition_id for a job (used by callers that want metadata).
    pub async fn composition_id_for(&self, job_id: &str) -> String {
        self.jobs
            .snapshot(job_id)
            .await
            .map(|s| s.composition_id)
            .unwrap_or_default()
    }

    // --- EventBus emitters ---

    async fn emit(&self, job_id: &str, payload: serde_json::Value) {
        if let Some(bus) = &self.event_bus {
            let topic = format!("jobs.render.{}", job_id);
            let event = BusEvent::ephemeral(topic, payload, PublisherIdentity::Internal);
            let _ = bus.publish(event).await;
        }
    }

    pub async fn emit_progress(&self, job_id: &str, current: u32, total: u32) {
        self.emit(
            job_id,
            serde_json::json!({
                "event": "progress",
                "job_id": job_id,
                "current": current,
                "total": total,
            }),
        )
        .await;
    }

    pub async fn emit_succeeded(&self, job_id: &str, path: &str, size_bytes: u64) {
        self.emit(
            job_id,
            serde_json::json!({
                "event": "succeeded",
                "job_id": job_id,
                "output_path": path,
                "size_bytes": size_bytes,
            }),
        )
        .await;
    }

    pub async fn emit_failed(&self, job_id: &str, error: &str) {
        self.emit(
            job_id,
            serde_json::json!({
                "event": "failed",
                "job_id": job_id,
                "error": error,
            }),
        )
        .await;
    }
}

impl Default for CanvasVideoService {
    fn default() -> Self {
        Self::new()
    }
}
