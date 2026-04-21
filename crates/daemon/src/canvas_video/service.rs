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

/// Pure throttle decision for progress deliveries. Emits when `current`
/// lands on a multiple of `throttle = max(1, total/20)`, OR when
/// `current == total` (so the final frame always lands on the bus).
///
/// P2 design §4.3: reduces 1080p × 30 s (900 frames) from 900 events to
/// ~21. At small totals (< 20) throttle is 1 so every frame emits —
/// acceptable since the total cost is bounded.
pub(crate) fn should_emit_progress(current: u32, total: u32) -> bool {
    let throttle = (total / 20).max(1);
    current % throttle == 0 || current == total
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

    /// Fetch the composition HTML + spec that the render page needs to
    /// draw the given job. Returns InvalidRequest if the job or its
    /// composition is unknown.
    pub async fn get_composition_for_job(
        &self,
        job_id: &str,
    ) -> Result<(String, u32, u32, f32, u32)> {
        let snap = self
            .jobs
            .snapshot(job_id)
            .await
            .ok_or_else(|| DaemonError::InvalidRequest(format!("job not found: {}", job_id)))?;
        let html = self.read_composition_html(&snap.composition_id).await?;
        let (width, height, duration_sec, fps) =
            self.composition_spec(&snap.composition_id).await?;
        Ok((html, width, height, duration_sec, fps))
    }

    // --- EventBus emitters ---

    async fn emit(&self, job_id: &str, payload: serde_json::Value) {
        if let Some(bus) = &self.event_bus {
            let topic = format!("jobs:render:{}", job_id);
            let event_kind = payload.get("event").and_then(|v| v.as_str()).unwrap_or("?");
            let event = BusEvent::ephemeral(topic.clone(), payload, PublisherIdentity::Internal);
            match bus.publish(event).await {
                Ok(_) => tracing::info!(
                    topic = %topic,
                    event = %event_kind,
                    "canvas_video emit publish ok"
                ),
                Err(e) => tracing::warn!(
                    topic = %topic,
                    event = %event_kind,
                    error = %e,
                    "canvas_video emit publish FAILED"
                ),
            }
        } else {
            tracing::warn!(
                job_id = %job_id,
                "canvas_video emit called but event_bus is None — service not wired"
            );
        }
    }

    pub async fn emit_progress(&self, job_id: &str, current: u32, total: u32) {
        if !should_emit_progress(current, total) {
            return;
        }
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

    pub async fn emit_cancelled(&self, job_id: &str, current: u32, total: u32) {
        self.emit(
            job_id,
            serde_json::json!({
                "event": "cancelled",
                "job_id": job_id,
                "current": current,
                "total": total,
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

#[cfg(test)]
mod p2_emit_tests {
    use super::*;
    use crate::event_bus::{BackpressurePolicy, EventBus, SubscriberIdentity};
    use std::sync::Arc;

    #[tokio::test]
    async fn emit_cancelled_publishes_event_with_frame_counts() {
        let bus = Arc::new(EventBus::new());
        let svc = Arc::new(
            CanvasVideoService::new_for_tests().with_event_bus(bus.clone()),
        );

        // Subscribe BEFORE emitting so we capture the delivery.
        let pattern = crate::event_bus::types::TopicPattern::wildcard("jobs:render:*");
        let mut sub = bus
            .subscribe(
                pattern,
                SubscriberIdentity::Internal,
                BackpressurePolicy::DropOldest,
                64,
            )
            .expect("subscribe");

        svc.emit_cancelled("job-abc", 42, 150).await;

        let delivered = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            sub.rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("delivery");

        assert_eq!(delivered.topic, "jobs:render:job-abc");
        let body = &delivered.payload;
        assert_eq!(body["event"], "cancelled");
        assert_eq!(body["job_id"], "job-abc");
        assert_eq!(body["current"], 42);
        assert_eq!(body["total"], 150);
    }

    /// Pure throttle decision: kept separate from the async emit path so
    /// we can exhaustively test the gating math without an EventBus.
    #[test]
    fn should_emit_progress_throttles_to_about_20_per_render() {
        // 900 frames → throttle = 45 → emits at 0, 45, …, 900 (inclusive).
        let mut n = 0;
        for current in 0..=900 {
            if super::should_emit_progress(current, 900) {
                n += 1;
            }
        }
        assert!((18..=25).contains(&n), "900→{}", n);

        // 150 frames → throttle = 7 → ~22 emits.
        let mut n = 0;
        for current in 0..=150 {
            if super::should_emit_progress(current, 150) {
                n += 1;
            }
        }
        assert!((18..=25).contains(&n), "150→{}", n);

        // total below divisor (10): throttle = 1 → emit every frame.
        let mut n = 0;
        for current in 0..=10 {
            if super::should_emit_progress(current, 10) {
                n += 1;
            }
        }
        assert_eq!(n, 11, "10→{}", n);

        // Terminal (current == total) always fires even if not on the divisor.
        assert!(super::should_emit_progress(900, 900));
        assert!(super::should_emit_progress(7, 150));
    }
}
