//! pack.* RPC handlers. Long ops (install/uninstall/update) run in the
//! background and stream `PackProgress` on `system:pack:progress`, mirroring
//! kb_wizard. Sync ops (validate/list/status) return inline.
//!
//! Param shape note: the server's `system_command` dispatch flattens the
//! incoming `payload.params` object into the `params` value passed here and
//! inserts `request_id` at its top level. So handlers read `request_id` and
//! every command-specific field (e.g. `manifest_path`, `name`) directly off
//! `params` — there is no nested `params.params`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use nevoflux_pack::capability;
use nevoflux_pack::manifest::Manifest;

use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};

pub const PROGRESS_TOPIC: &str = "system:pack:progress";

/// A progress frame forwarded to the EventBus during a long pack op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackProgress {
    pub op_id: String,
    pub phase: String,
    pub status: String,
    pub progress_pct: u8,
    pub log: String,
}

impl PackProgress {
    pub fn from_engine(op_id: &str, p: &nevoflux_pack::host::PackProgress) -> Self {
        Self {
            op_id: op_id.to_string(),
            phase: format!("{:?}", p.phase),
            status: format!("{:?}", p.status),
            progress_pct: p.progress_pct,
            log: p.log.clone(),
        }
    }
}

/// Publish a progress frame on the pack progress topic (best-effort).
pub async fn publish_progress(bus: &Arc<EventBus>, frame: &PackProgress) {
    let payload = match serde_json::to_value(frame) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize pack progress");
            return;
        }
    };
    let event = BusEvent::ephemeral(PROGRESS_TOPIC, payload, PublisherIdentity::Internal);
    if let Err(e) = bus.publish(event).await {
        tracing::warn!(error = %e, topic = PROGRESS_TOPIC, "failed to publish pack progress");
    }
}

fn ok(request_id: &str, command: &str, data: Value) -> Value {
    serde_json::json!({
        "type": "system_response",
        "payload": { "request_id": request_id, "command": command, "success": true, "data": data }
    })
}

fn err(request_id: &str, command: &str, code: &str, message: &str) -> Value {
    serde_json::json!({
        "type": "system_response",
        "payload": { "request_id": request_id, "command": command, "success": false,
                     "error": { "code": code, "message": message } }
    })
}

/// Read + parse a manifest file; returns (Manifest, raw_toml, pack_dir).
fn load_manifest(
    manifest_path: &str,
) -> Result<(Manifest, String, std::path::PathBuf), String> {
    let path = std::path::Path::new(manifest_path);
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {manifest_path}: {e}"))?;
    let m = Manifest::parse(&raw).map_err(|e| format!("parse manifest: {e}"))?;
    let pack_dir = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    Ok((m, raw, pack_dir))
}

/// pack.validate — pure capability check, no mutation. Used by `--dry-run`.
pub fn handle_pack_validate(params: &Value) -> Value {
    let request_id = params.get("request_id").and_then(|v| v.as_str()).unwrap_or("");
    let manifest_path = match params.get("manifest_path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return err(
                request_id,
                "pack.validate",
                "MISSING_PARAM",
                "manifest_path required",
            )
        }
    };
    let (m, raw, _dir) = match load_manifest(manifest_path) {
        Ok(t) => t,
        Err(e) => return err(request_id, "pack.validate", "BAD_MANIFEST", &e),
    };
    let paths = crate::paths::resolve_from_daemon();
    match capability::validate(&m, &paths, &raw) {
        Ok(()) => ok(
            request_id,
            "pack.validate",
            serde_json::json!({ "ok": true, "violations": [] }),
        ),
        Err(violations) => {
            let v: Vec<String> = violations.iter().map(|x| format!("{x:?}")).collect();
            ok(
                request_id,
                "pack.validate",
                serde_json::json!({ "ok": false, "violations": v }),
            )
        }
    }
}
