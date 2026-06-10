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

/// pack.list — enumerate installed packs by reading {config}/packs/*/receipt.json.
pub fn handle_pack_list(params: &Value) -> Value {
    let request_id = params.get("request_id").and_then(|v| v.as_str()).unwrap_or("");
    let paths = crate::paths::resolve_from_daemon();
    let mut packs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths.packs_dir()) {
        for e in entries.flatten() {
            let receipt = e.path().join("receipt.json");
            if let Ok(s) = std::fs::read_to_string(&receipt) {
                if let Ok(r) = serde_json::from_str::<nevoflux_pack::receipt::Receipt>(&s) {
                    packs.push(serde_json::json!({
                        "name": r.pack, "version": r.version.to_string(),
                        "installed_at": r.installed_at
                    }));
                }
            }
        }
    }
    ok(request_id, "pack.list", serde_json::json!({ "packs": packs }))
}

/// pack.status — receipt summary for one pack.
pub fn handle_pack_status(params: &Value) -> Value {
    let request_id = params.get("request_id").and_then(|v| v.as_str()).unwrap_or("");
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return err(request_id, "pack.status", "MISSING_PARAM", "name required"),
    };
    let paths = crate::paths::resolve_from_daemon();
    match std::fs::read_to_string(paths.receipt_path(name)) {
        Ok(s) => match serde_json::from_str::<nevoflux_pack::receipt::Receipt>(&s) {
            Ok(r) => ok(
                request_id,
                "pack.status",
                serde_json::json!({
                    "installed": true, "version": r.version.to_string(),
                    "files": r.files.len(), "artifacts": r.artifacts,
                    "seeded_pages": r.seeded_pages
                }),
            ),
            Err(e) => err(request_id, "pack.status", "BAD_RECEIPT", &e.to_string()),
        },
        Err(_) => ok(
            request_id,
            "pack.status",
            serde_json::json!({ "installed": false }),
        ),
    }
}

use nevoflux_pack::lifecycle::{self, InstallOpts, UninstallOpts};

fn new_op_id() -> String {
    // Monotonic-ish unique id without Date/rand: use an atomic counter.
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(1);
    format!("pack-op-{}", CTR.fetch_add(1, Ordering::Relaxed))
}

/// Snapshot of everything a `PackHostImpl` needs, captured on the async task
/// before we hop onto a `spawn_blocking` thread. The runtime `Handle` is
/// captured here (NOT inside the blocking closure) so brain/bus `block_on`
/// bridges always have a valid runtime to drive.
pub struct PackHostImplBuild {
    paths: nevoflux_pack::paths::ResolvedPaths,
    db: std::sync::Arc<nevoflux_storage::Database>,
    skills: std::sync::Arc<tokio::sync::RwLock<nevoflux_skills::SkillRegistry>>,
    brain: Option<std::sync::Arc<dyn nevoflux_brain::BrainEngine>>,
    bus: Option<std::sync::Arc<crate::event_bus::EventBus>>,
    op_id: String,
}

impl PackHostImplBuild {
    fn into_host(self, handle: tokio::runtime::Handle) -> super::host_impl::PackHostImpl {
        super::host_impl::PackHostImpl {
            paths: self.paths,
            db: self.db,
            skills: self.skills,
            brain: self.brain,
            bus: self.bus,
            handle,
            op_id: self.op_id,
        }
    }
}

fn build_host(services: &crate::wasm::services::HostServices, op_id: String) -> PackHostImplBuild {
    // `try_read` avoids awaiting; if the slot is contended (a concurrent
    // hot-reload), brain is treated as unavailable for this op. A manifest
    // without seed never touches brain, so this is acceptable for v1.
    let brain = services.brain_slot.as_ref().and_then(|slot| {
        slot.try_read()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.engine.clone()))
    });
    PackHostImplBuild {
        paths: crate::paths::resolve_from_daemon(),
        db: services.database.clone(),
        skills: services.skills.clone(),
        brain,
        bus: crate::kb_wizard::CURRENT_EVENT_BUS.get().cloned(),
        op_id,
    }
}

pub async fn handle_pack_install(
    services: &crate::wasm::services::HostServices,
    params: &Value,
) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let manifest_path = match params.get("manifest_path").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return err(
                &request_id,
                "pack.install",
                "MISSING_PARAM",
                "manifest_path required",
            )
        }
    };
    let force = params.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let wait = params.get("wait").and_then(|v| v.as_bool()).unwrap_or(true); // CLI default

    let (manifest, raw, pack_dir) = match load_manifest(&manifest_path) {
        Ok(t) => t,
        Err(e) => return err(&request_id, "pack.install", "BAD_MANIFEST", &e),
    };
    // Knowledge deferred: reject up front with a helpful message.
    if manifest.components.knowledge.is_some() {
        return err(
            &request_id,
            "pack.install",
            "KNOWLEDGE_UNSUPPORTED",
            "[components.knowledge] is not supported yet (deferred until gbrain source-mapping lands). Remove it to install.",
        );
    }

    let op_id = new_op_id();
    let build = build_host(services, op_id.clone());
    let now_utc = params
        .get("now_utc")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let opts = InstallOpts { force, now_utc };

    // Capture the runtime handle BEFORE spawn_blocking so the host's brain/bus
    // block_on bridges always resolve a runtime (Handle::current() inside a
    // blocking thread can be fragile; capture it on the async task instead).
    let handle = tokio::runtime::Handle::current();
    let run = move || {
        let host = build.into_host(handle);
        lifecycle::install(&host, &manifest, &raw, &pack_dir, &opts)
    };

    if wait {
        match tokio::task::spawn_blocking(run).await {
            Ok(Ok(receipt)) => ok(
                &request_id,
                "pack.install",
                serde_json::json!({ "success": true, "version": receipt.version.to_string(),
                                    "files": receipt.files.len() }),
            ),
            Ok(Err(e)) => err(&request_id, "pack.install", "INSTALL_FAILED", &e.to_string()),
            Err(e) => err(&request_id, "pack.install", "JOIN_ERROR", &e.to_string()),
        }
    } else {
        let op_id2 = op_id.clone();
        let bus = crate::kb_wizard::CURRENT_EVENT_BUS.get().cloned();
        tokio::spawn(async move {
            let failure = match tokio::task::spawn_blocking(run).await {
                Ok(Ok(_)) => None, // success: lifecycle already emitted a terminal Ok frame
                Ok(Err(e)) => Some(e.to_string()),
                Err(e) => Some(format!("join error: {e}")),
            };
            // Early lifecycle returns (compat/capability/idempotency) don't emit a
            // terminal frame; publish one here so a UI awaiting completion via the
            // progress stream never hangs.
            if let (Some(log), Some(bus)) = (failure, bus) {
                let frame = PackProgress {
                    op_id: op_id2,
                    phase: "Commit".to_string(),
                    status: "Failed".to_string(),
                    progress_pct: 100,
                    log,
                };
                publish_progress(&bus, &frame).await;
            }
        });
        ok(
            &request_id,
            "pack.install",
            serde_json::json!({ "started": true, "op_id": op_id }),
        )
    }
}

pub async fn handle_pack_uninstall(
    services: &crate::wasm::services::HostServices,
    params: &Value,
) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return err(&request_id, "pack.uninstall", "MISSING_PARAM", "name required"),
    };
    let purge_data = params
        .get("purge_data")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let force = params.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let build = build_host(services, new_op_id());
    let handle = tokio::runtime::Handle::current();
    let run = move || {
        let host = build.into_host(handle);
        lifecycle::uninstall(&host, &name, &UninstallOpts { purge_data, force })
    };
    match tokio::task::spawn_blocking(run).await {
        Ok(Ok(())) => ok(
            &request_id,
            "pack.uninstall",
            serde_json::json!({ "success": true }),
        ),
        Ok(Err(e)) => err(
            &request_id,
            "pack.uninstall",
            "UNINSTALL_FAILED",
            &e.to_string(),
        ),
        Err(e) => err(&request_id, "pack.uninstall", "JOIN_ERROR", &e.to_string()),
    }
}

pub async fn handle_pack_update(
    services: &crate::wasm::services::HostServices,
    params: &Value,
) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let manifest_path = match params.get("manifest_path").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return err(
                &request_id,
                "pack.update",
                "MISSING_PARAM",
                "manifest_path required",
            )
        }
    };
    let (manifest, raw, pack_dir) = match load_manifest(&manifest_path) {
        Ok(t) => t,
        Err(e) => return err(&request_id, "pack.update", "BAD_MANIFEST", &e),
    };
    if manifest.components.knowledge.is_some() {
        return err(
            &request_id,
            "pack.update",
            "KNOWLEDGE_UNSUPPORTED",
            "[components.knowledge] not supported yet",
        );
    }
    let now_utc = params
        .get("now_utc")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let build = build_host(services, new_op_id());
    let handle = tokio::runtime::Handle::current();
    let run = move || {
        let host = build.into_host(handle);
        lifecycle::update(&host, &manifest, &raw, &pack_dir, &now_utc)
    };
    match tokio::task::spawn_blocking(run).await {
        Ok(Ok(r)) => ok(
            &request_id,
            "pack.update",
            serde_json::json!({ "success": true, "version": r.version.to_string() }),
        ),
        Ok(Err(e)) => err(&request_id, "pack.update", "UPDATE_FAILED", &e.to_string()),
        Err(e) => err(&request_id, "pack.update", "JOIN_ERROR", &e.to_string()),
    }
}
