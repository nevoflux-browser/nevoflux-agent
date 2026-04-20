//! CanvasVideoService — dependency bag + method surface.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

use crate::canvas_video::{
    create,
    frame_chunks::ChunkBuffer,
    job::{JobRegistry, JobSnapshot},
    render,
};
use crate::error::{DaemonError, Result};
use nevoflux_protocol::canvas_video::{
    CreateCompositionRequest, CreateCompositionResponse, RenderFrameChunk, RenderStartRequest,
    RenderStartResponse,
};

/// Trait for pushing bridge messages to the extension side.
#[async_trait::async_trait]
pub trait BridgeSender: Send + Sync {
    async fn push(&self, message_type: &str, payload: serde_json::Value) -> Result<()>;
}

pub struct CanvasVideoService {
    jobs: JobRegistry,
    /// Maps artifact_id -> (html, width, height, duration_sec, fps).
    /// Phase B replaces with real artifact repo.
    test_compositions: Mutex<HashMap<String, TestComposition>>,
    /// If true, render_start returns immediately without bridge calls.
    bridge_stub: bool,

    // --- bridge-side async coordination ---

    /// job_id -> sender for the "render page is ready" signal.
    ready_channels: Mutex<HashMap<String, oneshot::Sender<()>>>,

    /// (job_id, frame_idx) -> sender that fires when the frame PNG is assembled.
    frame_awaiters: Mutex<HashMap<(String, u32), oneshot::Sender<Vec<u8>>>>,

    /// job_id -> ChunkBuffer accumulating incoming frame chunk pieces.
    chunk_buffers: Mutex<HashMap<String, ChunkBuffer>>,

    /// Outbound bridge push (None in stub / test mode).
    bridge_sender: Option<Arc<dyn BridgeSender>>,
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
            ready_channels: Mutex::new(Default::default()),
            frame_awaiters: Mutex::new(Default::default()),
            chunk_buffers: Mutex::new(Default::default()),
            bridge_sender: None,
        }
    }

    pub fn new_for_tests() -> Self {
        Self {
            jobs: JobRegistry::new(),
            test_compositions: Mutex::new(Default::default()),
            bridge_stub: true,
            ready_channels: Mutex::new(Default::default()),
            frame_awaiters: Mutex::new(Default::default()),
            chunk_buffers: Mutex::new(Default::default()),
            bridge_sender: None,
        }
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
        // Stash metadata so a subsequent render_start can look it up.
        // Phase B replaces this with real artifact persistence.
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

    /// Register a oneshot sender that fires when canvas_video_ready arrives
    /// for this job.
    pub async fn register_job_ready_channel(&self, job_id: &str, tx: oneshot::Sender<()>) {
        self.ready_channels
            .lock()
            .await
            .insert(job_id.to_string(), tx);
    }

    /// Register a oneshot sender for frame PNG bytes once the frame is assembled.
    pub async fn register_frame_awaiter(
        &self,
        job_id: &str,
        frame_idx: u32,
        tx: oneshot::Sender<Vec<u8>>,
    ) {
        self.frame_awaiters
            .lock()
            .await
            .insert((job_id.to_string(), frame_idx), tx);
    }

    /// Ensure a ChunkBuffer exists for this job_id.
    pub async fn register_job_chunk_buffer(&self, job_id: &str) {
        let mut bufs = self.chunk_buffers.lock().await;
        bufs.entry(job_id.to_string()).or_insert_with(ChunkBuffer::new);
    }

    /// Called when canvas_video_ready arrives from extension.
    pub async fn on_render_ready(&self, job_id: &str) {
        if let Some(tx) = self.ready_channels.lock().await.remove(job_id) {
            let _ = tx.send(());
        }
    }

    /// Called when canvas_video_frame_chunk arrives from extension.
    /// Accumulates chunks; fires the awaiter when a frame is complete.
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
        if let Some(png_bytes) = complete {
            let key = (chunk.job_id.clone(), chunk.frame_idx);
            if let Some(tx) = self.frame_awaiters.lock().await.remove(&key) {
                let _ = tx.send(png_bytes);
            }
        }
        Ok(())
    }

    /// Return composition_id for a job (used for bridge payloads).
    pub async fn composition_id_for(&self, job_id: &str) -> String {
        self.jobs
            .snapshot(job_id)
            .await
            .map(|s| s.composition_id)
            .unwrap_or_default()
    }

    // --- Bridge push helpers (fail gracefully when no sender is configured) ---

    fn require_sender(&self) -> Result<&Arc<dyn BridgeSender>> {
        self.bridge_sender
            .as_ref()
            .ok_or_else(|| DaemonError::InternalError("no bridge sender configured".into()))
    }

    pub async fn push_canvas_video_open(
        &self,
        job_id: &str,
        composition_id: &str,
    ) -> Result<()> {
        self.require_sender()?
            .push(
                "canvas_video_open",
                serde_json::json!({
                    "job_id": job_id,
                    "composition_id": composition_id,
                }),
            )
            .await
    }

    pub async fn push_canvas_video_load(
        &self,
        job_id: &str,
        html: &str,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.require_sender()?
            .push(
                "canvas_video_load",
                serde_json::json!({
                    "job_id": job_id,
                    "html": html,
                    "width": width,
                    "height": height,
                }),
            )
            .await
    }

    pub async fn push_canvas_video_seek(
        &self,
        job_id: &str,
        t: f64,
        frame_idx: u32,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.require_sender()?
            .push(
                "canvas_video_seek",
                serde_json::json!({
                    "job_id": job_id,
                    "t": t,
                    "frame_idx": frame_idx,
                    "width": width,
                    "height": height,
                }),
            )
            .await
    }

    pub async fn push_canvas_video_close(&self, job_id: &str) -> Result<()> {
        self.require_sender()?
            .push(
                "canvas_video_close",
                serde_json::json!({
                    "job_id": job_id,
                }),
            )
            .await
    }

    /// Emit render progress. No-op at P1 (Task 14 wires EventBus).
    pub async fn emit_progress(&self, _job_id: &str, _current: u32, _total: u32) {}

    /// Emit render success terminal event. No-op at P1.
    pub async fn emit_succeeded(&self, _job_id: &str, _path: &str, _size: u64) {}

    /// Emit render failure terminal event. No-op at P1.
    pub async fn emit_failed(&self, _job_id: &str, _error: &str) {}
}

impl Default for CanvasVideoService {
    fn default() -> Self {
        Self::new()
    }
}
