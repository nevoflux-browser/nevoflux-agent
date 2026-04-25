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
use nevoflux_protocol::canvas_video::{
    CreateCompositionRequest, LintCompositionRequest, RenderStartRequest,
};

/// `canvas_create_composition` agent tool.
pub struct CanvasCreateCompositionTool {
    svc: Arc<CanvasVideoService>,
}

impl CanvasCreateCompositionTool {
    pub fn new(svc: Arc<CanvasVideoService>) -> Self {
        Self { svc }
    }
}

/// Shared parser used at every LLM dispatch surface.
///
/// The LLM-facing JSON Schema does not advertise `html`, but the
/// `CreateCompositionRequest` struct still accepts it for internal callers.
/// serde sees `html` as a valid named field, so #[serde(deny_unknown_fields)]
/// can't catch hallucinated submissions. Reject the field explicitly here so
/// every surface (this tool, mcp_tool_executor, agent_host) routes through
/// the same gate and the LLM gets a clear retry signal.
pub fn parse_create_composition_args_strict(
    arguments: &Value,
) -> Result<CreateCompositionRequest> {
    if arguments.get("html").is_some() {
        return Err(DaemonError::InvalidRequest(
            "canvas_create_composition: the `html` field is not accepted from agents; \
             pass `template` (one of: website-promo-16x9, product-intro-16x9, \
             product-intro-9x16, tiktok-hook, video-overlay, logo-3d-reveal, \
             product-3d-spin) and use edit_artifact afterward to customize content."
                .into(),
        ));
    }
    serde_json::from_value(arguments.clone())
        .map_err(|e| DaemonError::InvalidRequest(format!("canvas_create_composition: {}", e)))
}

#[async_trait]
impl ToolExecutor for CanvasCreateCompositionTool {
    async fn execute(&self, _name: &str, arguments: &Value) -> Result<String> {
        let req = parse_create_composition_args_strict(arguments)?;
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
///
/// NOTE: `html` is intentionally omitted from the LLM-facing schema. The
/// CreateCompositionRequest struct still accepts it for internal callers
/// (tests, scripts) but agents must select one of the seven shipped
/// templates. Earlier iterations exposed `html` and the LLM aggressively
/// preferred it (copying skill_read'd template content into html instead
/// of passing template:), leaving meta.origin.template = null. Removing
/// the field from the schema makes the template path the only available
/// option and reliably routes through daemon's substitute_placeholders.
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
            "template":     {
                "type": "string",
                "enum": [
                    "website-promo-16x9",
                    "product-intro-16x9",
                    "product-intro-9x16",
                    "tiktok-hook",
                    "video-overlay",
                    "logo-3d-reveal",
                    "product-3d-spin"
                ],
                "description": "Skill template name from the /video skill. The daemon \
                                materializes the named template into the composition; you do \
                                NOT need to call skill_read first. To customize the rendered \
                                HTML afterward, use edit_artifact on the resulting composition."
            }
        },
        "required": ["title", "width", "height", "duration_sec", "fps", "template"]
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

/// `canvas_lint_composition` agent tool.
///
/// Delegates to `CanvasVideoService::lint_composition`, which publishes a
/// lint request on the EventBus and awaits a `LintReport` from the extension.
pub struct CanvasLintCompositionTool {
    svc: Arc<CanvasVideoService>,
}

impl CanvasLintCompositionTool {
    pub fn new(svc: Arc<CanvasVideoService>) -> Self {
        Self { svc }
    }
}

#[async_trait]
impl ToolExecutor for CanvasLintCompositionTool {
    async fn execute(&self, _name: &str, arguments: &Value) -> Result<String> {
        let req: LintCompositionRequest = serde_json::from_value(arguments.clone())
            .map_err(|e| DaemonError::InvalidRequest(format!("canvas_lint_composition: {e}")))?;
        let report = self.svc.lint_composition(&req.composition_id).await?;
        serde_json::to_string(&report)
            .map_err(|e| DaemonError::InternalError(format!("serialize lint report: {e}")))
    }
}

/// JSON Schema for `canvas_lint_composition`.
pub fn lint_composition_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "composition_id": { "type": "string" }
        },
        "required": ["composition_id"]
    })
}

/// Register all three canvas.video.* tools on an existing `ToolRegistry`.
///
/// Call sites inject the shared `CanvasVideoService` so all tools and
/// bridge handlers see the same job registry / composition store.
pub fn register(registry: &mut crate::agent::tools::ToolRegistry, svc: Arc<CanvasVideoService>) {
    registry.register(
        "canvas_create_composition",
        Box::new(CanvasCreateCompositionTool::new(svc.clone())),
    );
    registry.register(
        "canvas_render_video",
        Box::new(CanvasRenderVideoTool::new(svc.clone())),
    );
    registry.register(
        "canvas_lint_composition",
        Box::new(CanvasLintCompositionTool::new(svc)),
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
        // The dispatch boundary now rejects `html`, and the test SkillRegistry
        // is empty (no template files loaded), so a template payload reaches
        // the service layer and surfaces SkillAssetNotFound. That proves the
        // dispatch path: deserialize -> service::create_composition ->
        // resolve_index_html template branch.
        let args = serde_json::json!({
            "title": "demo",
            "width": 640,
            "height": 360,
            "duration_sec": 1.0,
            "fps": 30,
            "template": "tiktok-hook"
        });
        let err = tool
            .execute("canvas_create_composition", &args)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("skill asset not found") || msg.contains("SkillAssetNotFound"),
            "expected service-layer SkillAssetNotFound, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_canvas_create_composition_tool_rejects_html_field() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let tool = CanvasCreateCompositionTool::new(svc);
        // LLM-style payload: schema-valid required fields PLUS a sneaky html
        // (which the struct accepts, but the dispatch layer must reject).
        let args = serde_json::json!({
            "title": "demo",
            "width": 640,
            "height": 360,
            "duration_sec": 1.0,
            "fps": 30,
            "html": "<html><body>sneaky</body></html>"
        });
        let err = tool
            .execute("canvas_create_composition", &args)
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("`html` field is not accepted") && msg.contains("template"),
            "expected html-rejection error, got: {msg}"
        );
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
    async fn test_register_adds_all_three_tools() {
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let mut registry = ToolRegistry::empty();
        register(&mut registry, svc);
        assert!(registry.has_tool("canvas_create_composition"));
        assert!(registry.has_tool("canvas_render_video"));
        assert!(registry.has_tool("canvas_lint_composition"));
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

        // template is REQUIRED and constrained to the seven shipped names.
        assert!(props.get("template").is_some(), "template field missing");
        let tpl_enum = props["template"]["enum"].as_array().unwrap();
        assert_eq!(tpl_enum.len(), 7);
        let required = s["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v == "template"),
            "template must be required, got: {required:?}"
        );

        // html must NOT be in the LLM-facing schema (LLMs persistently
        // preferred it over template before this lockdown).
        assert!(
            props.get("html").is_none(),
            "html field must be hidden from LLM schema"
        );
    }

    #[tokio::test]
    async fn test_lint_composition_schema_shape() {
        let s = lint_composition_schema();
        let props = &s["properties"];
        assert!(props.get("composition_id").is_some());
        assert_eq!(s["required"][0], "composition_id");
    }

    #[tokio::test]
    async fn test_lint_composition_tool_returns_report_for_resolved_correlator() {
        use nevoflux_protocol::canvas_video::{CreateCompositionRequest, LintReport};
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        // Seed a composition so the tool has something to look up.
        let resp = svc
            .create_composition(CreateCompositionRequest {
                title: "t".into(),
                width: 640,
                height: 360,
                duration_sec: 5.0,
                fps: 30,
                bg: None,
                html: Some("<html><body></body></html>".into()),
                template: None,
                session_id: None,
            })
            .await
            .unwrap();
        // Spawn a task that mimics the extension: whenever the service has a
        // pending correlator, resolve it with an empty report.
        let svc_c = svc.clone();
        tokio::spawn(async move {
            for _ in 0..50 {
                if let Some(c) = svc_c.peek_pending_lint_correlator().await {
                    svc_c.on_lint_result(&c, LintReport::default()).await;
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });
        let tool = CanvasLintCompositionTool::new(svc.clone());
        let args = serde_json::json!({ "composition_id": resp.artifact_id });
        let out = tool
            .execute("canvas_lint_composition", &args)
            .await
            .unwrap();
        assert!(out.contains("\"errors\""), "got: {out}");
    }

    #[tokio::test]
    #[ignore]
    async fn test_lint_composition_tool_times_out_when_no_resolver() {
        use nevoflux_protocol::canvas_video::CreateCompositionRequest;
        let svc = Arc::new(CanvasVideoService::new_for_tests());
        let resp = svc
            .create_composition(CreateCompositionRequest {
                title: "t".into(),
                width: 640,
                height: 360,
                duration_sec: 5.0,
                fps: 30,
                bg: None,
                html: Some("<html><body></body></html>".into()),
                template: None,
                session_id: None,
            })
            .await
            .unwrap();
        let tool = CanvasLintCompositionTool::new(svc.clone());
        let args = serde_json::json!({ "composition_id": resp.artifact_id });
        let err = tool
            .execute("canvas_lint_composition", &args)
            .await
            .unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("timeout"),
            "got: {err}"
        );
    }
}
