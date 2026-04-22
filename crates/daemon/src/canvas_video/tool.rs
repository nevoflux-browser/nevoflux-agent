//! Agent-tool bridges for canvas.video.*.
//!
//! Wraps `CanvasVideoService` so the agent runner can invoke
//! `canvas_create_composition` and `canvas_render_video` through the
//! standard `ToolExecutor` / `ToolRegistry` surface.
//!
//! Production wiring in `agent/runner.rs` is deferred — those call sites
//! currently construct `ToolRegistry::new()` without access to
//! `CanvasVideoService`. A follow-up will thread the service through.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::tools::ToolExecutor;
use crate::canvas_video::CanvasVideoService;
use crate::error::{DaemonError, Result};
use nevoflux_protocol::canvas_video::{CreateCompositionRequest, RenderStartRequest};

/// `canvas_create_composition` agent tool.
pub struct CanvasCreateCompositionTool {
    svc: Arc<CanvasVideoService>,
}

impl CanvasCreateCompositionTool {
    pub fn new(svc: Arc<CanvasVideoService>) -> Self {
        Self { svc }
    }
}

#[async_trait]
impl ToolExecutor for CanvasCreateCompositionTool {
    async fn execute(&self, _name: &str, arguments: &Value) -> Result<String> {
        let req: CreateCompositionRequest =
            serde_json::from_value(arguments.clone()).map_err(|e| {
                DaemonError::InvalidRequest(format!("canvas_create_composition: {}", e))
            })?;
        let resp = self.svc.create_composition(req).await?;
        serde_json::to_string(&resp)
            .map_err(|e| DaemonError::InternalError(format!("serialize response: {}", e)))
    }
}

/// `canvas_render_video` agent tool.
pub struct CanvasRenderVideoTool {
    svc: Arc<CanvasVideoService>,
}

impl CanvasRenderVideoTool {
    pub fn new(svc: Arc<CanvasVideoService>) -> Self {
        Self { svc }
    }
}

#[async_trait]
impl ToolExecutor for CanvasRenderVideoTool {
    async fn execute(&self, _name: &str, arguments: &Value) -> Result<String> {
        let req: RenderStartRequest = serde_json::from_value(arguments.clone())
            .map_err(|e| DaemonError::InvalidRequest(format!("canvas_render_video: {}", e)))?;
        let resp = self.svc.render_start(req).await?;
        serde_json::to_string(&resp)
            .map_err(|e| DaemonError::InternalError(format!("serialize response: {}", e)))
    }
}

/// JSON Schema for `canvas_create_composition`. Exposed so the WASM agent
/// `ToolDefinition` list can reuse the exact same shape — keeps the dual
/// tool registry in sync without two sources of truth.
pub fn create_composition_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "title":        { "type": "string" },
            "width":        { "type": "integer", "minimum": 1, "maximum": 1920 },
            "height":       { "type": "integer", "minimum": 1, "maximum": 1920 },
            "duration_sec": { "type": "number",  "minimum": 0.5, "maximum": 60 },
            "fps":          { "type": "integer", "enum": [24, 25, 30] },
            "bg":           { "type": ["string", "null"] },
            "html":         { "type": ["string", "null"] }
        },
        "required": ["title", "width", "height", "duration_sec", "fps"]
    })
}

/// JSON Schema for `canvas_render_video`.
pub fn render_video_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "composition_id": { "type": "string" }
        },
        "required": ["composition_id"]
    })
}

/// Register both canvas.video.* tools on an existing `ToolRegistry`.
///
/// Call sites inject the shared `CanvasVideoService` so both tools and
/// bridge handlers see the same job registry / composition store.
pub fn register(registry: &mut crate::agent::tools::ToolRegistry, svc: Arc<CanvasVideoService>) {
    registry.register(
        "canvas_create_composition",
        Box::new(CanvasCreateCompositionTool::new(svc.clone())),
    );
    registry.register(
        "canvas_render_video",
        Box::new(CanvasRenderVideoTool::new(svc)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::ToolRegistry;

    #[tokio::test]
    async fn test_canvas_create_composition_tool_dispatches_to_service() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let tool = CanvasCreateCompositionTool::new(svc);
        let args = serde_json::json!({
            "title": "demo",
            "width": 640,
            "height": 360,
            "duration_sec": 1.0,
            "fps": 30
        });
        let out = tool
            .execute("canvas_create_composition", &args)
            .await
            .unwrap();
        assert!(out.contains("artifact_id"));
        assert!(out.contains("comp-"));
    }

    #[tokio::test]
    async fn test_canvas_create_composition_tool_surfaces_validation_errors() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let tool = CanvasCreateCompositionTool::new(svc);
        let args = serde_json::json!({
            "title": "bad",
            "width": 640,
            "height": 360,
            "duration_sec": 1.0,
            "fps": 60
        });
        let err = tool
            .execute("canvas_create_composition", &args)
            .await
            .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("fps") || msg.contains("60"),
            "unexpected: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_register_adds_both_tools() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let mut registry = ToolRegistry::empty();
        register(&mut registry, svc);
        assert!(registry.has_tool("canvas_create_composition"));
        assert!(registry.has_tool("canvas_render_video"));
    }

    #[test]
    fn test_schemas_match_hard_limits() {
        let s = create_composition_schema();
        let props = &s["properties"];
        assert_eq!(props["width"]["maximum"], 1920);
        assert_eq!(props["height"]["maximum"], 1920);
        assert_eq!(props["duration_sec"]["maximum"], 60);
        let fps_enum = props["fps"]["enum"].as_array().unwrap();
        assert_eq!(fps_enum.len(), 3);
    }
}
