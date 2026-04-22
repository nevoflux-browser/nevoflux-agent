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
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

use crate::canvas_video::{
    create,
    frame_chunks::ChunkBuffer,
    job::{JobRegistry, JobSnapshot},
    render,
};
use crate::error::{DaemonError, Result};
use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};
use nevoflux_protocol::canvas_video::{
    CreateCompositionRequest, CreateCompositionResponse, LintReport, RenderFrameChunk,
    RenderStartRequest, RenderStartResponse,
};
use nevoflux_skills::SkillRegistry;
use nevoflux_storage::Storage;

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

    /// Persistent artifact storage. Set by server.rs via `with_storage`; used
    /// by T6 to persist compositions to the artifact repo.
    storage: Option<Arc<Storage>>,

    /// Skill registry for reading auxiliary files (templates, DESIGN-template).
    /// Shared with HostServices so both see the same loaded registry.
    skills: Option<Arc<RwLock<SkillRegistry>>>,

    /// Pending lint correlators: correlator -> oneshot sender for the lint result.
    /// Inserted by `lint_composition`, removed by `on_lint_result` or on timeout.
    lint_correlators: Mutex<HashMap<String, oneshot::Sender<LintReport>>>,
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
            bridge_stub: false,
            chunk_buffers: Mutex::new(Default::default()),
            signal_senders: Mutex::new(Default::default()),
            event_bus: None,
            storage: None,
            skills: None,
            lint_correlators: Mutex::new(Default::default()),
        }
    }

    pub fn new_for_tests() -> Self {
        let storage = Storage::open_in_memory().expect("new_for_tests: in-memory Storage");
        Self {
            jobs: JobRegistry::new(),
            bridge_stub: true,
            chunk_buffers: Mutex::new(Default::default()),
            signal_senders: Mutex::new(Default::default()),
            event_bus: None,
            storage: Some(Arc::new(storage)),
            skills: Some(Arc::new(RwLock::new(SkillRegistry::new()))),
            lint_correlators: Mutex::new(Default::default()),
        }
    }

    /// Builder: attach an EventBus so emit_* methods publish progress and
    /// terminal events on `jobs.render.{job_id}`.
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Builder: attach the shared artifact storage (used by T6 to persist compositions).
    pub fn with_storage(mut self, storage: Arc<Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Builder: attach the shared skill registry (used by T7 to read templates/DESIGN-template).
    pub fn with_skills(mut self, skills: Arc<RwLock<SkillRegistry>>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Return a reference to the artifact storage, if configured.
    pub fn storage(&self) -> Option<&Arc<Storage>> {
        self.storage.as_ref()
    }

    /// Return a reference to the skill registry, if configured.
    pub fn skills(&self) -> Option<&Arc<RwLock<SkillRegistry>>> {
        self.skills.as_ref()
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
        create::create(self, req).await
    }

    /// Load a composition from the artifact repository and return (html, w, h, d, fps).
    pub async fn load_composition(
        &self,
        composition_id: &str,
    ) -> Result<(String, u32, u32, f32, u32)> {
        use nevoflux_protocol::canvas_video::CompositionMeta;
        use nevoflux_storage::repositories::ArtifactRepository;

        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| DaemonError::InternalError("canvas_video: storage not wired".into()))?;
        let repo = ArtifactRepository::new(storage.database());
        let rec = repo
            .get(composition_id)
            .map_err(|e| DaemonError::InternalError(format!("{e}")))?
            .ok_or_else(|| {
                DaemonError::InvalidRequest(format!("composition not found: {composition_id}"))
            })?;

        let files = rec
            .files
            .as_ref()
            .ok_or_else(|| DaemonError::InvalidComposition {
                reason: "no files map (not a composition artifact)".into(),
            })?;

        let meta_raw =
            files
                .get("composition.meta.json")
                .ok_or_else(|| DaemonError::InvalidComposition {
                    reason: "missing composition.meta.json".into(),
                })?;
        let meta: CompositionMeta =
            serde_json::from_str(meta_raw).map_err(|e| DaemonError::InvalidComposition {
                reason: format!("malformed meta: {e}"),
            })?;
        meta.validate_hard_limits()
            .map_err(|e| DaemonError::InvalidComposition {
                reason: format!("{e}"),
            })?;

        let entry = rec.entry.as_deref().unwrap_or("index.html");
        let html = files
            .get(entry)
            .cloned()
            .ok_or_else(|| DaemonError::InvalidComposition {
                reason: format!("entry file missing: {entry}"),
            })?;
        Ok((
            html,
            meta.spec.width,
            meta.spec.height,
            meta.spec.duration_sec,
            meta.spec.fps,
        ))
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
            .ok_or_else(|| DaemonError::InvalidRequest(format!("job not found: {job_id}")))?;
        self.load_composition(&snap.composition_id).await
    }

    // --- Lint correlator plumbing ---

    /// Request a lint pass for the given composition.
    ///
    /// Publishes a lint request event on the EventBus (if wired) and awaits
    /// a `LintReport` from the extension via `on_lint_result`. Times out
    /// after 5 seconds if no resolver calls `on_lint_result`.
    pub async fn lint_composition(self: &Arc<Self>, composition_id: &str) -> Result<LintReport> {
        let (html, _w, _h, _d, _fps) = self.load_composition(composition_id).await?;
        let correlator = uuid::Uuid::new_v4().simple().to_string();
        let (tx, rx) = oneshot::channel();
        self.lint_correlators
            .lock()
            .await
            .insert(correlator.clone(), tx);

        // Publish the lint request on the EventBus so Task 12 (TCP server glue)
        // can pick it up and forward to the extension. Skip in test mode where
        // event_bus is None; tests resolve correlators directly via on_lint_result.
        if let Some(bus) = &self.event_bus {
            let topic = format!("jobs:lint:request:{correlator}");
            let payload = serde_json::json!({
                "event": "lint_request",
                "job_correlator": correlator,
                "composition_id": composition_id,
                "composition_html": html,
            });
            let event = BusEvent::ephemeral(topic, payload, PublisherIdentity::Internal);
            let _ = bus.publish(event).await;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(report)) => Ok(report),
            Ok(Err(_)) => {
                // Sender was dropped before we received a result.
                self.lint_correlators.lock().await.remove(&correlator);
                Err(DaemonError::InternalError(
                    "lint oneshot dropped before resolve".into(),
                ))
            }
            Err(_) => {
                // Timeout: remove the dangling correlator entry.
                self.lint_correlators.lock().await.remove(&correlator);
                Err(DaemonError::LintTimeout {
                    composition_id: composition_id.to_string(),
                })
            }
        }
    }

    /// Called when the extension delivers a lint result for a pending correlator.
    ///
    /// Resolves the oneshot receiver held by `lint_composition`. If the
    /// correlator is unknown (e.g. the caller already timed out), logs a
    /// warning and does nothing.
    pub async fn on_lint_result(&self, correlator: &str, report: LintReport) {
        if let Some(tx) = self.lint_correlators.lock().await.remove(correlator) {
            let _ = tx.send(report);
        } else {
            tracing::warn!(
                correlator,
                "lint result for unknown correlator — already timed out or duplicate"
            );
        }
    }

    /// Returns the first pending lint correlator key, for use in tests that
    /// need to simulate an extension response without a real EventBus.
    #[cfg(test)]
    pub async fn peek_pending_lint_correlator(&self) -> Option<String> {
        self.lint_correlators.lock().await.keys().next().cloned()
    }

    // --- EventBus emitters ---

    async fn emit(&self, job_id: &str, payload: serde_json::Value) {
        if let Some(bus) = &self.event_bus {
            let topic = format!("jobs:render:{}", job_id);
            let event_kind = payload
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
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
mod p3_load_tests {
    use super::*;
    use nevoflux_protocol::canvas_video::CreateCompositionRequest;
    use nevoflux_storage::{repositories::ArtifactRepository, CreateArtifactParams};

    fn req() -> CreateCompositionRequest {
        CreateCompositionRequest {
            title: "t".into(),
            width: 640,
            height: 360,
            duration_sec: 5.0,
            fps: 30,
            bg: None,
            html: None,
            template: None,
            session_id: None,
        }
    }

    #[tokio::test]
    async fn load_reads_composition_written_by_create() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let resp = svc.create_composition(req()).await.unwrap();
        let (html, w, h, d, fps) = svc.load_composition(&resp.artifact_id).await.unwrap();
        assert!(html.contains("stage"));
        assert_eq!((w, h, fps), (640, 360, 30));
        assert!((d - 5.0).abs() < 1e-3);
    }

    #[tokio::test]
    async fn load_rejects_non_composition_artifact() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let storage = svc.storage().unwrap().clone();
        let repo = ArtifactRepository::new(storage.database());
        repo.create(CreateArtifactParams {
            id: "art-plain".into(),
            session_id: None,
            title: "plain".into(),
            description: None,
            content_type: "text/html".into(),
            content: "<p>hi</p>".into(),
            files: None,
            entry: None,
        })
        .unwrap();
        let err = svc.load_composition("art-plain").await.unwrap_err();
        assert!(
            format!("{err}").contains("invalid composition"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn load_rejects_malformed_meta() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let storage = svc.storage().unwrap().clone();
        let repo = ArtifactRepository::new(storage.database());
        let mut files = std::collections::HashMap::new();
        files.insert("index.html".into(), "<body>ok</body>".into());
        files.insert("composition.meta.json".into(), "not-json".into());
        repo.create(CreateArtifactParams {
            id: "comp-bad".into(),
            session_id: None,
            title: "bad".into(),
            description: None,
            content_type: "text/html".into(),
            content: "<body>ok</body>".into(),
            files: Some(files),
            entry: Some("index.html".into()),
        })
        .unwrap();
        let err = svc.load_composition("comp-bad").await.unwrap_err();
        assert!(format!("{err}").contains("malformed"), "got {err}");
    }

    #[tokio::test]
    async fn load_rejects_spec_out_of_bounds() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let storage = svc.storage().unwrap().clone();
        let repo = ArtifactRepository::new(storage.database());
        let bad_meta = serde_json::json!({
            "kind": "composition", "version": 1,
            "spec": {"width": 640, "height": 360, "duration_sec": 5.0, "fps": 60},
            "origin": {"created_with": "t", "created_at": 0}
        });
        let mut files = std::collections::HashMap::new();
        files.insert("index.html".into(), "<body>ok</body>".into());
        files.insert("composition.meta.json".into(), bad_meta.to_string());
        repo.create(CreateArtifactParams {
            id: "comp-bad-fps".into(),
            session_id: None,
            title: "bad".into(),
            description: None,
            content_type: "text/html".into(),
            content: "<body>ok</body>".into(),
            files: Some(files),
            entry: Some("index.html".into()),
        })
        .unwrap();
        let err = svc.load_composition("comp-bad-fps").await.unwrap_err();
        assert!(
            format!("{err}").contains("60") || format!("{err}").contains("fps"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn load_rejects_missing_entry_file() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let storage = svc.storage().unwrap().clone();
        let repo = ArtifactRepository::new(storage.database());
        let good_meta = serde_json::json!({
            "kind": "composition", "version": 1,
            "spec": {"width": 640, "height": 360, "duration_sec": 5.0, "fps": 30},
            "origin": {"created_with": "t", "created_at": 0}
        });
        let mut files = std::collections::HashMap::new();
        files.insert("composition.meta.json".into(), good_meta.to_string());
        // index.html intentionally missing
        repo.create(CreateArtifactParams {
            id: "comp-no-entry".into(),
            session_id: None,
            title: "bad".into(),
            description: None,
            content_type: "text/html".into(),
            content: "".into(),
            files: Some(files),
            entry: Some("index.html".into()),
        })
        .unwrap();
        let err = svc.load_composition("comp-no-entry").await.unwrap_err();
        assert!(format!("{err}").contains("entry"), "got {err}");
    }
}

#[cfg(test)]
mod p3_deps_tests {
    use super::*;
    use nevoflux_skills::SkillRegistry;
    use nevoflux_storage::Storage;
    use std::sync::Arc;
    use tokio::sync::RwLock as TokioRwLock;

    #[tokio::test]
    async fn new_for_tests_has_storage_and_skills() {
        let svc = CanvasVideoService::new_for_tests();
        assert!(svc.storage().is_some(), "test service must carry Storage");
        assert!(
            svc.skills().is_some(),
            "test service must carry SkillRegistry"
        );
    }

    #[tokio::test]
    async fn builders_install_storage_and_skills() {
        let storage = Arc::new(Storage::open_in_memory().expect("new Storage"));
        let skills = Arc::new(TokioRwLock::new(SkillRegistry::new()));
        let svc = CanvasVideoService::new()
            .with_storage(storage.clone())
            .with_skills(skills.clone());
        assert!(Arc::ptr_eq(svc.storage().unwrap(), &storage));
        assert!(Arc::ptr_eq(svc.skills().unwrap(), &skills));
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
        let svc = Arc::new(CanvasVideoService::new_for_tests().with_event_bus(bus.clone()));

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

        let delivered = tokio::time::timeout(std::time::Duration::from_millis(200), sub.rx.recv())
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
