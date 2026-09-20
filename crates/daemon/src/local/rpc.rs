//! `local.*` RPC surface: the control commands the browser calls to probe,
//! plan, install, configure and drive on-device (local) inference.
//!
//! Tasks 2.1-2.9 built the whole pipeline this module drives:
//! `hardware`/`catalog`/`memory`/`release` decide what a machine can run and
//! how big it is, `install` fetches and verifies the engine archive,
//! `marker`/`integrity` record and re-check what is on disk, and `engine`
//! (the [`crate::local::engine::EngineSupervisor`]) launches and supervises
//! the process once an install exists. None of that downloads the model
//! weights or drives the pre-install part of the state machine
//! (`AwaitingConsent` .. `DownloadingModel`) — `EngineSupervisor::cold_start`
//! only ever launches what is *already* installed, returning
//! [`crate::local::state::LocalError::EngineUpdateRequired`] otherwise. This
//! module owns that gap: `local.plan` sizes an install for the consent
//! modal, `local.install` downloads the engine archive (via
//! `crate::local::install::install`) and the model GGUF (via
//! [`download_model`], this module's model-download counterpart to
//! `crate::models::download_asset`) and then hands off to
//! `EngineSupervisor::ensure_started` to actually launch, `local.status`
//! reports the combined picture, and `local.set_default`/`local.set_config`
//! write the `[llm]`/`[llm.local]` config sections that drive everything
//! else.
//!
//! ## One canonical state slot
//!
//! `local.status`'s `state` field must reflect BOTH the pre-install phases
//! this module drives directly and the launch phases
//! `EngineSupervisor::cold_start` drives — otherwise a poll during a
//! download would report a stale `Idle`/`Ready` from a completely separate
//! slot. Rather than invent a second state cache that could drift from the
//! supervisor's own, this module drives pre-install transitions through the
//! SAME [`crate::local::engine::EngineSupervisor`] instance that later runs
//! `ensure_started` — see [`crate::local::engine::EngineSupervisor::set_state`],
//! bumped from private to `pub(crate)` by this task for exactly this reason.
//!
//! ## Progress vs. state
//!
//! Every byte-counted phase (`DownloadingEngine`, `DownloadingModel`) is
//! published TWICE, rate-limited the same way `crate::models::rpc` rate
//! limits its own tier downloads (`crate::models::should_emit`): once as a
//! [`crate::local::state::LocalState`] transition (sticky `TOPIC_STATE`, via
//! `EngineSupervisor::set_state`, so a late subscriber still gets the
//! current byte counts) and once as a raw `{phase, done, total}` frame on
//! [`crate::local::state::TOPIC_PROGRESS`] (ephemeral — see that constant's
//! own doc comment for why the two differ). Phases with no byte count
//! (`InstallingEngine`, `VerifyingEngine`) only get the state-side
//! transition, fired once at phase start.
//!
//! ## Cancellation is not an error (ruling R57)
//!
//! `local.install`/`local.update_engine`/`local.repair_engine` each hold the
//! [`tokio_util::sync::CancellationToken`] `local.cancel` cancels, so on any
//! failure from `crate::local::install::install` or [`download_model`] this
//! module checks `cancel.is_cancelled()` FIRST — that is the only reliable
//! signal, since a cancelled download surfaces as an ordinary
//! `LocalError::DownloadFailed { detail: "cancelled" }` (install.rs has no
//! dedicated variant for it). A cancelled run resets to [`LocalState::Idle`]
//! and publishes a `{"phase": "cancelled"}` progress frame; it never reaches
//! [`LocalState::Failed`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use nevoflux_storage::connection::Database;

use crate::config::AgentConfig;
use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};
use crate::kb_wizard::{err_response, ok_response, CURRENT_EVENT_BUS};
use crate::local::catalog;
use crate::local::config::{BackendPref, CtxPref, KvCacheType, LocalConfig, CTX_FLOOR};
use crate::local::engine::{self, EngineSupervisor};
use crate::local::hardware::{self, Backend, HardwareProbe, InstallKind};
use crate::local::install::{self, InstallProgress};
use crate::local::latch;
use crate::local::marker;
use crate::local::memory;
use crate::local::release::{self, ENGINE_PINNED};
use crate::local::state::{LocalError, LocalState};
use crate::local::sync;
use crate::models::fetch;
use crate::server::SharedAgentConfig;

fn request_id(params: &serde_json::Value) -> String {
    params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// The `<platform>-<backend>[-<variant>]` label an [`InstallKind`] displays
/// as — matches `crate::local::marker::Marker::kind`'s own convention, but
/// derived independently here (this is display formatting for
/// `local.status`'s JSON, not the directory-naming logic itself, which
/// stays solely `crate::local::install::install_dir`'s to get right).
fn kind_label(kind: &InstallKind) -> String {
    let backend = match kind.backend {
        Backend::Cpu => "cpu",
        Backend::Vulkan => "vulkan",
        Backend::Cuda => "cuda",
        Backend::Metal => "metal",
    };
    let mut s = format!("{}-{backend}", kind.platform);
    if let Some(v) = &kind.variant {
        s.push('-');
        s.push_str(v);
    }
    s
}

/// A concrete, already-hardware-resolved [`Backend`] as the [`BackendPref`]
/// that reaches it again via `hardware::fallback_chain`. `Metal` has no
/// preference of its own — macOS forces it regardless of `pref`
/// (`fallback_chain`'s `os == "macos"` branch returns before `pref` is even
/// read) — so `Auto` stands in for it here.
fn backend_to_pref(b: Backend) -> BackendPref {
    match b {
        Backend::Cpu => BackendPref::Cpu,
        Backend::Vulkan => BackendPref::Vulkan,
        Backend::Cuda => BackendPref::Cuda,
        Backend::Metal => BackendPref::Auto,
    }
}

/// Push `url`'s host onto `hosts` if it parses and isn't already present.
fn push_host(hosts: &mut Vec<String>, url: &str) {
    if let Some(host) = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
}

/// What's on disk for one catalog [`catalog::Quant`] under `dir`: present
/// (exact size match), partial (a `.part` shorter than the expected size),
/// or missing — mirrors `crate::models::state_of`, which cannot be reused
/// directly since `Quant` is catalog-local module's own shape, unrelated to
/// `crate::models::catalog::Asset`.
fn model_file_state(dir: &Path, q: &catalog::Quant) -> (&'static str, u64) {
    let dest = dir.join(q.file);
    if let Ok(m) = std::fs::metadata(&dest) {
        if m.len() == q.bytes {
            return ("present", q.bytes);
        }
        // Right name, wrong size: not ours -- a resume would corrupt it.
        return ("missing", 0);
    }
    match std::fs::metadata(fetch::part_path(&dest)) {
        Ok(m) if m.len() > 0 && m.len() < q.bytes => ("partial", m.len()),
        _ => ("missing", 0),
    }
}

// ── local.status ─────────────────────────────────────────────────────

/// The best installed (kind, marker) pair for `chain`, most-preferred first
/// — every supported tag (pinned, then each compatible one) is tried per
/// kind, mirroring `crate::local::engine`'s own (private) candidate walk,
/// but only for DISPLAY: this never decides what actually launches, that
/// stays `EngineSupervisor::cold_start`'s job alone.
fn installed_engine(root: &Path, chain: &[InstallKind]) -> Option<(InstallKind, marker::Marker)> {
    let tags: Vec<&str> = std::iter::once(ENGINE_PINNED.tag)
        .chain(release::ENGINE_COMPATIBLE.iter().map(|r| r.tag))
        .collect();
    for kind in chain {
        for tag in &tags {
            let dir = install::install_dir(root, tag, kind);
            if let Some(m) = marker::read_marker(&dir) {
                if m.bad.is_none() {
                    return Some((kind.clone(), m));
                }
            }
        }
    }
    None
}

fn status_with(
    cfg: &LocalConfig,
    supervisor: &EngineSupervisor,
    probe: Option<&HardwareProbe>,
    root: &Path,
    models_dir: Option<&Path>,
) -> serde_json::Value {
    let engine_json = probe
        .and_then(|p| installed_engine(root, &hardware::fallback_chain(p, cfg.backend)))
        .map(|(kind, m)| {
            let bytes: u64 = m.files.iter().map(|f| f.size).sum();
            let update = match marker::tag_status(&m.tag) {
                marker::TagStatus::Pinned => "none",
                marker::TagStatus::Compatible => "available",
                marker::TagStatus::Unsupported => "required",
            };
            serde_json::json!({
                "tag": m.tag,
                "kind": kind_label(&kind),
                "bytes": bytes,
                "update": update,
            })
        })
        .unwrap_or(serde_json::Value::Null);

    let model_json = catalog::model(&cfg.model)
        .and_then(|m| catalog::quant(m, &cfg.quant).map(|q| (m, q)))
        .map(|(m, q)| {
            let (state, have) = models_dir
                .map(|d| model_file_state(d, q))
                .unwrap_or(("missing", 0));
            serde_json::json!({
                "id": m.id,
                "quant": q.bits,
                "state": state,
                "have": have,
                "bytes": q.bytes,
            })
        })
        .unwrap_or(serde_json::Value::Null);

    serde_json::json!({
        "state": supervisor.state(),
        "latched": latch::is_on(),
        "config": cfg,
        "engine": engine_json,
        "model": model_json,
    })
}

pub async fn handle_status(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    let id = request_id(params);
    let cfg = shared_config.read().unwrap().llm.local.clone();
    let probe = Some(match hardware::cached() {
        Some(p) => p,
        None => hardware::probe().await,
    });
    let root = install::engine_root().unwrap_or_else(|| PathBuf::from("."));
    let models_dir = install::local_models_dir();
    let data = status_with(
        &cfg,
        engine::supervisor(),
        probe.as_ref(),
        &root,
        models_dir.as_deref(),
    );
    ok_response(&id, "local.status", data)
}

// ── local.probe ──────────────────────────────────────────────────────

pub async fn handle_probe(params: &serde_json::Value) -> serde_json::Value {
    let id = request_id(params);
    let refresh = params
        .get("refresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let probe = if refresh {
        hardware::probe().await
    } else {
        match hardware::cached() {
            Some(p) => p,
            None => hardware::probe().await,
        }
    };
    ok_response(
        &id,
        "local.probe",
        serde_json::to_value(&probe).unwrap_or(serde_json::Value::Null),
    )
}

// ── local.models ─────────────────────────────────────────────────────

/// `fit`/`estimate` are computed at ONE shared basis for every catalog
/// model: `Q8_0` KV cache, and a ctx derived from `fit`'s own auto-ladder
/// resolution (`choose_ctx(..., CtxPref::Auto, ...)`: `CTX_PREFERRED`
/// (32768) if the model fits fully on the GPU, else `CTX_FLOOR` (16384), or
/// `CTX_FLOOR` again for `Fit::Insufficient`, which carries no ctx of its
/// own — that is the size whose non-fit produced the verdict, and the
/// number the "try a smaller model" copy is implicitly about). Deliberately
/// NOT the currently-configured model's own (possibly `Fixed` to something
/// smaller) `ctx_size`, so the model-picker screen compares models on the
/// same basis rather than skewing toward whichever one happens to be active
/// right now (§11.1/§11.2/§12.2: the picker verdict is a property of
/// model × hardware via the auto ladder, not of the user's configured
/// `ctx_size`). `estimate` is computed at `fit`'s OWN resolved ctx (fix
/// round 1, Important 2) — an earlier version computed it at a fixed
/// `CTX_PREFERRED` regardless of what `fit` actually resolved to, so the
/// two fields in one card could describe different context sizes.
fn models_with(probe: Option<&HardwareProbe>, models_dir: Option<&Path>) -> serde_json::Value {
    let vram = probe
        .and_then(|p| p.nvidia_gpus.first())
        .map(|g| g.vram_bytes);
    let ram = probe.map(|p| p.ram_bytes).unwrap_or(0);
    let list: Vec<serde_json::Value> = catalog::MODELS
        .iter()
        .map(|m| {
            let quants: Vec<serde_json::Value> = m
                .quants
                .iter()
                .map(|q| {
                    let (state, have) = models_dir
                        .map(|d| model_file_state(d, q))
                        .unwrap_or(("missing", 0));
                    serde_json::json!({
                        "bits": q.bits,
                        "bytes": q.bytes,
                        "state": state,
                        "have": have,
                    })
                })
                .collect();
            // `m.quants[0]`: every catalog model has exactly one quant
            // today (`local/catalog.rs`), so this is lossless; a second
            // quant added later would need its own fit, not just inherit
            // the first's.
            let q0 = &m.quants[0];
            let fit = memory::choose_ctx(m, q0, CtxPref::Auto, KvCacheType::Q8_0, vram, ram);
            let ctx = match &fit {
                memory::Fit::FullGpu { ctx }
                | memory::Fit::PartialGpu { ctx, .. }
                | memory::Fit::CpuOnly { ctx } => *ctx,
                memory::Fit::Insufficient { .. } => CTX_FLOOR,
            };
            let estimate = memory::estimate(m, q0, ctx, KvCacheType::Q8_0, vram);
            serde_json::json!({
                "id": m.id,
                "display_name": m.display_name,
                "quants": quants,
                "fit": fit,
                "estimate": estimate,
            })
        })
        .collect();
    serde_json::json!(list)
}

pub async fn handle_models(params: &serde_json::Value) -> serde_json::Value {
    let id = request_id(params);
    let probe = Some(match hardware::cached() {
        Some(p) => p,
        None => hardware::probe().await,
    });
    let models_dir = install::local_models_dir();
    let data = models_with(probe.as_ref(), models_dir.as_deref());
    ok_response(&id, "local.models", data)
}

// ── local.plan ───────────────────────────────────────────────────────

/// One fallback-chain entry's download cost: the pinned release's
/// archive(s) for it, individually (for [`install::required_bytes`]'s
/// extraction-room math) and combined.
struct KindPlan {
    backend: Backend,
    archive_bytes: Vec<u64>,
    total: u64,
}

fn kind_plan(kind: &InstallKind) -> Option<KindPlan> {
    let (archive, cudart) = release::archive_for(&ENGINE_PINNED, kind)?;
    let mut archive_bytes = vec![archive.asset.bytes];
    if let Some(c) = cudart {
        archive_bytes.push(c.asset.bytes);
    }
    let total = archive_bytes.iter().sum();
    Some(KindPlan {
        backend: kind.backend,
        archive_bytes,
        total,
    })
}

/// Core of `local.plan`: size an install of `model_id`/`quant_bits` under
/// `backend_pref` against `probe`'s hardware, and what disk `root` has free.
/// `Err((code, message))` maps directly onto an `err_response`.
fn plan_with(
    model_id: &str,
    quant_bits: &str,
    backend_pref: BackendPref,
    probe: &HardwareProbe,
    root: &Path,
) -> Result<serde_json::Value, (&'static str, String)> {
    let model = catalog::model(model_id)
        .ok_or_else(|| ("unknown_model", format!("unknown model id {model_id:?}")))?;
    let quant = catalog::quant(model, quant_bits).ok_or_else(|| {
        (
            "unknown_quant",
            format!("{} has no {quant_bits:?} quantization", model.id),
        )
    })?;

    let chain = hardware::fallback_chain(probe, backend_pref);
    let plans: Vec<(InstallKind, KindPlan)> = chain
        .into_iter()
        .filter_map(|k| kind_plan(&k).map(|p| (k, p)))
        .collect();
    let Some((chosen_kind, chosen)) = plans.first() else {
        return Err((
            "backend_unavailable",
            "no usable backend on this machine".to_string(),
        ));
    };

    let alternatives: Vec<serde_json::Value> = plans[1..]
        .iter()
        .map(|(_, p)| serde_json::json!({"backend": p.backend, "engine_bytes": p.total}))
        .collect();

    let mut sources: Vec<String> = Vec::new();
    // Safe to re-resolve: `chosen_kind` came from `release::archive_for`
    // succeeding in `kind_plan` above.
    if let Some((archive, cudart)) = release::archive_for(&ENGINE_PINNED, chosen_kind) {
        for url in release::mirror_sources(ENGINE_PINNED.tag, archive.asset.name) {
            push_host(&mut sources, &url);
        }
        if let Some(c) = cudart {
            for url in release::mirror_sources(ENGINE_PINNED.tag, c.asset.name) {
                push_host(&mut sources, &url);
            }
        }
    }
    for url in quant.sources {
        push_host(&mut sources, url);
    }

    let needed = install::required_bytes(&chosen.archive_bytes, quant.bytes);
    let available = install::available_bytes(root).map_err(|e| ("disk_error", e.to_string()))?;

    Ok(serde_json::json!({
        "engine_bytes": chosen.total,
        "model_bytes": quant.bytes,
        "backend": chosen.backend,
        "alternatives": alternatives,
        "sources": sources,
        "disk": {"needed": needed, "available": available},
    }))
}

pub async fn handle_plan(params: &serde_json::Value) -> serde_json::Value {
    let id = request_id(params);
    let Some(model_id) = params.get("model").and_then(|v| v.as_str()) else {
        return err_response(&id, "local.plan", "unknown_model", "missing `model`");
    };
    let Some(quant_bits) = params.get("quant").and_then(|v| v.as_str()) else {
        return err_response(&id, "local.plan", "unknown_quant", "missing `quant`");
    };
    let backend_pref = match params.get("backend") {
        None => BackendPref::Auto,
        Some(v) => match serde_json::from_value::<BackendPref>(v.clone()) {
            Ok(b) => b,
            Err(_) => {
                return err_response(
                    &id,
                    "local.plan",
                    "unknown_backend",
                    "expected one of auto, cpu, vulkan, cuda",
                )
            }
        },
    };
    let probe = match hardware::cached() {
        Some(p) => p,
        None => hardware::probe().await,
    };
    let root = install::engine_root().unwrap_or_else(|| PathBuf::from("."));
    match plan_with(model_id, quant_bits, backend_pref, &probe, &root) {
        Ok(data) => ok_response(&id, "local.plan", data),
        Err((code, msg)) => err_response(&id, "local.plan", code, msg),
    }
}

// ── install machinery shared by local.install / update_engine / repair_engine ──

/// At most one heavy on-device operation (install/update/repair) runs at a
/// time — all three contend for the same engine-root lock and archive
/// anyway, so `local.cancel` cancels whichever is active without needing to
/// know which command started it.
static CURRENT_INSTALL: std::sync::Mutex<Option<CancellationToken>> = std::sync::Mutex::new(None);

fn begin_install() -> Option<CancellationToken> {
    let mut slot = CURRENT_INSTALL
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if slot.is_some() {
        return None;
    }
    let token = CancellationToken::new();
    *slot = Some(token.clone());
    Some(token)
}

fn finish_install() {
    *CURRENT_INSTALL
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
}

fn cancel_install() -> bool {
    let slot = CURRENT_INSTALL
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match slot.as_ref() {
        Some(t) => {
            t.cancel();
            true
        }
        None => false,
    }
}

/// Publish one ephemeral `{phase, done, total}` frame on
/// [`crate::local::state::TOPIC_PROGRESS`], swallowing bus errors (a flaky
/// EventBus must not take the download down with it — same rationale as
/// `crate::models::rpc`'s own `publish`). Spawned so this stays callable
/// from the synchronous `on_progress` callbacks `install::install` and
/// [`download_model`] both take.
fn publish_progress(bus: &Arc<EventBus>, phase: &'static str, done: u64, total: u64) {
    let bus = bus.clone();
    tokio::spawn(async move {
        let event = BusEvent::ephemeral(
            crate::local::state::TOPIC_PROGRESS,
            serde_json::json!({"phase": phase, "done": done, "total": total}),
            PublisherIdentity::Internal,
        );
        if let Err(e) = bus.publish(event).await {
            tracing::warn!(target: "local", error = %e, "failed to publish local progress");
        }
    });
}

/// Whether retrying an install-phase [`LocalError`] is worth offering: every
/// one of them is a download/verification hiccup that a plain retry can
/// plausibly get past, except [`LocalError::NoSpace`] (retrying without
/// freeing space fails the same way again).
fn install_error_retryable(e: &LocalError) -> bool {
    !matches!(e, LocalError::NoSpace { .. })
}

/// If `cancel` has fired, reset to [`LocalState::Idle`] and publish a
/// `{"phase":"cancelled"}` frame instead of a failure (ruling R57) — the
/// check every error branch AND phase-boundary checkpoint in `run_install`/
/// `run_engine_only` shares. Returns whether it fired, so a caller can
/// `return` immediately rather than fall through to writing config or
/// launching (fix round 1, Minor 7: a cancel that lands between two phases,
/// not just mid-transfer, must still be honoured).
fn handle_if_cancelled(
    supervisor: &EngineSupervisor,
    bus: &Arc<EventBus>,
    cancel: &CancellationToken,
) -> bool {
    if cancel.is_cancelled() {
        supervisor.set_state(LocalState::Idle);
        publish_progress(bus, "cancelled", 0, 0);
        true
    } else {
        false
    }
}

/// Releases [`CURRENT_INSTALL`] when dropped — including when the task
/// holding it panics (a panicking `tokio::spawn`ed task still unwinds and
/// drops its locals; it does not abort the process). Without this, a bug
/// elsewhere in the install pipeline could permanently wedge every future
/// `local.install`/`update_engine`/`repair_engine` behind
/// `{"reason":"already_running"}` with no way to clear it short of a daemon
/// restart, and `local.cancel` would report `true` for a token nothing is
/// listening to (fix round 1, Minor 8).
struct InstallGuard;

impl Drop for InstallGuard {
    fn drop(&mut self) {
        finish_install();
    }
}

/// Downloads `quant`'s GGUF into `dir`, trying its pinned sources in order —
/// this module's counterpart to `crate::models::download_asset`, mirrored
/// closely (same already-present short-circuit, same first-source-that-
/// works loop, same digest-mismatch mapping) but kept separate since
/// `catalog::Quant` has no `Asset`/`Tier` shape to share with it.
async fn download_model(
    quant: &catalog::Quant,
    dir: &Path,
    cancel: &CancellationToken,
    on_progress: &mut (dyn FnMut(u64, u64) + Send),
) -> Result<(), LocalError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| LocalError::DownloadFailed {
            detail: format!("{}: {e}", dir.display()),
        })?;
    let dest = dir.join(quant.file);
    if let Ok(m) = std::fs::metadata(&dest) {
        if m.len() == quant.bytes {
            on_progress(quant.bytes, quant.bytes);
            return Ok(());
        }
    }

    let available = install::available_bytes(dir).map_err(|e| LocalError::DownloadFailed {
        detail: format!("checking free space on {}: {e}", dir.display()),
    })?;
    if available < quant.bytes {
        return Err(LocalError::NoSpace {
            needed: quant.bytes,
            available,
        });
    }

    let client = crate::models::http_client();
    let mut last_err: Option<fetch::FetchError> = None;
    for url in quant.sources {
        if cancel.is_cancelled() {
            return Err(LocalError::DownloadFailed {
                detail: "cancelled".to_string(),
            });
        }
        match fetch::fetch_to(
            &client,
            url,
            &dest,
            quant.bytes,
            quant.sha256,
            cancel,
            on_progress,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retry = e.worth_another_source();
                tracing::warn!(url = %url, error = %e, "on-device model source failed");
                last_err = Some(e);
                if !retry {
                    break;
                }
            }
        }
    }

    Err(match last_err {
        Some(fetch::FetchError::Digest { .. }) => LocalError::ChecksumMismatch,
        Some(e) => LocalError::DownloadFailed {
            detail: e.to_string(),
        },
        None => LocalError::DownloadFailed {
            detail: "no source available".to_string(),
        },
    })
}

/// The engine-archive half of an install: download, extract, verify and
/// install `kind` from the pinned release, publishing `DownloadingEngine`
/// (rate-limited, state + progress) / `InstallingEngine` / `VerifyingEngine`
/// transitions through `supervisor` as it goes. Shared by `local.install`
/// (engine + model), `local.update_engine` and `local.repair_engine`
/// (engine only).
async fn install_engine_only(
    kind: &InstallKind,
    root: &Path,
    supervisor: &EngineSupervisor,
    bus: &Arc<EventBus>,
    cancel: &CancellationToken,
) -> Result<(), LocalError> {
    supervisor.set_state(LocalState::DownloadingEngine { done: 0, total: 0 });
    let mut last_emit = Instant::now();
    let mut last_done = 0u64;
    let mut on_progress = |p: InstallProgress| match p {
        InstallProgress::Downloading { done, total } => {
            if crate::models::should_emit(done, total, last_done, last_emit.elapsed()) {
                last_emit = Instant::now();
                last_done = done;
                supervisor.set_state(LocalState::DownloadingEngine { done, total });
                publish_progress(bus, "engine_download", done, total);
            }
        }
        InstallProgress::Extracting => supervisor.set_state(LocalState::InstallingEngine),
        InstallProgress::Verifying => supervisor.set_state(LocalState::VerifyingEngine),
    };
    install::install(&ENGINE_PINNED, kind, root, cancel, &mut on_progress)
        .await
        .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn run_install(
    model: &'static catalog::LlmModel,
    quant: &'static catalog::Quant,
    backend: Backend,
    supervisor: EngineSupervisor,
    bus: Arc<EventBus>,
    cancel: CancellationToken,
    shared_config: SharedAgentConfig,
    config_path: PathBuf,
    root: PathBuf,
    models_dir: Option<PathBuf>,
) {
    let probe = match hardware::cached() {
        Some(p) => p,
        None => hardware::probe().await,
    };
    let pref = backend_to_pref(backend);
    let chain = hardware::fallback_chain(&probe, pref);
    let Some(kind) = chain.first().cloned() else {
        supervisor.set_state(LocalState::Failed {
            error: LocalError::BackendUnavailable,
            retryable: true,
        });
        return;
    };
    if kind.backend != backend {
        // The hardware this machine actually has no longer matches what
        // `local.plan` proposed (a race, or a client replaying a stale
        // plan) -- refuse rather than silently install a different backend.
        supervisor.set_state(LocalState::Failed {
            error: LocalError::BackendUnavailable,
            retryable: true,
        });
        return;
    }

    if let Err(e) = install_engine_only(&kind, &root, &supervisor, &bus, &cancel).await {
        if handle_if_cancelled(&supervisor, &bus, &cancel) {
            return;
        }
        supervisor.set_state(LocalState::Failed {
            retryable: install_error_retryable(&e),
            error: e,
        });
        return;
    }
    // Checkpoint between phases (fix round 1, Minor 7): without this, a
    // cancel that lands in the gap between the engine finishing and the
    // model download starting is never observed, and the model download
    // proceeds regardless of `local.cancel` having already answered
    // `{cancelled: true}`.
    if handle_if_cancelled(&supervisor, &bus, &cancel) {
        return;
    }

    let Some(models_dir) = models_dir else {
        supervisor.set_state(LocalState::Failed {
            error: LocalError::DownloadFailed {
                detail: "no cache directory on this system".to_string(),
            },
            retryable: false,
        });
        return;
    };

    supervisor.set_state(LocalState::DownloadingModel {
        done: 0,
        total: quant.bytes,
    });
    let mut last_emit = Instant::now();
    let mut last_done = 0u64;
    let model_result = download_model(quant, &models_dir, &cancel, &mut |done, total| {
        if crate::models::should_emit(done, total, last_done, last_emit.elapsed()) {
            last_emit = Instant::now();
            last_done = done;
            supervisor.set_state(LocalState::DownloadingModel { done, total });
            publish_progress(&bus, "model_download", done, total);
        }
    })
    .await;

    if let Err(e) = model_result {
        if handle_if_cancelled(&supervisor, &bus, &cancel) {
            return;
        }
        supervisor.set_state(LocalState::Failed {
            retryable: install_error_retryable(&e),
            error: e,
        });
        return;
    }
    // Checkpoint after the model download's LAST byte but before writing
    // config/launching (fix round 1, Minor 7): a cancel that lands as the
    // final chunk arrives must not still enable+launch on-device inference
    // the user just asked to abandon.
    if handle_if_cancelled(&supervisor, &bus, &cancel) {
        return;
    }

    match AgentConfig::load_from_path(&config_path) {
        Ok(mut config) => {
            config.llm.local.enabled = true;
            config.llm.local.model = model.id.to_string();
            config.llm.local.quant = quant.bits.to_string();
            config.llm.local.backend = pref;
            match config.save_to_path(&config_path) {
                Ok(()) => {
                    *shared_config.write().unwrap() = Arc::new(config.clone());
                    crate::local::on_config_changed(&config).await;
                }
                Err(e) => {
                    supervisor.set_state(LocalState::Failed {
                        error: LocalError::DownloadFailed {
                            detail: format!(
                                "downloaded successfully but failed to save config: {e}"
                            ),
                        },
                        retryable: true,
                    });
                    return;
                }
            }
        }
        Err(e) => {
            supervisor.set_state(LocalState::Failed {
                error: LocalError::DownloadFailed {
                    detail: format!("downloaded successfully but failed to load config: {e}"),
                },
                retryable: true,
            });
            return;
        }
    }

    // Launch now rather than waiting for the next chat turn, so the UI
    // watching `TOPIC_STATE` sees `Ready` (or a launch `Failed`) without a
    // separate action. `ensure_started` already turns its own failure into
    // `LocalState::Failed` internally.
    let _ = supervisor.ensure_started().await;
}

pub async fn handle_install(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    let id = request_id(params);
    let Some(model) = params
        .get("model")
        .and_then(|v| v.as_str())
        .and_then(catalog::model)
    else {
        return err_response(
            &id,
            "local.install",
            "unknown_model",
            "unknown or missing model id",
        );
    };
    let Some(quant) = params
        .get("quant")
        .and_then(|v| v.as_str())
        .and_then(|b| catalog::quant(model, b))
    else {
        return err_response(
            &id,
            "local.install",
            "unknown_quant",
            format!("{} has no such quantization", model.id),
        );
    };
    let Some(backend) = params
        .get("backend")
        .cloned()
        .and_then(|v| serde_json::from_value::<Backend>(v).ok())
    else {
        return err_response(
            &id,
            "local.install",
            "unknown_backend",
            "expected one of cpu, vulkan, cuda, metal",
        );
    };
    let Some(bus) = CURRENT_EVENT_BUS.get().cloned() else {
        return err_response(
            &id,
            "local.install",
            "no_event_bus",
            "EventBus not initialised; cannot stream progress",
        );
    };
    let Some(cancel) = begin_install() else {
        return ok_response(
            &id,
            "local.install",
            serde_json::json!({"started": false, "reason": "already_running"}),
        );
    };
    let Ok(config_path) = AgentConfig::default_config_path() else {
        finish_install();
        return err_response(
            &id,
            "local.install",
            "config_error",
            "could not resolve the config file path",
        );
    };

    let root = install::engine_root().unwrap_or_else(|| PathBuf::from("."));
    let models_dir = install::local_models_dir();
    let supervisor = engine::supervisor().clone();
    let shared_config = shared_config.clone();
    tokio::spawn(async move {
        let _guard = InstallGuard;
        run_install(
            model,
            quant,
            backend,
            supervisor,
            bus,
            cancel,
            shared_config,
            config_path,
            root,
            models_dir,
        )
        .await;
    });

    ok_response(&id, "local.install", serde_json::json!({"started": true}))
}

// ── local.cancel ─────────────────────────────────────────────────────

pub async fn handle_cancel(params: &serde_json::Value) -> serde_json::Value {
    let id = request_id(params);
    let cancelled = cancel_install();
    ok_response(
        &id,
        "local.cancel",
        serde_json::json!({"cancelled": cancelled}),
    )
}

// ── local.update_engine / local.repair_engine ───────────────────────

async fn run_engine_only(
    repair: bool,
    supervisor: EngineSupervisor,
    bus: Arc<EventBus>,
    cancel: CancellationToken,
    root: PathBuf,
    backend_pref: BackendPref,
) {
    let probe = match hardware::cached() {
        Some(p) => p,
        None => hardware::probe().await,
    };
    let chain = hardware::fallback_chain(&probe, backend_pref);
    let Some(kind) = chain.first().cloned() else {
        supervisor.set_state(LocalState::Failed {
            error: LocalError::BackendUnavailable,
            retryable: true,
        });
        return;
    };

    if repair {
        // Release any handle a resident engine holds on the install's own
        // files FIRST -- on Windows especially, a running process keeps
        // `llama-server.exe` open, and removing the directory underneath it
        // fails silently into the arm below (fix round 1, Important 4).
        supervisor.stop().await;

        // `install::install` treats a directory with a parseable, matching
        // marker as an already-completed install without re-verifying its
        // contents -- a "repair" has to remove that marker first, or a
        // corrupted install would short-circuit right back to itself.
        let dir = install::install_dir(&root, ENGINE_PINNED.tag, &kind);
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                // A removal failure must not fall through to `install()`
                // finding the surviving (still-damaged) marker and
                // reporting "already installed" as a successful repair
                // (fix round 1, Important 4).
                supervisor.set_state(LocalState::Failed {
                    error: LocalError::EngineCorrupt {
                        detail: format!(
                            "could not remove the damaged install at {}: {e}",
                            dir.display()
                        ),
                    },
                    retryable: true,
                });
                return;
            }
        }
    }

    match install_engine_only(&kind, &root, &supervisor, &bus, &cancel).await {
        Ok(()) => {
            // Drop any stale resident process running the old binary so the
            // next demand cold-starts the freshly (re)installed one.
            supervisor.stop().await;
        }
        Err(e) => {
            if handle_if_cancelled(&supervisor, &bus, &cancel) {
                return;
            }
            supervisor.set_state(LocalState::Failed {
                retryable: install_error_retryable(&e),
                error: e,
            });
        }
    }
}

async fn handle_engine_only(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
    repair: bool,
    cmd: &'static str,
) -> serde_json::Value {
    let id = request_id(params);
    let Some(bus) = CURRENT_EVENT_BUS.get().cloned() else {
        return err_response(
            &id,
            cmd,
            "no_event_bus",
            "EventBus not initialised; cannot stream progress",
        );
    };
    let Some(cancel) = begin_install() else {
        return ok_response(
            &id,
            cmd,
            serde_json::json!({"started": false, "reason": "already_running"}),
        );
    };
    let backend_pref = shared_config.read().unwrap().llm.local.backend;
    let root = install::engine_root().unwrap_or_else(|| PathBuf::from("."));
    let supervisor = engine::supervisor().clone();
    tokio::spawn(async move {
        let _guard = InstallGuard;
        run_engine_only(repair, supervisor, bus, cancel, root, backend_pref).await;
    });
    ok_response(&id, cmd, serde_json::json!({"started": true}))
}

pub async fn handle_update_engine(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    handle_engine_only(params, shared_config, false, "local.update_engine").await
}

pub async fn handle_repair_engine(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    handle_engine_only(params, shared_config, true, "local.repair_engine").await
}

// ── local.retry_backend ──────────────────────────────────────────────

pub async fn handle_retry_backend(params: &serde_json::Value) -> serde_json::Value {
    let id = request_id(params);
    let Some(backend) = params
        .get("backend")
        .cloned()
        .and_then(|v| serde_json::from_value::<Backend>(v).ok())
    else {
        return err_response(
            &id,
            "local.retry_backend",
            "unknown_backend",
            "expected one of cpu, vulkan, cuda, metal",
        );
    };
    engine::supervisor().retry_backend(backend).await;
    ok_response(&id, "local.retry_backend", serde_json::json!({"ok": true}))
}

// ── local.set_default ────────────────────────────────────────────────

async fn set_default_with(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
    config_path: &Path,
    db: Option<&Database>,
) -> serde_json::Value {
    let id = request_id(params);
    let Some(on) = params.get("on").and_then(|v| v.as_bool()) else {
        return err_response(
            &id,
            "local.set_default",
            "missing_on",
            "expected a boolean `on`",
        );
    };

    let mut config = match AgentConfig::load_from_path(&config_path.to_path_buf()) {
        Ok(c) => c,
        Err(e) => {
            return err_response(
                &id,
                "local.set_default",
                "config_error",
                format!("failed to load config: {e}"),
            )
        }
    };

    if on {
        // The latch requires BOTH the active provider resolving to Local
        // AND `[llm.local].enabled` (`latch::refresh_from_config`) -- write
        // both here so this call can never report success while silently
        // leaving on-device the active provider with no latch and no
        // engine (fix round 1, Important 1).
        config.llm.provider = Some("local".to_string());
        config.llm.local.enabled = true;
    } else {
        let provider = params
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if provider.is_empty() {
            return err_response(
                &id,
                "local.set_default",
                "missing_provider",
                "on=false requires a `provider` naming the cloud provider to activate",
            );
        }
        match config.llm.resolve_wire(provider) {
            Some(nevoflux_llm::ProviderType::Local) | None => {
                return err_response(
                    &id,
                    "local.set_default",
                    "unknown_provider",
                    format!("unknown provider: {provider}"),
                );
            }
            Some(_) => {}
        }
        // Refuse to release the latch onto a provider with no API
        // key/base URL -- that would leave the user with no working
        // provider at all, one step removed from the outcome this
        // requirement exists to prevent (fix round 1, Minor 10).
        if !config.llm.is_provider_configured(provider) {
            return err_response(
                &id,
                "local.set_default",
                "provider_not_configured",
                format!(
                    "{provider} has no API key (or, for a custom provider, base URL) \
                     configured; releasing the latch onto it would leave no working provider"
                ),
            );
        }
        config.llm.provider = Some(provider.to_string());
    }

    if let Err(e) = config.save_to_path(&config_path.to_path_buf()) {
        return err_response(
            &id,
            "local.set_default",
            "config_error",
            format!("failed to save config: {e}"),
        );
    }
    *shared_config.write().unwrap() = Arc::new(config.clone());
    crate::local::on_config_changed(&config).await;

    let latched = latch::is_on();
    let (paused_loops, paused_schedules, paused_goals) =
        db.map(sync::paused_counts).unwrap_or((0, 0, 0));

    ok_response(
        &id,
        "local.set_default",
        serde_json::json!({
            "latched": latched,
            "paused_loops": paused_loops,
            "paused_schedules": paused_schedules,
            "paused_goals": paused_goals,
        }),
    )
}

pub async fn handle_set_default(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    let id = request_id(params);
    let Ok(config_path) = AgentConfig::default_config_path() else {
        return err_response(
            &id,
            "local.set_default",
            "config_error",
            "could not resolve the config file path",
        );
    };
    set_default_with(params, shared_config, &config_path, sync::CURRENT_DB.get()).await
}

// ── local.set_config ─────────────────────────────────────────────────

/// Whether `after` differs from `before` in a field the engine's launch
/// actually bakes in (argv / `n_ctx`). `idle_unload_secs` is deliberately
/// excluded: the idle timer reads it live on every tick, and it never
/// affects an already-running process, so a request that only changes it
/// must not pay for a multi-GB reload (fix round 1, Important 5).
fn launch_relevant_changed(before: &LocalConfig, after: &LocalConfig) -> bool {
    before.ctx_size != after.ctx_size
        || before.backend != after.backend
        || before.parallel != after.parallel
}

async fn set_config_with(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
    config_path: &Path,
    supervisor: &EngineSupervisor,
) -> serde_json::Value {
    let id = request_id(params);
    let mut config = match AgentConfig::load_from_path(&config_path.to_path_buf()) {
        Ok(c) => c,
        Err(e) => {
            return err_response(
                &id,
                "local.set_config",
                "config_error",
                format!("failed to load config: {e}"),
            )
        }
    };
    // Snapshot before any param is applied, so the launch-relevant fields
    // can be compared against what actually changed (see the conditional
    // `stop` below, fix round 1, Important 5).
    let before = config.llm.local.clone();

    if let Some(v) = params.get("ctx_size") {
        match serde_json::from_value::<CtxPref>(v.clone()) {
            Ok(ctx) => config.llm.local.ctx_size = ctx,
            Err(e) => {
                return err_response(
                    &id,
                    "local.set_config",
                    "bad_ctx",
                    format!("invalid ctx_size: {e}"),
                )
            }
        }
    }
    if let Some(v) = params.get("backend") {
        match serde_json::from_value::<BackendPref>(v.clone()) {
            Ok(b) => config.llm.local.backend = b,
            Err(_) => {
                return err_response(
                    &id,
                    "local.set_config",
                    "bad_backend",
                    "expected one of auto, cpu, vulkan, cuda",
                )
            }
        }
    }
    // `ctx_size`/`backend` above return typed errors for bad input; these
    // two now match (fix round 1, Important 3) -- `as_u64()` alone silently
    // accepted a string/float/negative number by doing nothing, and the
    // bare `as u32` truncated anything past `u32::MAX` into a
    // plausible-looking small value instead of erroring.
    if let Some(v) = params.get("parallel") {
        match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(p) if p >= 1 => config.llm.local.parallel = p,
            _ => {
                return err_response(
                    &id,
                    "local.set_config",
                    "bad_parallel",
                    "expected a positive integer",
                )
            }
        }
    }
    if let Some(v) = params.get("idle_unload_secs") {
        match v.as_u64() {
            Some(secs) => config.llm.local.idle_unload_secs = secs,
            None => {
                return err_response(
                    &id,
                    "local.set_config",
                    "bad_idle_unload_secs",
                    "expected a non-negative integer",
                )
            }
        }
    }
    // Model and quant, validated against the catalog. Without these the
    // settings page's model picker wrote into a void: the RPC answered
    // success, echoed the unchanged config, and the select snapped back with
    // no error anywhere (review of Task 4.2, Critical 3).
    if let Some(v) = params.get("model") {
        let Some(name) = v.as_str() else {
            return err_response(
                &id,
                "local.set_config",
                "unknown_model",
                "expected a string",
            );
        };
        let Some(model) = catalog::model(name) else {
            return err_response(
                &id,
                "local.set_config",
                "unknown_model",
                format!("unknown model id {name:?}"),
            );
        };
        // The quant must be valid FOR THE NEW MODEL. Checking them
        // independently would let a quant that exists on the old model
        // through, and the engine would then look for a file that is not
        // in the catalog at all.
        let quant_bits = match params.get("quant").and_then(|q| q.as_str()) {
            Some(q) => q.to_string(),
            None => config.llm.local.quant.clone(),
        };
        if catalog::quant(model, &quant_bits).is_none() {
            return err_response(
                &id,
                "local.set_config",
                "unknown_quant",
                format!("{name} has no {quant_bits:?} quantization"),
            );
        }
        config.llm.local.model = name.to_string();
        config.llm.local.quant = quant_bits;
    } else if let Some(v) = params.get("quant") {
        let Some(bits) = v.as_str() else {
            return err_response(
                &id,
                "local.set_config",
                "unknown_quant",
                "expected a string",
            );
        };
        let current = config.llm.local.model.clone();
        match catalog::model(&current) {
            Some(m) if catalog::quant(m, bits).is_some() => {
                config.llm.local.quant = bits.to_string()
            }
            _ => {
                return err_response(
                    &id,
                    "local.set_config",
                    "unknown_quant",
                    format!("{current} has no {bits:?} quantization"),
                )
            }
        }
    }

    if let Err(msg) = config.llm.local.validated_ctx() {
        return err_response(&id, "local.set_config", "bad_ctx", msg);
    }

    // Only `ctx_size`/`backend`/`parallel` are baked into the engine's
    // launch (argv/n_ctx); `idle_unload_secs` is read live by the idle
    // timer and never affects an already-running process, so it must NOT
    // force a multi-GB reload on its own (fix round 1, Important 5 --
    // "restarts engine on next demand" describes the effect of a changed
    // launch-relevant setting, not a mandate to tear down a resident
    // process on every call, including one that changed nothing at all).
    let launch_changed = launch_relevant_changed(&before, &config.llm.local);

    if let Err(e) = config.save_to_path(&config_path.to_path_buf()) {
        return err_response(
            &id,
            "local.set_config",
            "config_error",
            format!("failed to save config: {e}"),
        );
    }
    *shared_config.write().unwrap() = Arc::new(config.clone());
    crate::local::on_config_changed(&config).await;
    if launch_changed {
        supervisor.stop().await;
    }

    ok_response(
        &id,
        "local.set_config",
        serde_json::to_value(&config.llm.local).unwrap_or(serde_json::Value::Null),
    )
}

pub async fn handle_set_config(
    params: &serde_json::Value,
    shared_config: &SharedAgentConfig,
) -> serde_json::Value {
    let id = request_id(params);
    let Ok(config_path) = AgentConfig::default_config_path() else {
        return err_response(
            &id,
            "local.set_config",
            "config_error",
            "could not resolve the config file path",
        );
    };
    set_config_with(params, shared_config, &config_path, engine::supervisor()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    fn payload(v: &serde_json::Value) -> &serde_json::Value {
        v.get("payload").expect("system_response envelope")
    }

    fn ok(v: &serde_json::Value) -> bool {
        payload(v)
            .get("success")
            .and_then(|s| s.as_bool())
            .unwrap_or(false)
    }

    fn shared(cfg: AgentConfig) -> SharedAgentConfig {
        Arc::new(RwLock::new(Arc::new(cfg)))
    }

    fn windows_cuda13_older_probe() -> HardwareProbe {
        HardwareProbe {
            os: "windows".to_string(),
            arch: "x86_64".to_string(),
            nvidia_gpus: vec![hardware::GpuInfo {
                name: "Test GPU".to_string(),
                vram_bytes: 16 * memory::GIB,
                compute_cap: Some((7, 5)),
            }],
            has_physical_nvidia: true,
            has_usable_nvidia: true,
            driver_cuda_version: Some((13, 0)),
            cuda_runtime_lines: vec![],
            vulkan_available: false,
            ram_bytes: 32 * memory::GIB,
            macos_version: None,
        }
    }

    /// A machine with no usable GPU at all -- `BackendPref::Cpu` resolves
    /// to exactly one, hardware-independent `InstallKind` regardless of any
    /// of these fields, which is what makes `status_with`'s installed-marker
    /// test reproducible on any machine running the suite.
    fn cpu_only_probe() -> HardwareProbe {
        HardwareProbe {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            nvidia_gpus: vec![],
            has_physical_nvidia: false,
            has_usable_nvidia: false,
            driver_cuda_version: None,
            cuda_runtime_lines: vec![],
            vulkan_available: false,
            ram_bytes: 8 * memory::GIB,
            macos_version: None,
        }
    }

    /// Enough VRAM/RAM that every catalog model fits fully on the GPU at
    /// 32768 ctx (the largest, qwen3-8b, needs roughly 8GiB total there).
    fn generous_gpu_probe() -> HardwareProbe {
        HardwareProbe {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            nvidia_gpus: vec![hardware::GpuInfo {
                name: "Big GPU".to_string(),
                vram_bytes: 64 * memory::GIB,
                compute_cap: Some((8, 9)),
            }],
            has_physical_nvidia: true,
            has_usable_nvidia: true,
            driver_cuda_version: Some((12, 4)),
            cuda_runtime_lines: vec![12],
            vulkan_available: false,
            ram_bytes: 128 * memory::GIB,
            macos_version: None,
        }
    }

    // ------------------------------------------------------------------
    // local.plan
    // ------------------------------------------------------------------

    #[test]
    fn plan_with_computes_windows_cuda13_older_engine_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let probe = windows_cuda13_older_probe();
        let v = plan_with(
            "qwen3-4b-instruct-2507",
            "Q4_K_M",
            BackendPref::Cuda,
            &probe,
            dir.path(),
        )
        .expect("plan should resolve");
        assert_eq!(v["backend"], "cuda");
        assert_eq!(v["engine_bytes"], 183728712u64 + 390970417u64);
        assert_eq!(v["model_bytes"], 2_497_281_120u64);
        // The fallback chain for an explicit CUDA preference is [cuda, cpu]
        // -- cpu is the one alternative.
        assert_eq!(v["alternatives"].as_array().unwrap().len(), 1);
        assert_eq!(v["alternatives"][0]["backend"], "cpu");
        assert!(!v["sources"].as_array().unwrap().is_empty());
    }

    #[test]
    fn plan_with_rejects_an_unknown_model() {
        let dir = tempfile::tempdir().unwrap();
        let probe = windows_cuda13_older_probe();
        let err = plan_with(
            "no-such-model",
            "Q4_K_M",
            BackendPref::Auto,
            &probe,
            dir.path(),
        )
        .unwrap_err();
        assert_eq!(err.0, "unknown_model");
    }

    #[test]
    fn plan_with_rejects_an_unknown_quant() {
        let dir = tempfile::tempdir().unwrap();
        let probe = windows_cuda13_older_probe();
        let err = plan_with(
            "qwen3-4b-instruct-2507",
            "Q8_0",
            BackendPref::Auto,
            &probe,
            dir.path(),
        )
        .unwrap_err();
        assert_eq!(err.0, "unknown_quant");
    }

    // ------------------------------------------------------------------
    // local.status / local.models -- status_with / models_with
    // ------------------------------------------------------------------

    /// `status_with` touches the real global LocalOnly latch (via
    /// `latch::is_on()`, with no injection point of its own), so this holds
    /// `latch::test_serial_async` like every other real-global-touching test
    /// in this module.
    #[tokio::test]
    async fn status_with_reports_no_engine_and_a_missing_model_when_nothing_is_installed() {
        let _guard = latch::test_serial_async().await;
        latch::set_global_for_test(false);

        let dir = tempfile::tempdir().unwrap();
        let engine_root = dir.path().join("engine");
        let supervisor = EngineSupervisor::new(engine_root.clone());
        let cfg = LocalConfig::default();

        let v = status_with(
            &cfg,
            &supervisor,
            Some(&cpu_only_probe()),
            &engine_root,
            None,
        );
        assert_eq!(v["state"]["state"], "idle");
        assert_eq!(v["latched"], false);
        // No install directory has ever been created -- `engine` really is
        // absent.
        assert_eq!(v["engine"], serde_json::Value::Null);
        // The CONFIGURED model/quant still resolves in the catalog even
        // with no `models_dir` to check on disk -- `model` reports it as
        // "missing", not `null` (that only happens for an unknown
        // model/quant id in config, exercised separately below).
        // Against `cfg.model`, not a hardcoded id: what this test is about is
        // that status reports the CONFIGURED model and resolves it, whichever
        // one that is. The default's identity is pinned once, in
        // `local::config`'s `defaults_match_design`; repeating it here only
        // made this test fail when the default moved to 8B (R40), which is a
        // deliberate product change and not a regression in `status_with`.
        assert_eq!(v["model"]["id"], cfg.model);
        assert_eq!(v["model"]["state"], "missing");
        assert_eq!(v["model"]["have"], 0);

        latch::set_global_for_test(false);
    }

    #[test]
    fn status_with_reports_a_null_model_for_an_unknown_configured_model() {
        let dir = tempfile::tempdir().unwrap();
        let engine_root = dir.path().join("engine");
        let supervisor = EngineSupervisor::new(engine_root.clone());
        let cfg = LocalConfig {
            model: "no-such-model".to_string(),
            ..LocalConfig::default()
        };

        let v = status_with(
            &cfg,
            &supervisor,
            Some(&cpu_only_probe()),
            &engine_root,
            None,
        );
        assert_eq!(v["model"], serde_json::Value::Null);
    }

    /// Writes a real `marker.json` (no other files -- `marker::read_marker`
    /// never checks the manifest against the filesystem, only
    /// `install::integrity::verify_manifest` does, and that is
    /// `EngineSupervisor::cold_start`'s job, not `status_with`'s) into the
    /// exact directory `install::install_dir` would use for a CPU install,
    /// so `installed_engine`'s walk finds it the same way it would in
    /// production.
    #[test]
    fn status_with_reports_the_installed_engine_and_a_partial_model() {
        let dir = tempfile::tempdir().unwrap();
        let engine_root = dir.path().join("engine");
        let probe = cpu_only_probe();
        let cfg = LocalConfig {
            backend: BackendPref::Cpu,
            ..LocalConfig::default()
        };

        let chain = hardware::fallback_chain(&probe, cfg.backend);
        let kind = chain.first().expect("cpu chain is never empty").clone();
        let install_dir = install::install_dir(&engine_root, ENGINE_PINNED.tag, &kind);
        std::fs::create_dir_all(&install_dir).unwrap();
        marker::write_marker(
            &install_dir,
            &marker::Marker {
                tag: ENGINE_PINNED.tag.to_string(),
                kind: kind_label(&kind),
                archive_sha256: vec![],
                files: vec![marker::FileEntry {
                    path: "llama-server".to_string(),
                    size: 12345,
                    sha256: "a".repeat(64),
                }],
                installed_at: 0,
                last_used_at: 0,
                bad: None,
            },
        )
        .unwrap();

        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        let model = catalog::model(&cfg.model).unwrap();
        let quant = catalog::quant(model, &cfg.quant).unwrap();
        std::fs::write(
            models_dir.join(format!("{}.part", quant.file)),
            vec![0u8; 4096],
        )
        .unwrap();

        let supervisor = EngineSupervisor::new(engine_root.clone());
        let v = status_with(
            &cfg,
            &supervisor,
            Some(&probe),
            &engine_root,
            Some(&models_dir),
        );

        assert_eq!(v["engine"]["tag"], ENGINE_PINNED.tag);
        assert_eq!(v["engine"]["kind"], kind_label(&kind));
        assert_eq!(v["engine"]["bytes"], 12345);
        assert_eq!(v["engine"]["update"], "none");

        assert_eq!(v["model"]["id"], model.id);
        assert_eq!(v["model"]["state"], "partial");
        assert_eq!(v["model"]["have"], 4096);
        assert_eq!(v["model"]["bytes"], quant.bytes);
    }

    #[test]
    fn models_with_reports_missing_quants_without_a_models_dir() {
        let v = models_with(None, None);
        let list = v.as_array().unwrap();
        assert_eq!(list.len(), catalog::MODELS.len());
        for entry in list {
            for q in entry["quants"].as_array().unwrap() {
                assert_eq!(q["state"], "missing", "{entry}");
                assert_eq!(q["have"], 0, "{entry}");
            }
        }
    }

    /// Regression test for fix round 1, Important 2: `fit` and `estimate`
    /// must describe the SAME context size. On generous hardware every
    /// catalog model fits fully on the GPU at the preferred 32K ctx.
    #[test]
    fn models_with_shares_one_ctx_between_fit_and_estimate_when_full_gpu() {
        let probe = generous_gpu_probe();
        let v = models_with(Some(&probe), None);
        for entry in v.as_array().unwrap() {
            assert_eq!(entry["fit"]["fit"], "full_gpu", "{entry}");
            assert_eq!(entry["fit"]["ctx"], 32768, "{entry}");
            assert_eq!(entry["estimate"]["ctx"], 32768, "{entry}");
        }
    }

    /// Same regression, at the other end: no probe at all means nothing
    /// fits even CPU-only (`vram=None`, `ram=0`), and `estimate` must then
    /// use `CTX_FLOOR` -- the ctx whose non-fit produced the verdict --
    /// rather than the un-implemented fixed `CTX_PREFERRED` this replaced.
    #[test]
    fn models_with_estimates_at_ctx_floor_when_insufficient() {
        let v = models_with(None, None);
        for entry in v.as_array().unwrap() {
            assert_eq!(entry["fit"]["fit"], "insufficient", "{entry}");
            assert_eq!(entry["estimate"]["ctx"], CTX_FLOOR, "{entry}");
        }
    }

    // ------------------------------------------------------------------
    // local.set_default
    // ------------------------------------------------------------------

    /// Both directions exercise `crate::local::on_config_changed`, which
    /// always writes the REAL global LocalOnly latch
    /// (`latch::refresh_from_config` bypasses the thread-local test
    /// override on purpose -- see `latch`'s module docs) -- so this holds
    /// `latch::test_serial_async` for its whole body, exactly like
    /// `crate::local::sync`'s own `on_config_changed_with_...` tests do.
    ///
    /// Deliberately does NOT pre-seed `llm.local.enabled = true` (fix round
    /// 1, Important 1's regression test): `on:true` must set it itself, or
    /// this reports `latched: true`/success while the latch actually stays
    /// off -- exactly the bug this fix closes. Also asserts `enabled` is
    /// `true` afterward, not just that the latch flipped.
    #[tokio::test]
    async fn set_default_with_flips_and_releases_the_latch() {
        let _guard = latch::test_serial_async().await;
        latch::set_global_for_test(false);

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut seed = AgentConfig::default();
        // `on:false` now requires the release provider to be configured
        // (Minor 10) -- give "anthropic" a key so the second call below
        // exercises the latch release, not that separate refusal.
        seed.llm.anthropic.api_key = Some("test-key".to_string());
        seed.save_to_path(&config_path).unwrap();

        let shared_config = shared(seed);

        let v = set_default_with(
            &serde_json::json!({"request_id": "r1", "on": true}),
            &shared_config,
            &config_path,
            None,
        )
        .await;
        assert!(ok(&v), "{v}");
        assert_eq!(payload(&v)["data"]["latched"], true);
        assert!(latch::is_on());
        let after_on = AgentConfig::load_from_path(&config_path).unwrap();
        assert_eq!(after_on.llm.provider, Some("local".to_string()));
        assert!(after_on.llm.local.enabled, "on:true must enable local");

        let v2 = set_default_with(
            &serde_json::json!({"request_id": "r2", "on": false, "provider": "anthropic"}),
            &shared_config,
            &config_path,
            None,
        )
        .await;
        assert!(ok(&v2), "{v2}");
        assert_eq!(payload(&v2)["data"]["latched"], false);
        assert!(!latch::is_on());
        assert_eq!(
            AgentConfig::load_from_path(&config_path)
                .unwrap()
                .llm
                .provider,
            Some("anthropic".to_string())
        );

        latch::set_global_for_test(false);
    }

    #[tokio::test]
    async fn set_default_with_on_false_rejects_an_unconfigured_provider() {
        // "openai" resolves fine but has no api_key in a default config --
        // releasing onto it would leave the user with no working provider.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());

        let v = set_default_with(
            &serde_json::json!({"request_id": "r", "on": false, "provider": "openai"}),
            &shared_config,
            &config_path,
            None,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "provider_not_configured");
    }

    #[tokio::test]
    async fn set_default_with_on_false_requires_a_provider() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());

        let v = set_default_with(
            &serde_json::json!({"request_id": "r", "on": false}),
            &shared_config,
            &config_path,
            None,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "missing_provider");
    }

    #[tokio::test]
    async fn set_default_with_rejects_an_unknown_provider() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());

        let v = set_default_with(
            &serde_json::json!({"request_id": "r", "on": false, "provider": "not-a-provider"}),
            &shared_config,
            &config_path,
            None,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "unknown_provider");
    }

    #[tokio::test]
    async fn set_default_with_rejects_local_as_the_release_provider() {
        // on=false naming "local" itself would contradict the whole point
        // of releasing the latch.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());

        let v = set_default_with(
            &serde_json::json!({"request_id": "r", "on": false, "provider": "local"}),
            &shared_config,
            &config_path,
            None,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "unknown_provider");
    }

    // ------------------------------------------------------------------
    // local.set_config
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn set_config_with_rejects_ctx_below_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());
        let supervisor = EngineSupervisor::new(dir.path().join("engine"));

        let v = set_config_with(
            &serde_json::json!({"request_id": "r", "ctx_size": 8192}),
            &shared_config,
            &config_path,
            &supervisor,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "bad_ctx");
    }

    #[tokio::test]
    async fn set_config_with_accepts_a_valid_ctx_and_persists_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());
        let supervisor = EngineSupervisor::new(dir.path().join("engine"));

        let v = set_config_with(
            &serde_json::json!({"request_id": "r", "ctx_size": 32768, "parallel": 4}),
            &shared_config,
            &config_path,
            &supervisor,
        )
        .await;
        assert!(ok(&v), "{v}");
        assert_eq!(payload(&v)["data"]["ctx_size"], 32768);
        assert_eq!(payload(&v)["data"]["parallel"], 4);

        let persisted = AgentConfig::load_from_path(&config_path).unwrap();
        assert_eq!(persisted.llm.local.ctx_size, CtxPref::Fixed(32768));
        assert_eq!(persisted.llm.local.parallel, 4);
    }

    #[tokio::test]
    async fn set_config_with_rejects_an_unknown_backend() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());
        let supervisor = EngineSupervisor::new(dir.path().join("engine"));

        let v = set_config_with(
            &serde_json::json!({"request_id": "r", "backend": "quantum"}),
            &shared_config,
            &config_path,
            &supervisor,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "bad_backend");
    }

    /// Regression test for fix round 1, Important 3: `parallel` used to
    /// silently accept anything `as_u64()` couldn't parse (doing nothing)
    /// and truncate anything above `u32::MAX`; it must now error like
    /// `ctx_size`/`backend` do.
    #[tokio::test]
    async fn set_config_with_rejects_a_non_positive_or_oversized_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());
        let supervisor = EngineSupervisor::new(dir.path().join("engine"));

        for bad in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("two"),
            serde_json::json!(4.5),
            serde_json::json!(u64::from(u32::MAX) + 1),
        ] {
            let v = set_config_with(
                &serde_json::json!({"request_id": "r", "parallel": bad}),
                &shared_config,
                &config_path,
                &supervisor,
            )
            .await;
            assert!(!ok(&v), "{bad} -> {v}");
            assert_eq!(payload(&v)["error"]["code"], "bad_parallel", "{bad}");
        }
    }

    #[tokio::test]
    async fn set_config_with_rejects_a_non_integer_idle_unload_secs() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());
        let supervisor = EngineSupervisor::new(dir.path().join("engine"));

        let v = set_config_with(
            &serde_json::json!({"request_id": "r", "idle_unload_secs": -5}),
            &shared_config,
            &config_path,
            &supervisor,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "bad_idle_unload_secs");
    }

    #[tokio::test]
    async fn set_config_with_accepts_idle_unload_secs_alone() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let shared_config = shared(AgentConfig::default());
        let supervisor = EngineSupervisor::new(dir.path().join("engine"));

        let v = set_config_with(
            &serde_json::json!({"request_id": "r", "idle_unload_secs": 60}),
            &shared_config,
            &config_path,
            &supervisor,
        )
        .await;
        assert!(ok(&v), "{v}");
        assert_eq!(payload(&v)["data"]["idle_unload_secs"], 60);
    }

    /// Direct test of the pure comparison behind fix round 1, Important 5
    /// (`set_config` must not unconditionally stop the engine): only
    /// `ctx_size`/`backend`/`parallel` count as launch-relevant;
    /// `idle_unload_secs` alone must not.
    #[test]
    fn launch_relevant_changed_ignores_idle_unload_secs_alone() {
        let before = LocalConfig::default();

        let mut idle_only = before.clone();
        idle_only.idle_unload_secs += 60;
        assert!(!launch_relevant_changed(&before, &idle_only));

        let mut parallel_changed = before.clone();
        parallel_changed.parallel += 1;
        assert!(launch_relevant_changed(&before, &parallel_changed));

        let mut ctx_changed = before.clone();
        ctx_changed.ctx_size = CtxPref::Fixed(32768);
        assert!(launch_relevant_changed(&before, &ctx_changed));

        let mut backend_changed = before.clone();
        backend_changed.backend = BackendPref::Cuda;
        assert!(launch_relevant_changed(&before, &backend_changed));

        assert!(!launch_relevant_changed(&before, &before.clone()));
    }

    // ------------------------------------------------------------------
    // Validation-only paths for the remaining commands -- these return
    // before touching the EventBus/engine singleton/real config path, so
    // they're safe to run as ordinary unit tests.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn handle_install_rejects_an_unknown_model() {
        let shared_config = shared(AgentConfig::default());
        let v = handle_install(
            &serde_json::json!({"request_id": "r", "model": "nope", "quant": "Q4_K_M", "backend": "cpu"}),
            &shared_config,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "unknown_model");
    }

    #[tokio::test]
    async fn handle_install_rejects_an_unknown_quant() {
        let shared_config = shared(AgentConfig::default());
        let v = handle_install(
            &serde_json::json!({"request_id": "r", "model": "qwen3-4b-instruct-2507", "quant": "Q8_0", "backend": "cpu"}),
            &shared_config,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "unknown_quant");
    }

    #[tokio::test]
    async fn handle_install_rejects_an_unknown_backend() {
        let shared_config = shared(AgentConfig::default());
        let v = handle_install(
            &serde_json::json!({"request_id": "r", "model": "qwen3-4b-instruct-2507", "quant": "Q4_K_M", "backend": "quantum"}),
            &shared_config,
        )
        .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "unknown_backend");
    }

    #[tokio::test]
    async fn handle_retry_backend_rejects_an_unknown_backend() {
        let v = handle_retry_backend(&serde_json::json!({"request_id": "r", "backend": "quantum"}))
            .await;
        assert!(!ok(&v), "{v}");
        assert_eq!(payload(&v)["error"]["code"], "unknown_backend");
    }

    #[tokio::test]
    async fn handle_cancel_reports_false_when_nothing_is_running() {
        // No other test in this module leaves an install running, so
        // `CURRENT_INSTALL` is guaranteed empty here.
        let v = handle_cancel(&serde_json::json!({"request_id": "r"})).await;
        assert!(ok(&v), "{v}");
        assert_eq!(payload(&v)["data"]["cancelled"], false);
    }

    #[test]
    fn kind_label_matches_the_marker_kind_convention() {
        let kind = InstallKind {
            platform: "windows-x64".to_string(),
            backend: Backend::Cuda,
            variant: Some("cuda13-older".to_string()),
            cudart: Some("13.3".to_string()),
        };
        assert_eq!(kind_label(&kind), "windows-x64-cuda-cuda13-older");
    }
}
