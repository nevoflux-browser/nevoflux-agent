//! canvas.video.create_composition implementation.

use crate::canvas_video::CanvasVideoService;
use crate::error::{DaemonError, Result};
use nevoflux_protocol::canvas_video::{CreateCompositionRequest, CreateCompositionResponse};

const ALLOWED_FPS: &[u32] = &[24, 25, 30];
const MAX_DIMENSION: u32 = 1920;
const MAX_DURATION_SEC: f32 = 60.0;
const MIN_DURATION_SEC: f32 = 0.5;

pub async fn create(
    svc: &CanvasVideoService,
    req: CreateCompositionRequest,
) -> Result<CreateCompositionResponse> {
    validate(&req)?;

    let artifact_id = format!("comp-{}", uuid::Uuid::new_v4().simple());
    let _html = req.html.clone().unwrap_or_else(|| default_scaffold(&req));

    // Phase B wires svc deps (artifact repo) to persist the HTML.
    let _ = svc;

    Ok(CreateCompositionResponse { artifact_id })
}

fn validate(req: &CreateCompositionRequest) -> Result<()> {
    if !ALLOWED_FPS.contains(&req.fps) {
        return Err(DaemonError::InvalidRequest(format!(
            "invalid fps {}: allowed {:?}",
            req.fps, ALLOWED_FPS
        )));
    }
    if req.duration_sec < MIN_DURATION_SEC || req.duration_sec > MAX_DURATION_SEC {
        return Err(DaemonError::InvalidRequest(format!(
            "duration_sec {} outside [{}, {}]",
            req.duration_sec, MIN_DURATION_SEC, MAX_DURATION_SEC
        )));
    }
    if req.width == 0
        || req.width > MAX_DIMENSION
        || req.height == 0
        || req.height > MAX_DIMENSION
    {
        return Err(DaemonError::InvalidRequest(format!(
            "dimensions {}x{} out of range (max {})",
            req.width, req.height, MAX_DIMENSION
        )));
    }
    Ok(())
}

fn default_scaffold(req: &CreateCompositionRequest) -> String {
    let bg = req.bg.clone().unwrap_or_else(|| "#000".into());
    format!(
        r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body {{ margin: 0; background: {bg}; }}
  #stage {{ position: relative; width: {w}px; height: {h}px; overflow: hidden; }}
</style></head>
<body>
  <div id="stage"
       data-composition-id="{title}"
       data-width="{w}"
       data-height="{h}"
       data-duration="{d}"
       data-fps="{fps}"
       data-bg="{bg}">
  </div>
  <script type="module">
    window.__timelines = window.__timelines || [];
  </script>
</body>
</html>"##,
        bg = bg,
        title = req.title,
        w = req.width,
        h = req.height,
        d = req.duration_sec,
        fps = req.fps,
    )
}
