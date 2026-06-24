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

/// pack.validate — pure capability check, no mutation. Used by `--dry-run`.
/// Accepts a `source` (local path OR github:…) or the legacy `manifest_path`.
pub async fn handle_pack_validate(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Accept `source` (local path OR github:…) or legacy `manifest_path`.
    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .or_else(|| params.get("manifest_path").and_then(|v| v.as_str()));
    let source = match source {
        Some(s) => s.to_string(),
        None => {
            return err(
                &request_id,
                "pack.validate",
                "MISSING_PARAM",
                "source or manifest_path required",
            )
        }
    };
    let data_dir = crate::paths::resolve_from_daemon().data_dir;
    // `resolved` (with its TempDir guard, for remote sources) is held until the
    // end of this function — the capability check below reads `pack.toml`
    // synchronously, so the extracted files must stay alive that long.
    let resolved = match crate::pack::fetch::resolve_source(&source, &data_dir).await {
        Ok(r) => r,
        Err(e) => return err(&request_id, "pack.validate", "FETCH_FAILED", &e.to_string()),
    };
    let raw = match std::fs::read_to_string(resolved.pack_dir.join("pack.toml")) {
        Ok(s) => s,
        Err(e) => {
            return err(
                &request_id,
                "pack.validate",
                "MANIFEST_NOT_FOUND",
                &e.to_string(),
            )
        }
    };
    let m = match Manifest::parse(&raw) {
        Ok(m) => m,
        Err(e) => return err(&request_id, "pack.validate", "BAD_MANIFEST", &e),
    };
    let paths = crate::paths::resolve_from_daemon();
    match capability::validate(&m, &paths, &raw) {
        Ok(()) => ok(
            &request_id,
            "pack.validate",
            serde_json::json!({ "ok": true, "violations": [] }),
        ),
        Err(violations) => {
            let v: Vec<String> = violations.iter().map(|x| format!("{x:?}")).collect();
            ok(
                &request_id,
                "pack.validate",
                serde_json::json!({ "ok": false, "violations": v }),
            )
        }
    }
    // `resolved` (and its TempDir) drops here.
}

/// pack.list — enumerate installed packs by reading {config}/packs/*/receipt.json.
pub fn handle_pack_list(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let paths = crate::paths::resolve_from_daemon();
    let mut packs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths.packs_dir()) {
        for e in entries.flatten() {
            let receipt = e.path().join("receipt.json");
            if let Ok(s) = std::fs::read_to_string(&receipt) {
                if let Ok(r) = serde_json::from_str::<nevoflux_pack::receipt::Receipt>(&s) {
                    packs.push(serde_json::json!({
                        "name": r.pack, "version": r.version.to_string(),
                        "installed_at": r.installed_at, "source": r.source
                    }));
                }
            }
        }
    }
    ok(
        request_id,
        "pack.list",
        serde_json::json!({ "packs": packs }),
    )
}

/// pack.status — receipt summary for one pack.
pub fn handle_pack_status(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
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
                    "seeded_pages": r.seeded_pages, "source": r.source
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
    // Accept `source` (local path OR github:…) or legacy `manifest_path`.
    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .or_else(|| params.get("manifest_path").and_then(|v| v.as_str()));
    let source = match source {
        Some(s) => s.to_string(),
        None => {
            return err(
                &request_id,
                "pack.install",
                "MISSING_PARAM",
                "source or manifest_path required",
            )
        }
    };
    let force = params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let wait = params.get("wait").and_then(|v| v.as_bool()).unwrap_or(true); // CLI default

    let op_id = new_op_id();

    // For background (`wait:false`) installs, surface a Fetch frame so the UI
    // shows download progress while `resolve_source` runs.
    if !wait {
        if let Some(bus) = crate::kb_wizard::CURRENT_EVENT_BUS.get().cloned() {
            publish_progress(
                &bus,
                &PackProgress {
                    op_id: op_id.clone(),
                    phase: "Fetch".into(),
                    status: "Running".into(),
                    progress_pct: 5,
                    log: format!("fetching {source}"),
                },
            )
            .await;
        }
    }

    let data_dir = crate::paths::resolve_from_daemon().data_dir;
    let resolved = match crate::pack::fetch::resolve_source(&source, &data_dir).await {
        Ok(r) => r,
        Err(e) => return err(&request_id, "pack.install", "FETCH_FAILED", &e.to_string()),
    };
    let pack_dir = resolved.pack_dir.clone();
    let raw = match std::fs::read_to_string(pack_dir.join("pack.toml")) {
        Ok(s) => s,
        Err(e) => {
            return err(
                &request_id,
                "pack.install",
                "MANIFEST_NOT_FOUND",
                &e.to_string(),
            )
        }
    };
    let manifest = match Manifest::parse(&raw) {
        Ok(m) => m,
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

    let build = build_host(services, op_id.clone());
    // The pure engine is time-free; the daemon stamps install time. The CLI
    // doesn't send `now_utc`, so fall back to the current UTC time here.
    let mut now_utc = params
        .get("now_utc")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if now_utc.is_empty() {
        now_utc = chrono::Utc::now().to_rfc3339();
    }
    let opts = InstallOpts {
        force,
        now_utc,
        source: resolved.origin.clone(),
        tarball_sha256: resolved.tarball_sha256.clone(),
    };

    // Capture the runtime handle BEFORE spawn_blocking so the host's brain/bus
    // block_on bridges always resolve a runtime (Handle::current() inside a
    // blocking thread can be fragile; capture it on the async task instead).
    let handle = tokio::runtime::Handle::current();
    // CRITICAL: `resolved` owns the TempDir that holds the extracted remote pack
    // files. `lifecycle::install` reads those files synchronously inside the
    // blocking closure, so `resolved` MUST be moved INTO the closure and outlive
    // the install. We bind it here and reference it (a no-op read) so the
    // closure captures it by move rather than letting it drop early.
    let run = move || {
        let _keep_alive = &resolved; // hold the TempDir until install finishes
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
            Ok(Err(e)) => err(
                &request_id,
                "pack.install",
                "INSTALL_FAILED",
                &e.to_string(),
            ),
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
        None => {
            return err(
                &request_id,
                "pack.uninstall",
                "MISSING_PARAM",
                "name required",
            )
        }
    };
    let purge_data = params
        .get("purge_data")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let force = params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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

/// Re-read the receipt at `receipt_path`, set its provenance fields, and write
/// it back. Returns an error on any I/O or (de)serialization failure so the
/// caller can warn about lost provenance.
fn patch_receipt_provenance(
    receipt_path: &std::path::Path,
    origin: Option<String>,
    tarball_sha256: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let s = std::fs::read_to_string(receipt_path)?;
    let mut rec = serde_json::from_str::<nevoflux_pack::receipt::Receipt>(&s)?;
    rec.source = origin;
    rec.tarball_sha256 = tarball_sha256;
    let out = serde_json::to_string_pretty(&rec)?;
    std::fs::write(receipt_path, out)?;
    Ok(())
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
    // Accept `source` (local path OR github:…) or legacy `manifest_path`.
    let source = params
        .get("source")
        .and_then(|v| v.as_str())
        .or_else(|| params.get("manifest_path").and_then(|v| v.as_str()));
    let source = match source {
        Some(s) => s.to_string(),
        None => {
            return err(
                &request_id,
                "pack.update",
                "MISSING_PARAM",
                "source or manifest_path required",
            )
        }
    };
    let data_dir = crate::paths::resolve_from_daemon().data_dir;
    let resolved = match crate::pack::fetch::resolve_source(&source, &data_dir).await {
        Ok(r) => r,
        Err(e) => return err(&request_id, "pack.update", "FETCH_FAILED", &e.to_string()),
    };
    let pack_dir = resolved.pack_dir.clone();
    let raw = match std::fs::read_to_string(pack_dir.join("pack.toml")) {
        Ok(s) => s,
        Err(e) => {
            return err(
                &request_id,
                "pack.update",
                "MANIFEST_NOT_FOUND",
                &e.to_string(),
            )
        }
    };
    let manifest = match Manifest::parse(&raw) {
        Ok(m) => m,
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
    // The pure engine is time-free; the daemon stamps update time. The CLI
    // doesn't send `now_utc`, so fall back to the current UTC time here.
    let mut now_utc = params
        .get("now_utc")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if now_utc.is_empty() {
        now_utc = chrono::Utc::now().to_rfc3339();
    }
    let pack_name = manifest.pack.name.clone();
    // Carry provenance from the resolved source into the refreshed receipt.
    // `lifecycle::update` builds its own InstallOpts internally (it does not
    // accept provenance), so we patch the written receipt afterwards.
    let origin = resolved.origin.clone();
    let tarball_sha256 = resolved.tarball_sha256.clone();
    let build = build_host(services, new_op_id());
    let handle = tokio::runtime::Handle::current();
    // CRITICAL: `resolved` owns the TempDir holding the extracted remote pack
    // files; `lifecycle::update` reads those files synchronously inside the
    // blocking closure, so `resolved` MUST be moved INTO the closure and outlive
    // the update.
    let run = move || {
        let _keep_alive = &resolved; // hold the TempDir until update finishes
        let host = build.into_host(handle);
        lifecycle::update(&host, &manifest, &raw, &pack_dir, &now_utc)
    };
    match tokio::task::spawn_blocking(run).await {
        Ok(Ok(r)) => {
            // Persist provenance onto the freshly written receipt (best-effort).
            // A failure here loses provenance (source/tarball_sha256) for an
            // otherwise-successful update, so warn rather than fail silently.
            if origin.is_some() || tarball_sha256.is_some() {
                let paths = crate::paths::resolve_from_daemon();
                let receipt_path = paths.receipt_path(&pack_name);
                if let Err(e) = patch_receipt_provenance(&receipt_path, origin, tarball_sha256) {
                    tracing::warn!(
                        error = %e,
                        pack = %pack_name,
                        receipt = %receipt_path.display(),
                        "failed to patch update receipt provenance; source/tarball_sha256 not recorded"
                    );
                }
            }
            ok(
                &request_id,
                "pack.update",
                serde_json::json!({ "success": true, "version": r.version.to_string() }),
            )
        }
        Ok(Err(e)) => err(&request_id, "pack.update", "UPDATE_FAILED", &e.to_string()),
        Err(e) => err(&request_id, "pack.update", "JOIN_ERROR", &e.to_string()),
    }
}

/// Build the pack.inspect preview JSON from the parsed manifest, the gathered
/// skill names, the canvas-tool (name, binary) pairs, capability violations,
/// and the resolved origin. Pure — all I/O is done by the caller.
pub fn summarize_inspect(
    manifest: &nevoflux_pack::manifest::Manifest,
    skill_names: &[String],
    tool_binaries: &[(String, Option<String>)],
    violations: &[String],
    origin: Option<&str>,
    tarball_sha256: Option<&str>,
) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = tool_binaries
        .iter()
        .map(|(name, bin)| serde_json::json!({ "name": name, "binary": bin }))
        .collect();
    serde_json::json!({
        "source": origin,
        "tarball_sha256": tarball_sha256,
        "pack": {
            "name": manifest.pack.name,
            "version": manifest.pack.version.to_string(),
            "description": manifest.pack.description,
            "authors": manifest.pack.authors,
        },
        "components": {
            "skills": skill_names,
            "canvas_tools": tools,
            "seed": manifest.components.seed.iter().map(|s| s.slug.clone()).collect::<Vec<_>>(),
            "dashboard": manifest.components.dashboard.as_ref().map(|d| d.artifact_id.clone()),
            "knowledge": manifest.components.knowledge.is_some(),
        },
        "violations": violations,
    })
}

/// Read a pack dir's skill subdir names (one level) for the inspect preview.
fn gather_skill_names(
    pack_dir: &std::path::Path,
    manifest: &nevoflux_pack::manifest::Manifest,
) -> Vec<String> {
    let Some(skills) = &manifest.components.skills else {
        return Vec::new();
    };
    let dir = pack_dir.join(&skills.dir);
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                names.push(p.file_name().unwrap().to_string_lossy().into_owned());
            } else if p.extension().map(|x| x == "md").unwrap_or(false) {
                names.push(p.file_stem().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    names.sort();
    names
}

/// Read each declared canvas-tool TOML and extract (name, binary).
fn gather_tool_binaries(
    pack_dir: &std::path::Path,
    manifest: &nevoflux_pack::manifest::Manifest,
) -> Vec<(String, Option<String>)> {
    let Some(ct) = &manifest.components.canvas_tools else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in &ct.files {
        let path = pack_dir.join(f);
        let (mut name, mut binary) = (f.clone(), None);
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(toml::Value::Table(t)) = s.parse::<toml::Value>() {
                if let Some(n) = t.get("name").and_then(|v| v.as_str()) {
                    name = n.to_string();
                }
                binary = t
                    .get("binary")
                    .and_then(|v| v.as_str())
                    .map(|x| x.to_string());
            }
        }
        out.push((name, binary));
    }
    out
}

/// pack.inspect — fetch (if remote) + parse + capability check + preview. No install.
pub async fn handle_pack_inspect(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source = match params.get("source").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return err(
                &request_id,
                "pack.inspect",
                "MISSING_PARAM",
                "source required",
            )
        }
    };
    let data_dir = crate::paths::resolve_from_daemon().data_dir;
    let resolved = match crate::pack::fetch::resolve_source(&source, &data_dir).await {
        Ok(r) => r,
        Err(e) => return err(&request_id, "pack.inspect", "FETCH_FAILED", &e.to_string()),
    };
    let raw = match std::fs::read_to_string(resolved.pack_dir.join("pack.toml")) {
        Ok(s) => s,
        Err(e) => {
            return err(
                &request_id,
                "pack.inspect",
                "MANIFEST_NOT_FOUND",
                &e.to_string(),
            )
        }
    };
    let manifest = match nevoflux_pack::manifest::Manifest::parse(&raw) {
        Ok(m) => m,
        Err(e) => return err(&request_id, "pack.inspect", "BAD_MANIFEST", &e),
    };
    let paths = crate::paths::resolve_from_daemon();
    let violations: Vec<String> = match capability::validate(&manifest, &paths, &raw) {
        Ok(()) => Vec::new(),
        Err(vs) => vs.iter().map(|v| format!("{v:?}")).collect(),
    };
    let skills = gather_skill_names(&resolved.pack_dir, &manifest);
    let tools = gather_tool_binaries(&resolved.pack_dir, &manifest);
    let data = summarize_inspect(
        &manifest,
        &skills,
        &tools,
        &violations,
        resolved.origin.as_deref(),
        resolved.tarball_sha256.as_deref(),
    );
    ok(&request_id, "pack.inspect", data)
    // `resolved` (and its TempDir) drops here, cleaning up the fetched files.
}

#[cfg(test)]
mod tests {
    #[test]
    fn summarize_inspect_lists_components_and_flags_binary() {
        let src = r#"
[pack]
name = "demo"
version = "0.1.0"
protocol = "pack-protocol/0.1"
min_nevoflux = "0.3.0"
[components.skills]
dir = "components/skills"
[[components.seed]]
slug = "demo/cv"
from = "s.md"
[components.protected]
prefixes = ["demo/"]
"#;
        let m = nevoflux_pack::manifest::Manifest::parse(src).unwrap();
        let v = super::summarize_inspect(
            &m,
            &["demo-x".to_string()],
            &[("pdf.render".to_string(), Some("weasyprint".to_string()))],
            &[],
            Some("github:u/r@v1"),
            Some("abc"),
        );
        assert_eq!(v["pack"]["name"], "demo");
        assert_eq!(v["components"]["skills"][0], "demo-x");
        assert_eq!(v["components"]["canvas_tools"][0]["binary"], "weasyprint");
        assert_eq!(v["components"]["seed"][0], "demo/cv");
        assert_eq!(v["source"], "github:u/r@v1");
    }
}

#[cfg(test)]
mod inspect_preview_contract_tests {
    use nevoflux_pack::manifest::Manifest;

    const MINIMAL_PACK_TOML: &str = r#"
[pack]
name = "test-pack"
version = "0.1.0"
description = "A test pack"
authors = ["Tester"]
protocol = "pack-protocol/0.1"
min_nevoflux = "0.3.0"
"#;

    #[test]
    fn summarize_inspect_exposes_card_fields() {
        let manifest = Manifest::parse(MINIMAL_PACK_TOML).expect("manifest parses");
        let skills = vec!["skill-a".to_string()];
        let tools: Vec<(String, Option<String>)> =
            vec![("pdf".to_string(), Some("wkhtmltopdf".to_string()))];
        let violations: Vec<String> = Vec::new();

        let data = super::summarize_inspect(
            &manifest,
            &skills,
            &tools,
            &violations,
            Some("github:owner/test-pack@v1"),
            Some("deadbeef"),
        );

        // Preview-card contract: these keys must exist or the deep-link page breaks.
        assert_eq!(data["pack"]["name"], "test-pack");
        assert!(data["pack"]["version"].is_string());
        assert!(data["pack"]["description"].is_string());
        assert!(data["pack"]["authors"].is_array());
        assert!(data["components"]["skills"].is_array());
        assert!(data["components"]["canvas_tools"].is_array());
        assert_eq!(
            data["components"]["canvas_tools"][0]["binary"],
            "wkhtmltopdf"
        );
        assert!(data["violations"].is_array());
        assert_eq!(data["source"], "github:owner/test-pack@v1");
        assert_eq!(data["tarball_sha256"], "deadbeef");
    }
}
