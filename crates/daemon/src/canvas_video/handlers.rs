//! Bridge dispatch for canvas.video.* messages.

use std::sync::Arc;

use nevoflux_protocol::canvas_video::{
    CreateCompositionRequest, RenderCancelRequest, RenderFrameChunk, RenderReady,
    RenderStartRequest,
};
use serde_json::Value;

use crate::canvas_video::CanvasVideoService;
use crate::error::{DaemonError, Result};

pub async fn handle(
    svc: &Arc<CanvasVideoService>,
    message_type: &str,
    payload: Value,
) -> Result<Value> {
    match message_type {
        "canvas_video_create_composition" => {
            let req: CreateCompositionRequest = serde_json::from_value(payload)
                .map_err(|e| DaemonError::InvalidRequest(format!("parse: {}", e)))?;
            let resp = svc.create_composition(req).await?;
            serde_json::to_value(resp)
                .map_err(|e| DaemonError::InternalError(format!("{}", e)))
        }
        "canvas_video_render_start" => {
            let req: RenderStartRequest = serde_json::from_value(payload)
                .map_err(|e| DaemonError::InvalidRequest(format!("parse: {}", e)))?;
            let resp = svc.render_start(req).await?;
            serde_json::to_value(resp)
                .map_err(|e| DaemonError::InternalError(format!("{}", e)))
        }
        "canvas_video_render_cancel" => {
            let req: RenderCancelRequest = serde_json::from_value(payload)
                .map_err(|e| DaemonError::InvalidRequest(format!("parse: {}", e)))?;
            let cancelled = svc.jobs().cancel(&req.job_id).await;
            Ok(serde_json::json!({ "cancelled": cancelled }))
        }
        "canvas_video_ready" => {
            let m: RenderReady = serde_json::from_value(payload)
                .map_err(|e| DaemonError::InvalidRequest(format!("parse: {}", e)))?;
            svc.on_render_ready(&m.job_id).await;
            Ok(Value::Null)
        }
        "canvas_video_frame_chunk" => {
            let chunk: RenderFrameChunk = serde_json::from_value(payload)
                .map_err(|e| DaemonError::InvalidRequest(format!("parse: {}", e)))?;
            svc.on_frame_chunk(chunk).await?;
            Ok(Value::Null)
        }
        other => Err(DaemonError::InvalidRequest(format!(
            "unknown canvas.video.* message: {}",
            other
        ))),
    }
}
