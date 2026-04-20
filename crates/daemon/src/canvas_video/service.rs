//! CanvasVideoService — dependency bag + method surface.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::canvas_video::{
    create,
    job::{JobRegistry, JobSnapshot},
    render,
};
use crate::error::{DaemonError, Result};
use nevoflux_protocol::canvas_video::{
    CreateCompositionRequest, CreateCompositionResponse, RenderStartRequest, RenderStartResponse,
};

pub struct CanvasVideoService {
    jobs: JobRegistry,
    /// Maps artifact_id -> (html, width, height, duration_sec, fps).
    /// Phase B replaces with real artifact repo.
    test_compositions: Mutex<std::collections::HashMap<String, TestComposition>>,
    /// If true, render_start returns immediately without bridge calls.
    bridge_stub: bool,
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
        }
    }

    pub fn new_for_tests() -> Self {
        Self {
            jobs: JobRegistry::new(),
            test_compositions: Mutex::new(Default::default()),
            bridge_stub: true,
        }
    }

    pub fn bridge_is_stub(&self) -> bool {
        self.bridge_stub
    }

    pub fn jobs(&self) -> &JobRegistry {
        &self.jobs
    }

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
}

impl Default for CanvasVideoService {
    fn default() -> Self {
        Self::new()
    }
}
