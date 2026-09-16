//! The supervised on-device inference engine process.
//!
//! Everything Tasks 2.1-2.8 built is a piece of one launch: `hardware`
//! decides which [`InstallKind`]s to try, `install`/`marker` say which of
//! them are actually on disk and trustworthy, `integrity` re-verifies one
//! before it is trusted again, `memory` picks the context size that fits,
//! `harden` decides the exact argv/env the process may run with, `guard`
//! keeps it from outliving the daemon on Unix, and `admission` meters what
//! may be sent to it once it is up. This module is the state machine that
//! runs that sequence, owns the resulting process, and publishes the
//! endpoint [`crate::wasm::local_llm`] talks to.
//!
//! ## Cold start
//!
//! [`EngineSupervisor::ensure_started`] is demand-driven: nothing here runs
//! at daemon boot ([`init`] only registers the hook and the idle timer), and
//! the first local chat request is what actually launches an engine. In
//! order:
//!
//! 1. Resolve config + model + fit (`catalog`/`memory`).
//! 2. Probe hardware and walk `hardware::fallback_chain`, keeping only kinds
//!    that have an install on disk, under a tag this build supports, that is
//!    not marked bad. Nothing left → [`LocalError::EngineUpdateRequired`].
//! 3. [`LocalState::VerifyingEngine`]: `integrity::verify_manifest` re-hashes
//!    the whole install directory against its marker.
//! 4. Take the machine-wide `engine.lock`, reap a stale `engine.pid`.
//! 5. [`LocalState::EngineStarting`]: pick a free port, build the launch with
//!    [`harden::server_spec`], spawn it through
//!    [`harden::SpawnSpec::to_command`], and poll `/health`.
//! 6. [`self_check`] (fail-closed), then a `max_tokens: 1` first-generation
//!    probe.
//! 7. Publish the endpoint, hand `admission` the real token budget, and
//!    settle in [`LocalState::Ready`].
//!
//! This module owns [`LocalState::VerifyingEngine`]. v3 §7.3 step 2 lists
//! `llama-server --version` as the install-time "does this machine start it
//! at all" check and §9 places `VerifyingEngine` between `InstallingEngine`
//! and `DownloadingModel`; step 3 above sits in the same place and steps 5-6
//! are strictly stronger than a `--version` call, so a bare `--version`
//! invocation is deliberately not implemented.
//!
//! ## Rollback (§8.2)
//!
//! Two of the three failures that mean "this build is broken" — a failed
//! [`self_check`] and a failed first generation — write a [`BadMark`] onto
//! the install and hand the walk the next candidate, which for the same kind
//! is the compatible tag sitting behind the pinned one. That is the whole
//! point of §16.1's self-check: a new pinned tag that renames a hardening
//! flag is *intercepted and rolled back*, not left to brick local inference
//! on every boot. A cold-start integrity failure is deliberately NOT a
//! rollback trigger — §16.3 routes a damaged install to the repair card
//! instead, since re-downloading is the fix and silently running an older
//! build would hide it.
//!
//! ## Two locks, and what each protects
//!
//! `Shared::state` is a [`std::sync::Mutex`] holding everything observable
//! (the state machine, the running process, the crash log). It is **never**
//! held across an `.await`: every path that needs to await something takes
//! what it needs out of the mutex first (see [`EngineSupervisor::take_running`]).
//! `Shared::start_lock` is a `tokio::sync::Mutex` held for the whole cold
//! start, so two concurrent first requests produce one engine rather than
//! two racing launches over the same port and lock file.
//!
//! `engine.lock` lives in the shared engine cache root
//! (`$CACHE/nevoflux/engine/` — v3 §6, **not** the daemon's own data
//! directory) and is the cross-*process* lock: two daemons on one machine
//! must not both drive an engine, so the second gets [`LocalError::Busy`]
//! rather than a second multi-GB model load. The cache root is what makes
//! that machine-wide — two daemons configured with different
//! `NEVOFLUX_DATA_DIR`s, a second channel or profile, still contend for the
//! same lock. `engine.pid` sits beside it and records the pid, process
//! creation time and executable path of what we spawned, so a daemon killed
//! without running [`EngineSupervisor::stop`] can reap its own orphan on the
//! next cold start and can never mistake a recycled pid for it.
//!
//! ## Orphan prevention
//!
//! On Unix the engine is not spawned directly: it is spawned as
//! `nevoflux-agent --engine-guard -- <program> <args…>`
//! ([`harden::guard_spec`], [`crate::local::guard`]), with its stdin held
//! open by this supervisor. The guard parents the engine in its own process
//! group and blocks on that pipe; when this daemon dies by ANY means —
//! SIGKILL, a panic, or the `force_exit` its shutdown path ends in — the
//! pipe closes and the guard SIGTERMs the engine's group, waits 3s, then
//! SIGKILLs it (v3 §16.2 / D10). Windows gets the same guarantee from the
//! daemon's kill-on-close Job Object instead
//! (`assign_self_to_kill_on_close_job` in `src/main.rs`), so there the
//! engine is spawned directly.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use crate::local::config::{LocalConfig, CTX_FLOOR};
use crate::local::endpoint::{self, LocalEndpoint};
use crate::local::harden::{self, ServerParams};
use crate::local::hardware::{self, Backend, HardwareProbe, InstallKind};
use crate::local::integrity::{self, IntegrityError};
use crate::local::marker::{self, BadMark, Marker};
use crate::local::memory::Fit;
use crate::local::state::{publish_state, LocalError, LocalState};
use crate::local::{catalog, install, memory, release};

/// The process-wide admission controller, re-exported (ruling R66).
///
/// It is **defined** in [`crate::local::admission`] — Task 2.7 owns it,
/// because a process-wide admission singleton is admission's concern and
/// that task runs before this one, so it cannot wait for this module to
/// exist. This re-export only means callers that think of admission as part
/// of the engine's published surface can reach it from here; there is
/// exactly one instance.
pub use crate::local::admission::admission;

/// How long a cold start waits for `/health` to come up. A multi-GB model
/// load from cold page cache onto a GPU is the slow case this covers.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(120);

/// Gap between `/health` polls during a cold start.
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long [`EngineSupervisor::stop`] waits for a graceful exit before
/// escalating.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// How often the idle-unload timer looks at the engine.
const IDLE_TICK: Duration = Duration::from_secs(30);

/// How many different ports one cold start will try before giving up.
/// [`harden::pick_free_port`] is explicitly not a reservation, so losing a
/// port between picking it and binding it is an expected race, not a
/// failure.
const MAX_PORT_ATTEMPTS: usize = 3;

/// Crashes within this many seconds of each other count toward
/// [`crash_policy_should_downgrade`].
const CRASH_WINDOW_SECS: i64 = 600;

/// How many crashes inside [`CRASH_WINDOW_SECS`] mean the current backend is
/// not merely unlucky.
const CRASH_DOWNGRADE_THRESHOLD: usize = 2;

/// How much of the engine's stderr is kept for a failure message.
const STDERR_TAIL_LINES: usize = 40;

/// How much of a self-check response body is read before giving up on it.
///
/// The bodies this module reads are all supposed to be small (a JSON error,
/// a `/props` document). A `--static-path` serving something large, or an
/// engine that has gone strange, must not be able to make the security
/// self-check allocate an arbitrary amount of memory, so every read is
/// capped rather than trusting `Content-Length` or the peer.
const MAX_PROBE_BODY_BYTES: usize = 64 * 1024;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The engine's own `<backend>` slug as it appears inside a marker's `kind`
/// string (`crate::local::install::kind_suffix` builds those, but its
/// `backend_slug` is private to that module). Used only by
/// [`clear_bad_marks_for_backend`] to find the install directories a backend
/// retry should un-mark.
fn backend_slug(backend: Backend) -> &'static str {
    match backend {
        Backend::Cpu => "cpu",
        Backend::Vulkan => "vulkan",
        Backend::Cuda => "cuda",
        Backend::Metal => "metal",
    }
}

/// The backend `llama-server` reports having loaded on one stderr line, e.g.
/// `load_backend: loaded CUDA backend from …/ggml-cuda.dll`.
///
/// `None` for any other line, and for backends this daemon does not model as
/// a [`Backend`] (`RPC`, `BLAS`, …) — a line naming one of those says
/// nothing about which compute backend the model actually runs on.
pub fn parse_loaded_backends(log_line: &str) -> Option<Backend> {
    let rest = log_line.split("load_backend: loaded ").nth(1)?;
    let name = rest.split(" backend").next()?.trim();
    match name.to_ascii_uppercase().as_str() {
        "CUDA" => Some(Backend::Cuda),
        "VULKAN" => Some(Backend::Vulkan),
        "METAL" => Some(Backend::Metal),
        "CPU" => Some(Backend::Cpu),
        _ => None,
    }
}

/// Whether one `(tag, backend)`'s crash log justifies dropping to the next
/// backend in the fallback chain: two or more crashes within
/// [`CRASH_WINDOW_SECS`].
///
/// One crash is bad luck (a driver hiccup, an OOM from something else on the
/// GPU) and gets a restart on the next demand. Two in ten minutes is the
/// backend itself, and continuing to relaunch it just makes the user wait
/// through another model load for the same failure.
pub fn crash_policy_should_downgrade(crashes: &[i64], now: i64) -> bool {
    crashes
        .iter()
        .filter(|t| now.saturating_sub(**t) <= CRASH_WINDOW_SECS)
        .count()
        >= CRASH_DOWNGRADE_THRESHOLD
}

/// How an [`IntegrityError`] from a cold-start re-verify reaches the user.
///
/// `Io` is deliberately NOT [`LocalError::EngineCorrupt`]: a permissions
/// error or a failing disk is not repaired by a reinstall, and may not even
/// let one run. Everything else — a missing file, a wrong size, a wrong
/// hash, or a file the manifest never recorded — does share the reinstall
/// path.
fn integrity_error_to_local(e: IntegrityError) -> LocalError {
    match e {
        IntegrityError::Io(detail) => LocalError::EngineUnreadable { detail },
        IntegrityError::Missing(p) => LocalError::EngineCorrupt {
            detail: format!("missing {p}"),
        },
        IntegrityError::SizeMismatch(p) => LocalError::EngineCorrupt {
            detail: format!("wrong size: {p}"),
        },
        IntegrityError::HashMismatch(p) => LocalError::EngineCorrupt {
            detail: format!("wrong contents: {p}"),
        },
        IntegrityError::Unexpected(p) => LocalError::EngineCorrupt {
            detail: format!("unexpected file in the install directory: {p}"),
        },
    }
}

/// Whether retrying is worth offering the user for `e` — drives
/// [`LocalState::Failed`]'s `retryable` flag.
fn retryable(e: &LocalError) -> bool {
    matches!(
        e,
        LocalError::EngineCrash { .. }
            | LocalError::BackendUnavailable
            | LocalError::Busy
            | LocalError::DownloadFailed { .. }
            | LocalError::Offline
    )
}

/// An HTTP client for talking to the engine: loopback only, never through a
/// proxy.
///
/// `no_proxy` for the same reason [`crate::wasm::local_llm`]'s own client
/// sets it — `reqwest` otherwise honours `HTTP_PROXY`/`ALL_PROXY` for every
/// request regardless of host, and routing a LocalOnly-latched engine's
/// traffic through a proxy would send it off the machine.
fn engine_http_client(timeout: Duration) -> Result<reqwest::Client, LocalError> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .build()
        .map_err(|e| LocalError::EngineCrash {
            detail: format!("http client: {e}"),
        })
}

/// Reads at most [`MAX_PROBE_BODY_BYTES`] of a response body.
///
/// Chunk-by-chunk rather than `Response::text()`, which would buffer
/// whatever the peer decides to send.
async fn read_capped(mut response: reqwest::Response) -> String {
    let mut out: Vec<u8> = Vec::new();
    while out.len() < MAX_PROBE_BODY_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_PROBE_BODY_BYTES - out.len();
                out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a body looks like an OpenAI-shaped model listing with at least
/// one entry — v3 §16.1's "非 2xx 且未返回模型列表": a refusal that still
/// enumerates the loaded model has not actually refused anything.
fn body_lists_models(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    ["data", "models"]
        .iter()
        .filter_map(|k| value.get(*k))
        .filter_map(|v| v.as_array())
        .any(|a| !a.is_empty())
}

/// The engine's security self-check, run once per launch before anything is
/// published. `base_url` is the server ROOT (`http://127.0.0.1:PORT`), not
/// the `/v1` endpoint URL. Returns the `n_ctx` `/props` reports.
///
/// **Fail-closed**: every failure is [`LocalError::EngineInsecure`],
/// including one where a check could not be completed at all (a request that
/// errored, a `/props` that does not report `n_ctx`). An engine whose
/// exposed surface cannot be confirmed is not an engine this daemon will put
/// a conversation through, and the alternative — treating "could not check"
/// as "probably fine" — is exactly the shape of hole [`harden`] exists to
/// close.
///
/// Every criterion is "did it answer", never an exact status code. The
/// pinned build answers `/slots` with **501** and unauthenticated
/// `/v1/models` with **401**, but a future build could pick 404 or 403 for
/// either without becoming less hardened, and a check pinned to one literal
/// would reject a perfectly good engine (or, worse, be "fixed" by loosening
/// it). See the tests, which deliberately feed it both shapes.
///
/// The four checks, in order:
/// 1. `/v1/models` **without** the bearer token must be refused, and must
///    not return a model list even while refusing.
/// 2. `/slots` (with the token) must not answer. It exposes other requests'
///    prompts and cached state; `--no-slots` is what closes it.
/// 3. `/` must not serve the bundled web UI. `--no-webui` closes it; an HTML
///    document here means it reopened.
/// 4. `/props` must answer with the context size, which is both the
///    confirmation that the launch flags took effect and the value the
///    caller checks against what it asked for.
pub async fn self_check(base_url: &str, api_key: &str) -> Result<u32, LocalError> {
    let root = base_url.trim_end_matches('/');
    let client = engine_http_client(Duration::from_secs(10))?;
    let insecure = |detail: String| LocalError::EngineInsecure { detail };

    // 1. Unauthenticated `/v1/models` must be refused, and must not leak the
    //    model list while refusing.
    let models = client
        .get(format!("{root}/v1/models"))
        .send()
        .await
        .map_err(|e| insecure(format!("unauthenticated GET /v1/models failed: {e}")))?;
    let models_status = models.status();
    let models_body = read_capped(models).await;
    if models_status.is_success() {
        return Err(insecure(format!(
            "unauthenticated GET /v1/models returned {models_status} — the engine is not \
             requiring its API key"
        )));
    }
    if body_lists_models(&models_body) {
        return Err(insecure(format!(
            "unauthenticated GET /v1/models returned {models_status} but still listed models"
        )));
    }

    // 2. `/slots` must not answer, even authenticated.
    let slots = client
        .get(format!("{root}/slots"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| insecure(format!("GET /slots failed: {e}")))?;
    if slots.status().is_success() {
        return Err(insecure(format!(
            "GET /slots returned {} — the slots endpoint is exposed",
            slots.status()
        )));
    }

    // 3. `/` must not be the bundled web UI.
    let root_page = client
        .get(format!("{root}/"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| insecure(format!("GET / failed: {e}")))?;
    if root_page.status().is_success() {
        let content_type = root_page
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = read_capped(root_page).await;
        let looks_like_html = content_type.contains("text/html") || {
            let head = body.trim_start().to_ascii_lowercase();
            head.starts_with("<!doctype html") || head.starts_with("<html")
        };
        if looks_like_html {
            return Err(insecure(
                "GET / served an HTML document — the bundled web UI is exposed".to_string(),
            ));
        }
    }

    // 4. `/props` must confirm the context size.
    let props = client
        .get(format!("{root}/props"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| insecure(format!("GET /props failed: {e}")))?;
    let props_status = props.status();
    let props_body = read_capped(props).await;
    if !props_status.is_success() {
        return Err(insecure(format!(
            "GET /props returned {props_status} — the launch could not be confirmed"
        )));
    }
    let body: serde_json::Value = serde_json::from_str(&props_body)
        .map_err(|e| insecure(format!("GET /props was not JSON: {e}")))?;
    n_ctx_from_props(&body)
        .ok_or_else(|| insecure(format!("GET /props did not report n_ctx: {body}")))
}

/// Pulls `n_ctx` out of a `/props` body. `llama-server` reports it under
/// `default_generation_settings`; a top-level `n_ctx` is accepted as well so
/// a future build that moves it does not fail the self-check for a shape
/// change.
fn n_ctx_from_props(body: &serde_json::Value) -> Option<u32> {
    body.get("default_generation_settings")
        .and_then(|v| v.get("n_ctx"))
        .or_else(|| body.get("n_ctx"))
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
}

/// One install this daemon could launch: a fallback-chain kind, the
/// directory it is installed in, and that directory's marker.
#[derive(Debug, Clone)]
struct Candidate {
    kind: InstallKind,
    dir: PathBuf,
    marker: Marker,
    /// Index of `kind` in the fallback chain — 0 is the most preferred, so
    /// anything above it means the launch is degraded.
    chain_index: usize,
}

/// The release tags this build knows how to run, most-preferred first: the
/// pinned tag, then every compatible one.
///
/// Passed *into* [`usable_candidates`] rather than read inside it — the same
/// seam Task 2.3 used to make `tag_status` exercisable. Without it the
/// rollback walk is untestable while `release::ENGINE_COMPATIBLE` is empty,
/// which is precisely the situation in which a rollback bug would ship
/// unnoticed and only surface when a shipped engine upgrade is already
/// broken.
fn supported_tags() -> Vec<&'static str> {
    std::iter::once(release::ENGINE_PINNED.tag)
        .chain(release::ENGINE_COMPATIBLE.iter().map(|r| r.tag))
        .collect()
}

/// Walks `chain` in preference order and returns every install that could be
/// launched, most-preferred first.
///
/// An entry is kept only if: the directory for `(tag, kind)` exists with a
/// marker `marker::read_marker` trusts, that marker carries no [`BadMark`],
/// and the kind is not in `session_bad` (this process already watched it
/// fail). `tags` IS the supported set — a tag this build has no archive
/// table for is simply never probed — so no separate `tag_status` check is
/// needed here.
///
/// Every supported tag is considered for each kind, in `tags` order, which
/// is what makes the §8.2 rollback possible without a second lookup: when
/// the newly pinned build fails its self-check or cannot generate, the
/// compatible one behind it is already the next entry in this list.
fn usable_candidates(
    root: &Path,
    chain: &[InstallKind],
    tags: &[&str],
    session_bad: &HashSet<InstallKind>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (chain_index, kind) in chain.iter().enumerate() {
        if session_bad.contains(kind) {
            tracing::debug!(?kind, "skipping install kind marked bad in this session");
            continue;
        }
        for tag in tags {
            let dir = install::install_dir(root, tag, kind);
            let Some(marker) = marker::read_marker(&dir) else {
                continue;
            };
            if let Some(bad) = &marker.bad {
                tracing::debug!(
                    dir = %dir.display(),
                    reason = %bad.reason,
                    by_build = %bad.by_build,
                    "skipping install marked bad on disk"
                );
                continue;
            }
            out.push(Candidate {
                kind: kind.clone(),
                dir,
                marker,
                chain_index,
            });
        }
    }
    out
}

/// Whether a launch failure should write a [`BadMark`] and roll back to an
/// older build, per §8.2.
///
/// Only for the newest tag this build knows — the first entry of
/// [`supported_tags`], i.e. the pinned one. Marking a *compatible* install
/// bad would be throwing away the fallback itself, and marking the live-test
/// override (which has no manifest behind it) bad would write a mark onto a
/// directory this daemon does not own.
fn should_roll_back(candidate: &Candidate, tags: &[&str]) -> bool {
    !candidate.marker.files.is_empty() && tags.first() == Some(&candidate.marker.tag.as_str())
}

/// What a launch attempt decided about the rest of the chain.
enum LaunchFailure {
    /// This install could not serve, but another one might — the caller
    /// moves to the next candidate and records the launch as degraded.
    TryNext(LocalError),
    /// Stop here and report. Nothing about the next candidate would be
    /// different: a bad config, no disk, a damaged install that needs the
    /// §16.3 repair card, or another daemon already holding the lock.
    Fatal(LocalError),
}

/// A recorded process identity — pid, creation time and executable path.
///
/// §16.2 requires all three before killing anything, so that a pid the OS
/// has since recycled cannot get an unrelated process terminated in the
/// engine's place. A pid alone is not evidence: on a busy machine it is
/// reused within hours, and this audience plausibly runs its own
/// `llama-server` too, which a name-only check would happily kill.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct EngineIdentity {
    pid: u32,
    /// Opaque, platform-specific process-creation token: the creation
    /// `FILETIME` on Windows, `/proc/<pid>/stat` field 22 on Linux, `ps -o
    /// lstart=` elsewhere. Never parsed — only compared against a token read
    /// back from the live process, on the same machine, by the same build,
    /// which is all "is this still the same process?" requires.
    start_token: String,
    /// The executable the process is running.
    exe_path: String,
}

/// Reads `pid`'s live identity, or `None` if the process is gone or cannot
/// be inspected. `None` is always the safe answer: every caller treats it as
/// "do not kill".
#[cfg(windows)]
fn live_identity(pid: u32) -> Option<EngineIdentity> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: `OpenProcess` returns either null or a handle this function
    // owns and closes on every exit path below. `GetProcessTimes` and
    // `QueryFullProcessImageNameW` are handed live, uniquely-borrowed locals
    // and a buffer whose length is passed alongside it.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        let times_ok =
            GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0;

        let mut buf = vec![0u16; 4096];
        let mut len = buf.len() as u32;
        let path_ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(handle);

        if !times_ok || !path_ok {
            return None;
        }
        Some(EngineIdentity {
            pid,
            start_token: format!("{}:{}", creation.dwHighDateTime, creation.dwLowDateTime),
            exe_path: String::from_utf16_lossy(&buf[..len as usize]),
        })
    }
}

#[cfg(unix)]
fn live_identity(pid: u32) -> Option<EngineIdentity> {
    let exe_path = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.display().to_string())
        .or_else(|| ps_field(pid, "comm="))?;
    let start_token = proc_start_token(pid).or_else(|| ps_field(pid, "lstart="))?;
    Some(EngineIdentity {
        pid,
        start_token,
        exe_path,
    })
}

/// Field 22 (`starttime`) of `/proc/<pid>/stat`, as an opaque string.
///
/// Counted from the LAST `)` rather than by splitting the whole line:
/// field 2 (`comm`) is parenthesised and may itself contain spaces and
/// `)`, so a naive `split_whitespace().nth(21)` reads the wrong field for
/// any process whose name contains one.
#[cfg(unix)]
fn proc_start_token(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    // After `)` the next field is 3 (`state`), so field 22 is offset 19.
    after_comm.split_whitespace().nth(19).map(str::to_string)
}

/// One `ps -o <field>=` value for `pid` — the macOS fallback for the two
/// facts Linux reads out of `/proc`.
#[cfg(unix)]
fn ps_field(pid: u32, field: &str) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", field])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Everything one running engine process owns, taken out of the state mutex
/// as a unit whenever it has to be awaited on.
struct Running {
    /// The process this daemon spawned: on Unix the `--engine-guard`
    /// watchdog (which in turn owns the engine's process group); on Windows
    /// the engine itself.
    child: tokio::process::Child,
    pid: u32,
    kind: InstallKind,
    /// The release tag of the install this engine is running, so the crash
    /// log can be keyed by `(tag, backend)` per §8.3.
    marker_tag: String,
    backend: Backend,
    dir: PathBuf,
    idle_unload_secs: u64,
    /// Held open for the process's whole life: closing it releases
    /// `engine.lock`.
    _lock: std::fs::File,
    /// Unix only. The guard blocks reading this pipe; dropping it is the EOF
    /// that starts §16.2's SIGTERM → 3s → SIGKILL, run by the guard, which
    /// is the only process that knows the engine's process group.
    #[cfg(unix)]
    guard_stdin: Option<tokio::process::ChildStdin>,
}

/// What the stderr forwarder learned from the engine's own logging.
#[derive(Default)]
struct StderrFindings {
    backends: Vec<Backend>,
    compute_buffer_lines: Vec<String>,
    tail: Vec<String>,
}

impl StderrFindings {
    fn push_line(&mut self, line: &str) {
        if let Some(backend) = parse_loaded_backends(line) {
            if !self.backends.contains(&backend) {
                self.backends.push(backend);
            }
        }
        if line.contains("compute buffer size") {
            self.compute_buffer_lines.push(line.to_string());
        }
        self.tail.push(line.to_string());
        if self.tail.len() > STDERR_TAIL_LINES {
            self.tail.remove(0);
        }
    }

    fn tail_text(&self) -> String {
        self.tail.join("\n")
    }

    /// The compute backend actually in use: the first non-CPU backend the
    /// engine loaded, else CPU if it said so, else nothing learned.
    fn effective_backend(&self) -> Option<Backend> {
        self.backends
            .iter()
            .find(|b| **b != Backend::Cpu)
            .or_else(|| self.backends.first())
            .copied()
    }
}

struct Inner {
    /// The shared engine cache root: installs, `engine.lock` and
    /// `engine.pid` all live here (v3 §6).
    engine_root: PathBuf,
    state: LocalState,
    running: Option<Running>,
    /// Crashes per `(tag, backend)`. §8.3 gives each its own two-strike
    /// budget, so a backend that has already been downgraded cannot spend
    /// its successor's budget for it.
    crashes: HashMap<(String, Backend), Vec<i64>>,
    /// Kinds this process has watched fail. Cleared for one backend by
    /// [`EngineSupervisor::retry_backend`].
    session_bad: HashSet<InstallKind>,
    /// The last time the idle timer saw interactive work, or the engine
    /// became ready.
    last_interactive_at: i64,
    idle_task_started: bool,
}

struct Shared {
    state: Mutex<Inner>,
    /// Held for the whole of one cold start, so concurrent first requests
    /// produce one engine instead of racing over the port and lock file.
    start_lock: tokio::sync::Mutex<()>,
}

/// Owns the on-device engine process: starts it on demand, watches it, takes
/// it down when it goes idle, and publishes what it learns.
///
/// Cheaply cloneable (an `Arc` handle over shared state) so background tasks
/// — the stderr forwarder, the idle timer — can hold one without borrowing
/// from the singleton's `&'static`, which is also what lets a test drive an
/// instance of its own instead of the process-global.
#[derive(Clone)]
pub struct EngineSupervisor {
    shared: Arc<Shared>,
}

/// Where installs, `engine.lock` and `engine.pid` all live: the shared
/// engine cache root (v3 §6).
///
/// Resolved through [`crate::local::install::engine_root`], so
/// `NEVOFLUX_LOCAL_CACHE_DIR` redirects the whole set together — and, more
/// importantly, so a test can hand a supervisor a temporary directory and be
/// structurally unable to touch the real user cache, rather than relying on
/// remembering to set an environment variable.
fn default_engine_root() -> PathBuf {
    install::engine_root().unwrap_or_else(|| PathBuf::from("."))
}

/// A launch directed at a specific engine directory and model file instead
/// of an installed one, for the `#[ignore]`d live test that drives a real
/// `llama-server` out of a staging directory.
///
/// Compiled only under `cfg(test)` — on purpose. The production cold start
/// has exactly one way to reach a binary (an install whose marker and
/// manifest both verify), and adding an environment variable that bypasses
/// that would be a real hole in a real build, for the sake of a test. The
/// live test still drives the whole of [`EngineSupervisor::ensure_started`];
/// only the "which directory" half of step 2 and the integrity re-verify in
/// step 3 are replaced, since a staging directory has no marker to verify
/// against.
#[cfg(test)]
fn live_override() -> Option<(PathBuf, PathBuf)> {
    let dir = std::env::var("NEVOFLUX_LOCAL_LIVE_ENGINE_DIR")
        .ok()
        .filter(|s| !s.is_empty())?;
    let model = std::env::var("NEVOFLUX_LOCAL_LIVE_MODEL")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some((PathBuf::from(dir), PathBuf::from(model)))
}

#[cfg(not(test))]
fn live_override() -> Option<(PathBuf, PathBuf)> {
    None
}

impl EngineSupervisor {
    /// A supervisor rooted at `engine_root` — the cache directory holding
    /// installs, `engine.lock` and `engine.pid`. [`supervisor`] builds the
    /// process-global one from [`default_engine_root`]; tests build their
    /// own against a temporary directory, which is what keeps them off the
    /// real user cache.
    pub(crate) fn new(engine_root: PathBuf) -> Self {
        EngineSupervisor {
            shared: Arc::new(Shared {
                state: Mutex::new(Inner {
                    engine_root,
                    state: LocalState::Idle,
                    running: None,
                    crashes: HashMap::new(),
                    session_bad: HashSet::new(),
                    last_interactive_at: now_unix(),
                    idle_task_started: false,
                }),
                start_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The current state of on-device inference.
    pub fn state(&self) -> LocalState {
        self.lock().state.clone()
    }

    /// Records `state` and publishes it on [`crate::local::state::TOPIC_STATE`].
    fn set_state(&self, state: LocalState) {
        publish_state(&state);
        self.lock().state = state;
    }

    fn engine_root(&self) -> PathBuf {
        self.lock().engine_root.clone()
    }

    /// The pid of the process this daemon spawned, if one is running.
    ///
    /// Test-only: the live test captures it up front so it can kill a
    /// survivor if an assertion fails partway through. Production code takes
    /// the whole [`Running`] out of the mutex instead (see
    /// [`Self::take_running`]) rather than acting on a bare pid.
    #[cfg(test)]
    pub(crate) fn running_pid(&self) -> Option<u32> {
        self.lock().running.as_ref().map(|r| r.pid)
    }

    /// Takes the running process out of the state mutex, so whoever got it
    /// can await on it without holding a `std::sync::Mutex` guard across an
    /// `.await`. Whoever loses this race (a deliberate `stop` and the
    /// stderr forwarder noticing the same exit, say) gets `None` and does
    /// nothing.
    fn take_running(&self) -> Option<Running> {
        self.lock().running.take()
    }

    /// Start the engine if it is not already running, and return the
    /// endpoint to talk to it.
    ///
    /// Concurrent callers collapse into one launch: the first takes
    /// `start_lock` and does the work, the rest wait and then find the
    /// endpoint already published.
    pub async fn ensure_started(&self) -> Result<LocalEndpoint, LocalError> {
        if let Some(ep) = endpoint::current() {
            return Ok(ep);
        }
        let _start = self.shared.start_lock.lock().await;
        // Re-checked under the start lock: while this call was waiting for
        // it, the caller that held it may have finished a launch.
        if let Some(ep) = endpoint::current() {
            return Ok(ep);
        }

        match self.cold_start().await {
            Ok(ep) => Ok(ep),
            Err(e) => {
                tracing::warn!(error = ?e, "on-device engine cold start failed");
                self.set_state(LocalState::Failed {
                    error: e.clone(),
                    retryable: retryable(&e),
                });
                Err(e)
            }
        }
    }

    /// Resolve everything a launch needs, then try each usable install in
    /// turn until one serves.
    async fn cold_start(&self) -> Result<LocalEndpoint, LocalError> {
        let config = match crate::config::AgentConfig::load() {
            Ok(c) => c.llm.local.clone(),
            Err(e) => {
                tracing::warn!(error = %e, "could not load config for the on-device engine; using defaults");
                LocalConfig::default()
            }
        };

        let model = catalog::model(&config.model).ok_or_else(|| LocalError::EngineCorrupt {
            detail: format!("unknown model id {:?}", config.model),
        })?;
        let quant =
            catalog::quant(model, &config.quant).ok_or_else(|| LocalError::EngineCorrupt {
                detail: format!("model {} has no {} quantization", model.id, config.quant),
            })?;

        let probe = match hardware::cached() {
            Some(p) => p,
            None => hardware::probe().await,
        };
        let chain = hardware::fallback_chain(&probe, config.backend);
        if chain.is_empty() {
            return Err(LocalError::BackendUnavailable);
        }

        let candidates = self.resolve_candidates(&chain)?;
        self.launch_candidates(&candidates, &config, model, quant, &probe)
            .await
    }

    /// The installs worth trying, in preference order — or the live-test
    /// override standing in for one.
    fn resolve_candidates(&self, chain: &[InstallKind]) -> Result<Vec<Candidate>, LocalError> {
        if let Some((dir, _model)) = live_override() {
            tracing::warn!(
                dir = %dir.display(),
                "using the live-test engine directory override; install resolution and \
                 integrity verification are skipped"
            );
            return Ok(vec![Candidate {
                kind: chain[0].clone(),
                dir,
                marker: Marker {
                    tag: release::ENGINE_PINNED.tag.to_string(),
                    kind: String::new(),
                    archive_sha256: Vec::new(),
                    files: Vec::new(),
                    installed_at: now_unix(),
                    last_used_at: now_unix(),
                    bad: None,
                },
                chain_index: 0,
            }]);
        }

        let root = self.engine_root();
        let session_bad = self.lock().session_bad.clone();
        let candidates = usable_candidates(&root, chain, &supported_tags(), &session_bad);
        if candidates.is_empty() {
            return Err(LocalError::EngineUpdateRequired);
        }
        Ok(candidates)
    }

    /// Try each candidate until one serves. A candidate that cannot load is
    /// marked bad for this session and the next one is tried, with the
    /// launch reported as degraded; a fatal failure stops the walk.
    async fn launch_candidates(
        &self,
        candidates: &[Candidate],
        config: &LocalConfig,
        model: &'static catalog::LlmModel,
        quant: &catalog::Quant,
        probe: &HardwareProbe,
    ) -> Result<LocalEndpoint, LocalError> {
        let mut last: Option<LocalError> = None;
        let mut degrade_reason: Option<String> = None;

        for (index, candidate) in candidates.iter().enumerate() {
            let degraded = candidate.chain_index > 0 || degrade_reason.is_some();
            match self
                .launch(
                    candidate,
                    config,
                    model,
                    quant,
                    probe,
                    degraded,
                    degrade_reason.clone(),
                )
                .await
            {
                Ok(ep) => return Ok(ep),
                Err(LaunchFailure::Fatal(e)) => return Err(e),
                Err(LaunchFailure::TryNext(e)) => {
                    tracing::warn!(
                        dir = %candidate.dir.display(),
                        error = ?e,
                        "on-device engine could not be launched from this install; trying the next"
                    );
                    self.lock().session_bad.insert(candidate.kind.clone());
                    // Carries the real reason — including a failed security
                    // self-check — into `Ready.reason`, so a rollback is
                    // visible rather than silent (§8.2 + §16.1).
                    degrade_reason = Some(format!("{} ({e})", candidate.marker.tag));
                    last = Some(e);
                    if index + 1 == candidates.len() {
                        break;
                    }
                }
            }
        }

        Err(last.unwrap_or(LocalError::BackendUnavailable))
    }

    /// One install, start to finish: verify it, lock the machine, spawn it,
    /// wait for it, check it, prove it can generate, and publish it.
    #[allow(clippy::too_many_arguments)]
    async fn launch(
        &self,
        candidate: &Candidate,
        config: &LocalConfig,
        model: &'static catalog::LlmModel,
        quant: &catalog::Quant,
        probe: &HardwareProbe,
        degraded: bool,
        degrade_reason: Option<String>,
    ) -> Result<LocalEndpoint, LaunchFailure> {
        // --- verify ------------------------------------------------------
        if !candidate.marker.files.is_empty() {
            self.set_state(LocalState::VerifyingEngine);
            let dir = candidate.dir.clone();
            let files = candidate.marker.files.clone();
            let verified =
                tokio::task::spawn_blocking(move || integrity::verify_manifest(&dir, &files))
                    .await
                    .map_err(|e| {
                        LaunchFailure::Fatal(LocalError::EngineUnreadable {
                            detail: format!("integrity check task failed: {e}"),
                        })
                    })?;
            if let Err(e) = verified {
                // Deliberately NOT a rollback trigger: §16.3 routes a
                // damaged install to the repair card, where re-downloading
                // is the fix. Quietly running an older build instead would
                // hide the damage.
                return Err(LaunchFailure::Fatal(integrity_error_to_local(e)));
            }
        }

        // --- fit ----------------------------------------------------------
        let vram = if candidate.kind.backend == Backend::Cpu {
            None
        } else {
            probe.nvidia_gpus.first().map(|g| g.vram_bytes)
        };
        let fit = memory::choose_ctx(
            model,
            quant,
            config.ctx_size,
            config.kv_cache_type,
            vram,
            probe.ram_bytes,
        );
        let (ctx, fit_gpu_layers) = match &fit {
            // -1, not the layer count: `harden::server_spec` turns -1 into
            // `-ngl 99`, its "offload as many as fit" spelling. Passing an
            // exact count would also work today, and would silently
            // under-offload if a model's real GGUF block count ever
            // disagreed with the catalog constant.
            Fit::FullGpu { ctx } => (*ctx, -1),
            Fit::PartialGpu { ctx, gpu_layers } => (*ctx, *gpu_layers as i32),
            Fit::CpuOnly { ctx } => (*ctx, 0),
            Fit::Insufficient { reason } => {
                return Err(LaunchFailure::Fatal(LocalError::ModelTooLargeForMemory {
                    reason: reason.clone(),
                }))
            }
        };
        // An explicit `gpu_layers` in config wins over the fit's own count;
        // -1 (the default) means "let the fit decide".
        let gpu_layers = if config.gpu_layers >= 0 {
            config.gpu_layers
        } else {
            fit_gpu_layers
        };

        // --- model file ----------------------------------------------------
        let model_path = match live_override() {
            Some((_dir, model_path)) => model_path,
            None => crate::models::models_dir()
                .map(|d| d.join(quant.file))
                .ok_or_else(|| {
                    LaunchFailure::Fatal(LocalError::EngineUnreadable {
                        detail: "no cache directory for downloaded models".to_string(),
                    })
                })?,
        };
        if !model_path.is_file() {
            // The only "a file this launch needs is not there" shape the
            // state model has. The download itself belongs to the consent
            // flow, which runs before anything calls this.
            return Err(LaunchFailure::Fatal(LocalError::SentinelMissing {
                missing: model_path.display().to_string(),
            }));
        }

        // --- the machine-wide lock, and any orphan it protects -------------
        // Both files live in the shared engine cache root (§6), NOT the
        // daemon's data directory: that is what makes the exclusion
        // machine-wide across daemons with different data directories.
        let root = self.engine_root();
        if let Err(e) = std::fs::create_dir_all(&root) {
            return Err(LaunchFailure::Fatal(LocalError::EngineUnreadable {
                detail: format!("{}: {e}", root.display()),
            }));
        }
        let lock = acquire_engine_lock(&root).map_err(LaunchFailure::Fatal)?;
        reap_stale_engine(&root);

        // --- spawn ----------------------------------------------------------
        self.set_state(LocalState::EngineStarting);
        let api_key = harden::generate_api_key();
        let mut last_spawn_error: Option<LocalError> = None;

        for attempt in 1..=MAX_PORT_ATTEMPTS {
            let port = match harden::pick_free_port() {
                Ok(p) => p,
                Err(e) => {
                    last_spawn_error = Some(LocalError::EngineCrash {
                        detail: format!("could not reserve a loopback port: {e}"),
                    });
                    continue;
                }
            };
            let params = ServerParams {
                engine_dir: &candidate.dir,
                model_path: &model_path,
                // Ruling R60: the platform string the install itself was
                // selected for, never `cfg!(target_os)`.
                platform: &candidate.kind.platform,
                port,
                api_key: &api_key,
                ctx,
                parallel: config.parallel,
                kv: config.kv_cache_type,
                gpu_layers,
                // The quantized V cache the default `kv_cache_type` asks for
                // needs flash attention; leaving it off would make a Q8_0
                // launch fail outright.
                flash_attn: true,
            };
            let spec = harden::server_spec(&params);
            tracing::info!(
                program = %spec.program().display(),
                args = ?spec.args(),
                port,
                ctx,
                gpu_layers,
                "launching the on-device engine"
            );

            match self.spawn_and_wait_for_health(&spec, port, &api_key).await {
                Ok(started) => {
                    return self
                        .finish_launch(
                            started,
                            candidate,
                            config,
                            model,
                            ctx,
                            gpu_layers,
                            &api_key,
                            degraded,
                            degrade_reason,
                            lock,
                        )
                        .await;
                }
                Err(SpawnFailure::PortInUse(detail)) if attempt < MAX_PORT_ATTEMPTS => {
                    tracing::warn!(port, %detail, "engine could not bind its port; retrying on another");
                    last_spawn_error = Some(LocalError::EngineCrash { detail });
                }
                Err(SpawnFailure::PortInUse(detail)) => {
                    last_spawn_error = Some(LocalError::EngineCrash { detail });
                }
                Err(SpawnFailure::Failed(e)) => return Err(LaunchFailure::TryNext(e)),
                Err(SpawnFailure::Fatal(e)) => return Err(LaunchFailure::Fatal(e)),
            }
        }

        Err(LaunchFailure::TryNext(last_spawn_error.unwrap_or(
            LocalError::EngineCrash {
                detail: "engine did not start".to_string(),
            },
        )))
    }

    /// Self-check, first generation, publish. Split out of [`Self::launch`]
    /// so the port-retry loop above stays readable.
    #[allow(clippy::too_many_arguments)]
    async fn finish_launch(
        &self,
        started: Started,
        candidate: &Candidate,
        config: &LocalConfig,
        model: &'static catalog::LlmModel,
        ctx: u32,
        gpu_layers: i32,
        api_key: &str,
        degraded: bool,
        degrade_reason: Option<String>,
        lock: std::fs::File,
    ) -> Result<LocalEndpoint, LaunchFailure> {
        let Started {
            mut child,
            pid,
            root_url,
            findings,
            #[cfg(unix)]
            guard_stdin,
        } = started;

        // A failure from here on must not leave the process running: it was
        // spawned, it answered /health, and nothing else will reap it.
        macro_rules! abandon {
            ($failure:expr) => {{
                terminate(&mut child, pid).await;
                clear_engine_pid(&self.engine_root());
                return Err($failure);
            }};
        }

        let tags = supported_tags();

        let n_ctx = match self_check(&root_url, api_key).await {
            Ok(n) => n,
            Err(e) => {
                // §8.2 rollback trigger 2. §16.1's whole purpose is to
                // intercept an upgrade that renamed a hardening flag or
                // changed a default — so the broken build is marked bad and
                // the compatible install behind it is tried, rather than
                // bricking local inference until someone notices. The
                // reason still reaches the user through `Ready.reason` (see
                // `launch_candidates`) and the `Failed` state if nothing
                // else works.
                if should_roll_back(candidate, &tags) {
                    mark_install_bad(
                        &candidate.dir,
                        &candidate.marker,
                        &format!("failed the security self-check: {e}"),
                    );
                }
                abandon!(LaunchFailure::TryNext(e));
            }
        };
        if n_ctx != ctx {
            // Not fatal: `llama-server` reports the per-slot context here in
            // some builds. Logged because the admission budget below is
            // derived from what we asked for, and a disagreement is worth
            // seeing in the field.
            tracing::info!(
                requested_ctx = ctx,
                props_n_ctx = n_ctx,
                parallel = config.parallel,
                "engine /props reports a different n_ctx than the launch requested"
            );
        }

        // §8.2 rollback trigger 3.
        if let Err(e) = first_generation_probe(&root_url, api_key, model.id).await {
            if should_roll_back(candidate, &tags) {
                mark_install_bad(
                    &candidate.dir,
                    &candidate.marker,
                    &format!("first generation failed: {e}"),
                );
            }
            abandon!(LaunchFailure::TryNext(e));
        }

        let backend = findings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .effective_backend()
            .unwrap_or(candidate.kind.backend);
        if backend != candidate.kind.backend {
            tracing::warn!(
                selected = ?candidate.kind.backend,
                loaded = ?backend,
                "the engine loaded a different backend than the install was selected for"
            );
        }

        let endpoint_value = LocalEndpoint {
            base_url: format!("{root_url}/v1"),
            api_key: api_key.to_string(),
            n_ctx: ctx,
            model_id: model.id.to_string(),
            thinking_hybrid: model.thinking == catalog::ThinkingMode::Hybrid,
        };

        // The install has now served a real generation: record the use so
        // `marker::should_gc` keeps it.
        touch_marker(&candidate.dir, &candidate.marker);

        {
            let mut inner = self.lock();
            inner.running = Some(Running {
                child,
                pid,
                kind: candidate.kind.clone(),
                marker_tag: candidate.marker.tag.clone(),
                backend,
                dir: candidate.dir.clone(),
                idle_unload_secs: config.idle_unload_secs,
                _lock: lock,
                #[cfg(unix)]
                guard_stdin,
            });
            inner.last_interactive_at = now_unix();
        }

        endpoint::publish(Some(endpoint_value.clone()));
        // The admission controller has been metering against a placeholder
        // since boot; this is the only place the real budget is knowable.
        // §17 launches with `--kv-unified`, so every slot draws from ONE
        // shared KV pool of `ctx` tokens — not `ctx * parallel`.
        self.apply_capacity(ctx);
        self.set_state(LocalState::Ready {
            backend,
            degraded,
            reason: degrade_reason,
            ctx,
            gpu_layers: if gpu_layers < 0 {
                // -1 asked the engine for "as many as fit", which for a
                // `Fit::FullGpu` launch is every layer.
                model.header.block_count
            } else {
                gpu_layers as u32
            },
            layer_count: model.header.block_count,
        });
        tracing::info!(?backend, ctx, pid, "on-device engine ready");

        Ok(endpoint_value)
    }

    /// Hand the admission controller the engine's real token budget.
    ///
    /// Called on every launch, so a backend downgrade, a `local.set_config`
    /// resize, or a restart at a different context size all re-set it rather
    /// than leaving admission metering against the previous engine. A shrink
    /// is safe by construction inside `Admission` (it drains rather than
    /// evicting, and re-tests queued requests).
    fn apply_capacity(&self, ctx: u32) {
        admission().set_capacity_tokens(ctx);
        tracing::debug!(
            capacity_tokens = ctx,
            "admission budget set from the live engine"
        );
    }

    /// Spawn the engine and wait for `/health`, forwarding its stderr the
    /// whole time.
    async fn spawn_and_wait_for_health(
        &self,
        spec: &harden::SpawnSpec,
        port: u16,
        api_key: &str,
    ) -> Result<Started, SpawnFailure> {
        // On Unix this wraps the launch in the `--engine-guard` watchdog; on
        // Windows it is the launch itself. See `launch_spec`.
        let launch = launch_spec(spec).map_err(SpawnFailure::Fatal)?;
        let mut command = build_command(&launch);
        let mut child = command.spawn().map_err(|e| {
            SpawnFailure::Failed(LocalError::EngineCrash {
                detail: format!("could not spawn {}: {e}", launch.program().display()),
            })
        })?;
        let pid = child.id().unwrap_or(0);
        write_engine_pid(&self.engine_root(), pid);

        #[cfg(unix)]
        let guard_stdin = child.stdin.take();

        let findings = Arc::new(Mutex::new(StderrFindings::default()));
        if let Some(stderr) = child.stderr.take() {
            let findings = Arc::clone(&findings);
            let supervisor = self.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "nevoflux::local::engine", "{line}");
                    findings
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push_line(&line);
                }
                // The pipe closed: the process is gone (or has closed its
                // own stderr, which it never does). Whoever owns the child
                // reaps it; if a deliberate stop already took it, this is a
                // no-op.
                supervisor.handle_child_exit().await;
            });
        }

        let root_url = format!("http://127.0.0.1:{port}");
        let client = match engine_http_client(Duration::from_secs(5)) {
            Ok(c) => c,
            Err(e) => {
                terminate(&mut child, pid).await;
                return Err(SpawnFailure::Fatal(e));
            }
        };

        let deadline = tokio::time::Instant::now() + HEALTH_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let tail = findings
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .tail_text();
                    let detail = format!("engine exited during startup ({status}): {tail}");
                    clear_engine_pid(&self.engine_root());
                    return Err(if looks_like_bind_failure(&tail) {
                        SpawnFailure::PortInUse(detail)
                    } else {
                        SpawnFailure::Failed(LocalError::EngineCrash { detail })
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    let detail = format!("could not check on the engine process: {e}");
                    terminate(&mut child, pid).await;
                    clear_engine_pid(&self.engine_root());
                    return Err(SpawnFailure::Failed(LocalError::EngineCrash { detail }));
                }
            }

            if let Ok(resp) = client
                .get(format!("{root_url}/health"))
                .bearer_auth(api_key)
                .send()
                .await
            {
                if resp.status().is_success() {
                    return Ok(Started {
                        child,
                        pid,
                        root_url,
                        findings,
                        #[cfg(unix)]
                        guard_stdin,
                    });
                }
            }

            if tokio::time::Instant::now() >= deadline {
                let tail = findings
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .tail_text();
                terminate(&mut child, pid).await;
                clear_engine_pid(&self.engine_root());
                return Err(SpawnFailure::Failed(LocalError::EngineCrash {
                    detail: format!(
                        "engine did not become healthy within {}s: {tail}",
                        HEALTH_TIMEOUT.as_secs()
                    ),
                }));
            }
            tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
        }
    }

    /// The engine went away on its own. Reap it, look at whether the install
    /// is still intact, record the crash, and apply the crash policy.
    async fn handle_child_exit(&self) {
        let Some(mut running) = self.take_running() else {
            return;
        };
        let status = match tokio::time::timeout(STOP_GRACE, running.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not reap the on-device engine");
                None
            }
            Err(_) => None,
        };
        endpoint::publish(None);
        clear_engine_pid(&self.engine_root());

        // Integrity first: a crash caused by a damaged install must be
        // reported as damage, not as a backend that keeps falling over —
        // and must not spend one of that backend's two strikes.
        let dir = running.dir.clone();
        if let Some(marker) = marker::read_marker(&dir) {
            if !marker.files.is_empty() {
                let files = marker.files.clone();
                let verified =
                    tokio::task::spawn_blocking(move || integrity::verify_manifest(&dir, &files))
                        .await;
                if let Ok(Err(e)) = verified {
                    let error = integrity_error_to_local(e);
                    tracing::error!(?error, "the on-device engine's install no longer verifies");
                    self.lock().session_bad.insert(running.kind.clone());
                    self.set_state(LocalState::Failed {
                        retryable: retryable(&error),
                        error,
                    });
                    return;
                }
            }
        }

        let now = now_unix();
        let downgrade = {
            let mut inner = self.lock();
            let log = inner
                .crashes
                .entry((running.marker_tag.clone(), running.backend))
                .or_default();
            log.push(now);
            let downgrade = crash_policy_should_downgrade(log, now);
            if downgrade {
                inner.session_bad.insert(running.kind.clone());
            }
            downgrade
        };

        let detail = match status {
            Some(s) => format!("engine exited unexpectedly ({s})"),
            None => "engine exited unexpectedly".to_string(),
        };
        tracing::warn!(%detail, downgrade, tag = %running.marker_tag, backend = ?running.backend, "on-device engine exited");
        self.set_state(LocalState::Failed {
            error: LocalError::EngineCrash { detail },
            retryable: true,
        });
    }

    /// Stop the engine: ask nicely where that is possible, then insist.
    /// Clears the published endpoint, releases `engine.lock`, removes
    /// `engine.pid`, and returns the admission budget to the floor.
    ///
    /// Safe to call when nothing is running — which is why the daemon's own
    /// shutdown can call it unconditionally.
    pub async fn stop(&self) {
        endpoint::publish(None);
        let Some(mut running) = self.take_running() else {
            return;
        };
        let backend = running.backend;

        #[cfg(unix)]
        {
            // Closing the guard's stdin is the EOF it blocks on: it SIGTERMs
            // the engine's whole process group, waits 3s, then SIGKILLs —
            // §16.2's "SIGTERM → 3s → SIGKILL", performed by the only
            // process that knows that group. Killing the guard here instead
            // would orphan the engine outright.
            drop(running.guard_stdin.take());
            let exited = tokio::time::timeout(STOP_GRACE, running.child.wait())
                .await
                .is_ok();
            if !exited {
                tracing::warn!(
                    pid = running.pid,
                    "engine has not exited; waiting out the guard's own SIGKILL escalation"
                );
                if tokio::time::timeout(STOP_GRACE, running.child.wait())
                    .await
                    .is_err()
                {
                    terminate(&mut running.child, running.pid).await;
                }
            }
        }
        #[cfg(not(unix))]
        {
            // Nothing was signalled and nothing can be — see `terminate` for
            // why CTRL_BREAK is undeliverable to a CREATE_NO_WINDOW child —
            // so there is no graceful exit to wait for. Waiting `STOP_GRACE`
            // first would add a fixed 3s to every idle unload and every
            // daemon shutdown against the 8s force-exit watchdog.
            // `terminate` still waits for the process to actually be gone.
            terminate(&mut running.child, running.pid).await;
        }

        clear_engine_pid(&self.engine_root());
        drop(running); // releases engine.lock
                       // The engine that budget described is gone. Back to the floor until
                       // the next launch publishes a real one, so a request sized against
                       // the old engine cannot be admitted while there is no engine at all.
        admission().set_capacity_tokens(CTX_FLOOR);
        self.set_state(LocalState::Stopped { backend });
        tracing::info!(?backend, "on-device engine stopped");
    }

    /// Forget that `backend` failed, so the next request tries it again.
    ///
    /// Clears both halves of the sticky downgrade: the kinds this process
    /// watched fail, and any [`BadMark`] an earlier run wrote onto an
    /// install of that backend. If a degraded engine is running right now it
    /// is stopped, so the next demand cold-starts back at the top of the
    /// fallback chain instead of staying on the backend it fell back to.
    pub async fn retry_backend(&self, backend: Backend) {
        let root = {
            let mut inner = self.lock();
            inner.session_bad.retain(|k| k.backend != backend);
            // Only this backend's crash budget: another backend's two
            // strikes remain its own (§8.3).
            inner.crashes.retain(|(_, b), _| *b != backend);
            inner.engine_root.clone()
        };
        clear_bad_marks_for_backend(&root, backend);

        let degraded_now = matches!(
            self.state(),
            LocalState::Ready { degraded: true, .. } | LocalState::Failed { .. }
        );
        // Bound to a `let` inside its own scope: a `MutexGuard` temporary in
        // an `if` condition lives to the end of that `if`, which would read
        // as holding a `std::sync::Mutex` across the `.await` below.
        let running_now = {
            let inner = self.lock();
            inner.running.is_some()
        };
        if degraded_now && running_now {
            tracing::info!(
                ?backend,
                "stopping the degraded engine so the retry can cold start"
            );
            self.stop().await;
        }
    }

    /// Start the idle-unload timer. Called once, from [`init`].
    fn spawn_idle_task(&self) {
        {
            let mut inner = self.lock();
            if inner.idle_task_started {
                return;
            }
            inner.idle_task_started = true;
        }
        let supervisor = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(IDLE_TICK).await;
                supervisor.idle_tick().await;
            }
        });
    }

    /// One pass of the idle-unload timer: an engine nobody has talked to for
    /// `idle_unload_secs` gives its memory back.
    ///
    /// Background (P1) work deliberately does not count as activity — see
    /// `Admission::has_interactive_activity` — so a long consolidation job
    /// cannot hold the engine resident against a user who has walked away.
    ///
    /// Activity is sampled once per [`IDLE_TICK`] rather than timestamped,
    /// because `Admission` exposes no "when was the last request" clock. The
    /// unload therefore fires up to one tick (30s) after
    /// `idle_unload_secs` actually elapses, never before.
    async fn idle_tick(&self) {
        let busy = admission().has_interactive_activity();
        let now = now_unix();
        let unload_after = {
            let mut inner = self.lock();
            if busy {
                inner.last_interactive_at = now;
            }
            let Some(running) = inner.running.as_ref() else {
                return;
            };
            if running.idle_unload_secs == 0 {
                return;
            }
            let idle_for = now.saturating_sub(inner.last_interactive_at);
            idle_for > running.idle_unload_secs as i64
        };
        if busy || !unload_after {
            return;
        }
        tracing::info!("on-device engine idle; unloading");
        self.stop().await;
    }
}

/// A spawned engine that has answered `/health`.
struct Started {
    child: tokio::process::Child,
    pid: u32,
    root_url: String,
    findings: Arc<Mutex<StderrFindings>>,
    #[cfg(unix)]
    guard_stdin: Option<tokio::process::ChildStdin>,
}

/// Why one spawn attempt did not produce a healthy engine.
enum SpawnFailure {
    /// The port was taken between reserving and binding it — retry on
    /// another port, same install.
    PortInUse(String),
    /// This install could not serve; another one might.
    Failed(LocalError),
    /// Nothing about another install would be different.
    Fatal(LocalError),
}

/// The spec actually spawned for `spec`.
///
/// On Unix that is `spec` wrapped in the `--engine-guard` watchdog
/// ([`harden::guard_spec`]): the guard, not this daemon, directly parents
/// the engine, so a daemon that is SIGKILLed — or that exits through
/// `force_exit`, which runs no destructors — cannot leave a multi-GB
/// `llama-server` holding the GPU (v3 §16.2 / D10). Windows has no
/// equivalent need: the daemon's kill-on-close Job Object already reaps
/// every child it spawns, and `guard::run` exits 2 there.
#[cfg(unix)]
fn launch_spec(spec: &harden::SpawnSpec) -> Result<harden::SpawnSpec, LocalError> {
    let exe = std::env::current_exe().map_err(|e| LocalError::EngineUnreadable {
        detail: format!("cannot resolve this executable to spawn the engine guard: {e}"),
    })?;
    Ok(harden::guard_spec(&exe, spec))
}

#[cfg(not(unix))]
fn launch_spec(spec: &harden::SpawnSpec) -> Result<harden::SpawnSpec, LocalError> {
    Ok(spec.clone())
}

/// Builds the `Command` for one launch.
///
/// **[`harden::SpawnSpec::to_command`] is the only supported way to get
/// one** — it calls `env_clear()` internally, which v3 §16.1 treats as part
/// of the LocalOnly gate itself. `llama-server` reads 142 environment
/// variables, and inheriting the daemon's environment would let
/// `LLAMA_ARG_AGENT` reopen the built-in tools and
/// `LLAMA_ARG_MODEL_URL`/`HF_REPO`/`RPC` plus `HF_TOKEN` give the engine its
/// own way onto the network, around the daemon's egress guard entirely.
/// Building a `Command` here by hand would compile and no test would fail.
/// [`harden::guard_spec`] carries the same environment through the guard, so
/// wrapping does not reopen that either.
///
/// stdio, on top of what the spec decides:
/// - stdout to null. The daemon's real stdout is the native-messaging
///   channel and `llama-server` is chatty; a single inherited line corrupts
///   the protocol. (The guard inherits our stdout for the engine, so this
///   covers both.)
/// - stderr piped, so the forwarder can log it and read the backend and
///   compute-buffer lines out of it — on Unix the engine's stderr is
///   inherited by the guard and arrives here unchanged.
/// - stdin: null on Windows; on Unix it is the pipe the guard blocks on, so
///   it is piped and the supervisor holds the write end.
fn build_command(spec: &harden::SpawnSpec) -> tokio::process::Command {
    let mut std_command = spec.to_command();

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console window flashes up for a process the
        // user never asked to see. CREATE_NEW_PROCESS_GROUP: the engine does
        // not receive the Ctrl+C the daemon's own console delivers, so it
        // outlives an interactive interrupt only as long as the daemon's
        // kill-on-close Job Object allows (see `assign_self_to_kill_on_close_job`).
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        std_command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    let mut command = tokio::process::Command::from(std_command);
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::piped());

    #[cfg(unix)]
    {
        command.stdin(std::process::Stdio::piped());
        // NOT kill_on_drop on Unix: the child is the `--engine-guard`
        // watchdog, and SIGKILLing it would orphan the engine it is the only
        // watcher of. Dropping its stdin (EOF) is the supported way to take
        // the pair down — see `EngineSupervisor::stop`.
        command.kill_on_drop(false);
    }
    #[cfg(not(unix))]
    {
        command.stdin(std::process::Stdio::null());
        // On Windows the child IS the engine, so dropping the handle without
        // this would leave it running until the daemon's Job Object closes.
        command.kill_on_drop(true);
    }

    command
}

/// Whether the engine's stderr says it could not bind its port.
fn looks_like_bind_failure(stderr_tail: &str) -> bool {
    let lower = stderr_tail.to_ascii_lowercase();
    lower.contains("address already in use")
        || lower.contains("bind: ")
        || lower.contains("failed to bind")
        || lower.contains("couldn't bind")
        || lower.contains("error while binding")
}

/// Kill `child` and wait for it to actually be gone.
///
/// On Windows this is `TerminateProcess`. The CTRL_BREAK half of "ask, then
/// insist" is deliberately not attempted: `GenerateConsoleCtrlEvent` only
/// reaches a process group inside the *caller's* console, and the engine is
/// spawned `CREATE_NO_WINDOW` — it has its own hidden console, not the
/// daemon's, so the event could never be delivered and implementing it would
/// be dead code. This is consistent with §16.2's Job Object arrangement on
/// Windows, whose `KILL_ON_JOB_CLOSE` fallback is itself an abrupt
/// terminate, and `llama-server` holds nothing that needs flushing (its KV
/// cache is memory).
///
/// On Unix this is the last resort only: [`EngineSupervisor::stop`] has
/// already closed the guard's stdin and given it both its SIGTERM window and
/// its own SIGKILL escalation before reaching here.
async fn terminate(child: &mut tokio::process::Child, pid: u32) {
    if let Err(e) = child.start_kill() {
        tracing::warn!(pid, error = %e, "could not signal the on-device engine");
    }
    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(Ok(status)) => tracing::debug!(pid, %status, "on-device engine terminated"),
        Ok(Err(e)) => tracing::warn!(pid, error = %e, "could not reap the on-device engine"),
        Err(_) => tracing::error!(pid, "on-device engine did not exit after being killed"),
    }
}

/// Take the machine-wide `engine.lock`, or report that another NevoFlux is
/// already driving an engine.
fn acquire_engine_lock(root: &Path) -> Result<std::fs::File, LocalError> {
    use fs2::FileExt;

    let path = root.join("engine.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| LocalError::EngineUnreadable {
            detail: format!("{}: {e}", path.display()),
        })?;
    file.try_lock_exclusive().map_err(|_| LocalError::Busy)?;
    Ok(file)
}

fn engine_pid_path(root: &Path) -> PathBuf {
    root.join("engine.pid")
}

/// Record what we just spawned: pid, creation time and executable path
/// (§16.2 / §6).
///
/// If the identity cannot be read, nothing is written at all — a pid on its
/// own is not enough to justify killing anything later, so recording one
/// would be worse than recording nothing.
fn write_engine_pid(root: &Path, pid: u32) {
    let Some(identity) = live_identity(pid) else {
        tracing::warn!(
            pid,
            "could not read the engine's process identity; not recording engine.pid, so a \
             later run will not try to reap it"
        );
        return;
    };
    match serde_json::to_vec(&identity) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(engine_pid_path(root), bytes) {
                tracing::warn!(error = %e, "could not record the on-device engine's pid");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not serialize the engine's identity"),
    }
}

fn clear_engine_pid(root: &Path) {
    let _ = std::fs::remove_file(engine_pid_path(root));
}

/// Kill whatever `engine.pid` recorded, but only if all three recorded
/// fields still match the live process.
///
/// Reached only while holding `engine.lock`, so the recorded process cannot
/// be a live sibling daemon's engine — it is either our own orphan (this
/// daemon was killed without running [`EngineSupervisor::stop`]) or a pid
/// the OS has since recycled. §16.2 requires pid + start time + exe path
/// precisely because the second case is otherwise indistinguishable: a
/// name-only check would happily `taskkill /F /T` an unrelated process, and
/// on this audience's machines "some other llama-server" is a realistic
/// thing to find. Any mismatch, or any field that cannot be read, means no
/// kill.
fn reap_stale_engine(root: &Path) {
    let path = engine_pid_path(root);
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let recorded: Option<EngineIdentity> = serde_json::from_slice(&bytes).ok();
    // Removed either way: a record we cannot act on is not worth keeping,
    // and one we do act on is spent.
    let _ = std::fs::remove_file(&path);

    let Some(recorded) = recorded else {
        tracing::warn!("engine.pid did not hold a recognizable identity record; ignoring it");
        return;
    };
    if recorded.pid == 0 || recorded.pid == std::process::id() {
        return;
    }
    let Some(live) = live_identity(recorded.pid) else {
        tracing::debug!(
            pid = recorded.pid,
            "the recorded engine process is gone; nothing to reap"
        );
        return;
    };
    if live != recorded {
        tracing::warn!(
            pid = recorded.pid,
            recorded_exe = %recorded.exe_path,
            live_exe = %live.exe_path,
            "pid now belongs to a different process; NOT killing it"
        );
        return;
    }
    tracing::warn!(
        pid = recorded.pid,
        exe = %recorded.exe_path,
        "reaping an orphaned on-device engine from a previous run"
    );
    kill_pid(recorded.pid);
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output();
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
}

/// One `max_tokens: 1` generation, to prove the engine can actually run the
/// model rather than merely answer `/health`. A build that loads but cannot
/// generate (a bad kernel for this GPU, a chat template it cannot apply) is
/// exactly what the §8.2 rollback exists for, and only a real generation
/// finds it.
async fn first_generation_probe(
    root_url: &str,
    api_key: &str,
    model_id: &str,
) -> Result<(), LocalError> {
    let client = engine_http_client(Duration::from_secs(120))?;
    let body = serde_json::json!({
        "model": model_id,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": false,
    });
    let response = client
        .post(format!("{root_url}/v1/chat/completions"))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| LocalError::EngineCrash {
            detail: format!("first generation failed: {e}"),
        })?;
    if !response.status().is_success() {
        let status = response.status();
        let text = read_capped(response).await;
        return Err(LocalError::EngineCrash {
            detail: format!(
                "first generation returned HTTP {status}: {}",
                text.chars().take(500).collect::<String>()
            ),
        });
    }
    let text = read_capped(response).await;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| LocalError::EngineCrash {
            detail: format!("first generation response was not JSON: {e}"),
        })?;
    if value["choices"].get(0).is_none() {
        return Err(LocalError::EngineCrash {
            detail: format!("first generation returned no choices: {value}"),
        });
    }
    Ok(())
}

/// Record that an install was used, so `marker::should_gc` keeps it.
fn touch_marker(dir: &Path, current: &Marker) {
    let mut updated = current.clone();
    updated.last_used_at = now_unix();
    if let Err(e) = marker::write_marker(dir, &updated) {
        tracing::warn!(error = %e, dir = %dir.display(), "could not update the install's last_used_at");
    }
}

/// Mark an install unusable on disk, so this daemon and the next one skip it.
fn mark_install_bad(dir: &Path, current: &Marker, reason: &str) {
    let mut updated = current.clone();
    updated.bad = Some(BadMark {
        by_build: env!("CARGO_PKG_VERSION").to_string(),
        reason: reason.to_string(),
    });
    if let Err(e) = marker::write_marker(dir, &updated) {
        tracing::warn!(error = %e, dir = %dir.display(), "could not mark the install bad");
    } else {
        tracing::warn!(dir = %dir.display(), reason, "install marked bad");
    }
}

/// Remove the [`BadMark`] from every install of `backend` under `root` — the
/// on-disk half of [`EngineSupervisor::retry_backend`].
///
/// Takes the root explicitly rather than resolving it: the supervisor owns
/// which cache directory it is allowed to rewrite, so a test instance can
/// never reach the real user cache.
fn clear_bad_marks_for_backend(root: &Path, backend: Backend) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let slug = format!("-{}", backend_slug(backend));
    for entry in entries.filter_map(|e| e.ok()) {
        let dir = entry.path();
        let Some(mut m) = marker::read_marker(&dir) else {
            continue;
        };
        if m.bad.is_none() || !m.kind.contains(&slug) {
            continue;
        }
        m.bad = None;
        if let Err(e) = marker::write_marker(&dir, &m) {
            tracing::warn!(error = %e, dir = %dir.display(), "could not clear the install's bad mark");
        }
    }
}

/// The process-wide engine supervisor.
pub fn supervisor() -> &'static EngineSupervisor {
    static SUPERVISOR: OnceLock<EngineSupervisor> = OnceLock::new();
    SUPERVISOR.get_or_init(|| EngineSupervisor::new(default_engine_root()))
}

/// Wire the supervisor into the daemon at boot.
///
/// Deliberately does no probing, no installing and no spawning: v3 §10 keeps
/// the whole on-device pipeline demand-driven, so a daemon whose user never
/// touches local inference pays nothing for it. All this does is register
/// the cold-start hook `crate::local::endpoint::ensure` falls back to and
/// start the idle timer.
///
/// Takes no directory: everything this module writes lives under the shared
/// engine cache root (§6), which [`default_engine_root`] resolves, not under
/// the daemon's own data directory.
///
/// Must be called from inside a tokio runtime (the idle timer is a spawned
/// task).
pub fn init() {
    let sup = supervisor();
    endpoint::install_ensure(Arc::new(|| {
        Box::pin(async {
            supervisor().ensure_started().await.map_err(|e| {
                // The JSON form keeps the error's `code` and `detail`, which
                // a bare Display would drop.
                serde_json::to_string(&e).unwrap_or_else(|_| e.to_string())
            })
        })
    }));
    sup.spawn_idle_task();
    tracing::debug!(
        engine_root = %sup.engine_root().display(),
        "on-device engine supervisor registered"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::marker::FileEntry;

    // --- parse_loaded_backends -------------------------------------------

    #[test]
    fn parses_the_backends_llama_server_reports_loading() {
        assert_eq!(
            parse_loaded_backends(
                "load_backend: loaded CUDA backend from C:\\engine\\ggml-cuda.dll"
            ),
            Some(Backend::Cuda)
        );
        assert_eq!(
            parse_loaded_backends(
                "load_backend: loaded Vulkan backend from /engine/libggml-vulkan.so"
            ),
            Some(Backend::Vulkan)
        );
        assert_eq!(
            parse_loaded_backends(
                "load_backend: loaded Metal backend from /engine/libggml-metal.dylib"
            ),
            Some(Backend::Metal)
        );
        assert_eq!(
            parse_loaded_backends(
                "load_backend: loaded CPU backend from /engine/libggml-cpu-haswell.so"
            ),
            Some(Backend::Cpu)
        );
    }

    #[test]
    fn a_line_with_a_real_timestamp_prefix_still_parses() {
        assert_eq!(
            parse_loaded_backends(
                "ggml_backend_load_best: load_backend: loaded CUDA backend from ggml-cuda.dll"
            ),
            Some(Backend::Cuda)
        );
    }

    #[test]
    fn backends_this_daemon_does_not_model_are_not_reported() {
        // RPC is a real `load_backend` line, and it says nothing about which
        // compute backend the model actually runs on.
        assert_eq!(
            parse_loaded_backends("load_backend: loaded RPC backend from /engine/libggml-rpc.so"),
            None
        );
        assert_eq!(
            parse_loaded_backends("llama_model_loader: loaded meta data"),
            None
        );
        assert_eq!(parse_loaded_backends(""), None);
    }

    // --- crash_policy_should_downgrade -------------------------------------

    #[test]
    fn one_crash_is_not_enough_to_downgrade() {
        assert!(!crash_policy_should_downgrade(&[1_000], 1_000));
    }

    #[test]
    fn two_crashes_inside_the_window_downgrade() {
        assert!(crash_policy_should_downgrade(&[1_000, 1_500], 1_500));
    }

    #[test]
    fn two_crashes_exactly_at_the_window_edge_still_count() {
        // 600s apart, evaluated at the later one: both are within
        // `now - t <= 600`.
        assert!(crash_policy_should_downgrade(&[900, 1_500], 1_500));
    }

    #[test]
    fn crashes_that_have_aged_out_of_the_window_do_not_downgrade() {
        // The first is 601s old at `now`; only the second counts.
        assert!(!crash_policy_should_downgrade(&[899, 1_500], 1_500));
    }

    #[test]
    fn an_empty_crash_log_never_downgrades() {
        assert!(!crash_policy_should_downgrade(&[], 1_500));
    }

    /// §8.3 gives each `(tag, backend)` its own two-strike budget. The bug
    /// this pins: one shared crash list meant the backend we downgraded TO
    /// inherited its predecessor's timestamps and was itself downgraded on
    /// its very first crash.
    #[test]
    fn each_tag_and_backend_gets_its_own_two_strike_budget() {
        let mut crashes: HashMap<(String, Backend), Vec<i64>> = HashMap::new();
        let cuda = ("b10909".to_string(), Backend::Cuda);
        let vulkan = ("b10909".to_string(), Backend::Vulkan);

        crashes.entry(cuda.clone()).or_default().push(1_000);
        crashes.entry(cuda.clone()).or_default().push(1_100);
        assert!(
            crash_policy_should_downgrade(&crashes[&cuda], 1_100),
            "two CUDA crashes in the window must downgrade CUDA"
        );

        crashes.entry(vulkan.clone()).or_default().push(1_150);
        assert!(
            !crash_policy_should_downgrade(&crashes[&vulkan], 1_150),
            "Vulkan's FIRST crash must not downgrade it just because CUDA already crashed twice"
        );
    }

    // --- integrity error mapping --------------------------------------------

    #[test]
    fn an_io_failure_is_unreadable_not_corrupt() {
        // A reinstall does not fix — and may not even be reachable under —
        // an I/O failure, so it must not land on the reinstall path.
        assert_eq!(
            integrity_error_to_local(IntegrityError::Io("permission denied".into())),
            LocalError::EngineUnreadable {
                detail: "permission denied".into()
            }
        );
    }

    #[test]
    fn every_other_integrity_failure_is_corrupt() {
        for e in [
            IntegrityError::Missing("llama.dll".into()),
            IntegrityError::SizeMismatch("llama.dll".into()),
            IntegrityError::HashMismatch("llama.dll".into()),
            IntegrityError::Unexpected("evil.dll".into()),
        ] {
            assert!(
                matches!(
                    integrity_error_to_local(e),
                    LocalError::EngineCorrupt { .. }
                ),
                "expected EngineCorrupt"
            );
        }
    }

    // --- usable_candidates: the fallback walk -------------------------------

    fn kind(backend: Backend, variant: Option<&str>) -> InstallKind {
        InstallKind {
            platform: "windows-x64".to_string(),
            backend,
            variant: variant.map(str::to_string),
            cudart: None,
        }
    }

    /// Creates an install directory for `(tag, kind)` with a valid marker.
    /// `kind_suffix` is private to `install`, so the directory name is
    /// derived from `install::install_dir` itself — which is also what
    /// `marker::read_marker` checks the marker's own `kind` against.
    fn fake_install(root: &Path, tag: &str, k: &InstallKind, bad: Option<BadMark>) -> PathBuf {
        let dir = install::install_dir(root, tag, k);
        std::fs::create_dir_all(&dir).unwrap();
        let dir_name = dir.file_name().unwrap().to_str().unwrap().to_string();
        let kind_suffix = dir_name
            .strip_prefix(&format!("{tag}-"))
            .expect("install_dir names are <tag>-<kind suffix>")
            .to_string();
        let m = Marker {
            tag: tag.to_string(),
            kind: kind_suffix,
            archive_sha256: vec!["a".repeat(64)],
            files: vec![FileEntry {
                path: "llama-server.exe".to_string(),
                size: 1,
                sha256: "b".repeat(64),
            }],
            installed_at: 1,
            last_used_at: 1,
            bad,
        };
        marker::write_marker(&dir, &m).unwrap();
        dir
    }

    /// A synthetic pinned/compatible pair, so the rollback walk is testable
    /// while the real `release::ENGINE_COMPATIBLE` is still empty.
    const PINNED_TAG: &str = "b99999-new";
    const COMPATIBLE_TAG: &str = "b10909-known-good";
    fn synthetic_tags() -> Vec<&'static str> {
        vec![PINNED_TAG, COMPATIBLE_TAG]
    }

    #[test]
    fn the_fallback_walk_prefers_the_first_installed_kind_in_the_chain() {
        let root = tempfile::tempdir().unwrap();
        let tag = release::ENGINE_PINNED.tag;
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        let vulkan = kind(Backend::Vulkan, None);
        let cpu = kind(Backend::Cpu, None);
        fake_install(root.path(), tag, &cuda, None);
        fake_install(root.path(), tag, &cpu, None);

        let chain = vec![cuda.clone(), vulkan, cpu.clone()];
        let found = usable_candidates(root.path(), &chain, &supported_tags(), &HashSet::new());

        // Vulkan is in the chain but not installed, so it never appears.
        assert_eq!(
            found.iter().map(|c| c.kind.clone()).collect::<Vec<_>>(),
            vec![cuda, cpu]
        );
        assert_eq!(found[0].chain_index, 0);
        assert_eq!(found[1].chain_index, 2);
    }

    #[test]
    fn a_kind_marked_bad_in_this_session_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let tag = release::ENGINE_PINNED.tag;
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        let cpu = kind(Backend::Cpu, None);
        fake_install(root.path(), tag, &cuda, None);
        fake_install(root.path(), tag, &cpu, None);

        let mut session_bad = HashSet::new();
        session_bad.insert(cuda.clone());

        let found = usable_candidates(
            root.path(),
            &[cuda, cpu.clone()],
            &supported_tags(),
            &session_bad,
        );
        assert_eq!(
            found.iter().map(|c| c.kind.clone()).collect::<Vec<_>>(),
            vec![cpu],
            "the bad-marked kind must be skipped even though it is installed and preferred"
        );
    }

    #[test]
    fn an_install_marked_bad_on_disk_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let tag = release::ENGINE_PINNED.tag;
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        fake_install(
            root.path(),
            tag,
            &cuda,
            Some(BadMark {
                by_build: "0.3.15".to_string(),
                reason: "first generation failed".to_string(),
            }),
        );
        assert!(
            usable_candidates(root.path(), &[cuda], &supported_tags(), &HashSet::new()).is_empty()
        );
    }

    #[test]
    fn a_tag_this_build_does_not_support_is_never_probed() {
        let root = tempfile::tempdir().unwrap();
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        // A directory left by a much older daemon: its marker is internally
        // consistent, but this build has no archive table for the tag, so it
        // is not in the supported list and never looked at.
        fake_install(root.path(), "b00001-ancient", &cuda, None);
        assert!(
            usable_candidates(root.path(), &[cuda], &supported_tags(), &HashSet::new()).is_empty()
        );
    }

    #[test]
    fn nothing_installed_leaves_no_candidates() {
        let root = tempfile::tempdir().unwrap();
        assert!(usable_candidates(
            root.path(),
            &[kind(Backend::Cpu, None)],
            &supported_tags(),
            &HashSet::new()
        )
        .is_empty());
    }

    /// The pinned-first ordering the §8.2 rollback depends on: both installs
    /// of one kind are candidates, newest first, so the walk can fall
    /// through from one to the other without a second lookup.
    #[test]
    fn both_supported_tags_are_candidates_for_one_kind_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        fake_install(root.path(), PINNED_TAG, &cuda, None);
        fake_install(root.path(), COMPATIBLE_TAG, &cuda, None);

        let found = usable_candidates(root.path(), &[cuda], &synthetic_tags(), &HashSet::new());

        assert_eq!(
            found
                .iter()
                .map(|c| c.marker.tag.as_str())
                .collect::<Vec<_>>(),
            vec![PINNED_TAG, COMPATIBLE_TAG]
        );
    }

    /// §8.2's rollback, end of the walk: once the newly pinned build has
    /// been marked bad (by a failed self-check or first generation), the
    /// compatible install behind it is what the next cold start picks.
    #[test]
    fn a_bad_marked_pinned_install_falls_back_to_the_compatible_tag_behind_it() {
        let root = tempfile::tempdir().unwrap();
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        let pinned_dir = fake_install(root.path(), PINNED_TAG, &cuda, None);
        fake_install(root.path(), COMPATIBLE_TAG, &cuda, None);

        // Exactly what `finish_launch` does on a failed self-check.
        let marker = marker::read_marker(&pinned_dir).unwrap();
        mark_install_bad(&pinned_dir, &marker, "failed the security self-check");

        let found = usable_candidates(root.path(), &[cuda], &synthetic_tags(), &HashSet::new());

        assert_eq!(
            found
                .iter()
                .map(|c| c.marker.tag.as_str())
                .collect::<Vec<_>>(),
            vec![COMPATIBLE_TAG],
            "the rolled-back build must drop out and the known-good one take over"
        );
    }

    #[test]
    fn only_the_newest_tag_with_a_real_manifest_is_ever_rolled_back() {
        let root = tempfile::tempdir().unwrap();
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        fake_install(root.path(), PINNED_TAG, &cuda, None);
        fake_install(root.path(), COMPATIBLE_TAG, &cuda, None);
        let found = usable_candidates(root.path(), &[cuda], &synthetic_tags(), &HashSet::new());
        let tags = synthetic_tags();

        assert!(
            should_roll_back(&found[0], &tags),
            "the pinned install is the one an upgrade broke"
        );
        assert!(
            !should_roll_back(&found[1], &tags),
            "marking the fallback bad would throw away the thing we roll back TO"
        );

        // The live-test override has no manifest behind it and names a
        // directory this daemon does not own.
        let mut override_candidate = found[0].clone();
        override_candidate.marker.files.clear();
        assert!(!should_roll_back(&override_candidate, &tags));
    }

    // --- self_check ---------------------------------------------------------

    /// A stand-in engine: answers a fixed reply per path, over as many
    /// connections as the caller makes, until the test drops it.
    ///
    /// Hand-rolled for the same reason `wasm::local_llm`'s tests hand-roll
    /// theirs (no mocking dependency in this crate), but multi-connection —
    /// `self_check` makes four separate requests, and a one-shot fixture
    /// would only ever exercise the first.
    struct FakeEngine {
        base_url: String,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    /// One path's canned reply: `(status line + extra headers, body)`.
    type Route = (&'static str, String);

    async fn fake_engine(routes: Vec<(&'static str, Route)>) -> FakeEngine {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, mut rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = &mut rx => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _)) = accepted else { break };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let authorized = request.to_ascii_lowercase().contains("authorization:");
                    let key = if authorized {
                        format!("AUTH {path}")
                    } else {
                        format!("ANON {path}")
                    };
                    let (head, body) = routes
                        .iter()
                        .find(|(k, _)| *k == key)
                        .map(|(_, r)| r.clone())
                        .unwrap_or(("HTTP/1.1 404 Not Found", String::new()));
                    let response = format!(
                        "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        FakeEngine {
            base_url: format!("http://{addr}"),
            _shutdown: shutdown,
        }
    }

    /// The replies a correctly hardened engine gives — using the status
    /// codes the REAL pinned build returns, captured from a live run:
    /// `/slots` answers 501 (not 404), and `/` answers 404 with JSON.
    fn hardened_routes() -> Vec<(&'static str, Route)> {
        vec![
            (
                "ANON /v1/models",
                ("HTTP/1.1 401 Unauthorized", String::new()),
            ),
            (
                "AUTH /slots",
                ("HTTP/1.1 501 Not Implemented", String::new()),
            ),
            (
                "AUTH /",
                (
                    "HTTP/1.1 404 Not Found\r\nContent-Type: application/json",
                    serde_json::json!({"error": "not found"}).to_string(),
                ),
            ),
            (
                "AUTH /props",
                (
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json",
                    serde_json::json!({"default_generation_settings": {"n_ctx": 32768}})
                        .to_string(),
                ),
            ),
        ]
    }

    #[tokio::test]
    async fn self_check_passes_a_hardened_engine_and_reports_its_context_size() {
        let engine = fake_engine(hardened_routes()).await;
        assert_eq!(self_check(&engine.base_url, "secret").await, Ok(32768));
    }

    /// The criterion is "did it answer", never one exact status code. This
    /// is the test that would fail if someone "tightened" `self_check` back
    /// to `== 404` / `== 401` — the amendment that the live 501 forced.
    #[tokio::test]
    async fn self_check_accepts_any_refusal_status_not_one_exact_code() {
        for (models_status, slots_status) in [
            ("HTTP/1.1 401 Unauthorized", "HTTP/1.1 501 Not Implemented"),
            ("HTTP/1.1 403 Forbidden", "HTTP/1.1 404 Not Found"),
            (
                "HTTP/1.1 400 Bad Request",
                "HTTP/1.1 503 Service Unavailable",
            ),
        ] {
            let mut routes = hardened_routes();
            routes[0] = ("ANON /v1/models", (models_status, String::new()));
            routes[1] = ("AUTH /slots", (slots_status, String::new()));
            let engine = fake_engine(routes).await;
            assert_eq!(
                self_check(&engine.base_url, "secret").await,
                Ok(32768),
                "a refusal of {models_status} / {slots_status} is still a refusal"
            );
        }
    }

    #[tokio::test]
    async fn self_check_rejects_an_engine_that_serves_models_unauthenticated() {
        let mut routes = hardened_routes();
        routes[0] = (
            "ANON /v1/models",
            (
                "HTTP/1.1 200 OK\r\nContent-Type: application/json",
                serde_json::json!({"data": []}).to_string(),
            ),
        );
        let engine = fake_engine(routes).await;
        let err = self_check(&engine.base_url, "secret").await.unwrap_err();
        assert!(
            matches!(err, LocalError::EngineInsecure { ref detail } if detail.contains("/v1/models")),
            "{err:?}"
        );
    }

    /// v3 §16.1's fuller criterion: "非 2xx 且未返回模型列表". A refusal
    /// status that still enumerates the loaded model has not refused.
    #[tokio::test]
    async fn self_check_rejects_a_refusal_that_still_lists_the_model() {
        let mut routes = hardened_routes();
        routes[0] = (
            "ANON /v1/models",
            (
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json",
                serde_json::json!({"data": [{"id": "qwen3-4b-instruct-2507"}]}).to_string(),
            ),
        );
        let engine = fake_engine(routes).await;
        let err = self_check(&engine.base_url, "secret").await.unwrap_err();
        assert!(
            matches!(err, LocalError::EngineInsecure { ref detail } if detail.contains("listed models")),
            "{err:?}"
        );
    }

    #[test]
    fn a_model_listing_is_recognized_only_when_it_actually_lists_something() {
        assert!(body_lists_models(r#"{"data":[{"id":"m"}]}"#));
        assert!(body_lists_models(r#"{"models":[{"id":"m"}]}"#));
        assert!(!body_lists_models(r#"{"data":[]}"#));
        assert!(!body_lists_models(r#"{"error":"unauthorized"}"#));
        assert!(!body_lists_models("not json at all"));
        assert!(!body_lists_models(""));
    }

    #[tokio::test]
    async fn self_check_rejects_an_engine_whose_slots_endpoint_answers() {
        let mut routes = hardened_routes();
        routes[1] = (
            "AUTH /slots",
            (
                "HTTP/1.1 200 OK\r\nContent-Type: application/json",
                "[]".to_string(),
            ),
        );
        let engine = fake_engine(routes).await;
        let err = self_check(&engine.base_url, "secret").await.unwrap_err();
        assert!(
            matches!(err, LocalError::EngineInsecure { ref detail } if detail.contains("/slots")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn self_check_rejects_an_engine_still_serving_the_web_ui() {
        let mut routes = hardened_routes();
        routes[2] = (
            "AUTH /",
            (
                "HTTP/1.1 200 OK\r\nContent-Type: text/html",
                "<!DOCTYPE html><html><body>llama.cpp</body></html>".to_string(),
            ),
        );
        let engine = fake_engine(routes).await;
        let err = self_check(&engine.base_url, "secret").await.unwrap_err();
        assert!(
            matches!(err, LocalError::EngineInsecure { ref detail } if detail.contains("web UI")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn self_check_rejects_an_html_root_that_does_not_admit_to_being_html() {
        // Same page, served without a `Content-Type` — the body itself is
        // the evidence.
        let mut routes = hardened_routes();
        routes[2] = (
            "AUTH /",
            (
                "HTTP/1.1 200 OK",
                "<!doctype html><html><body>llama.cpp</body></html>".to_string(),
            ),
        );
        let engine = fake_engine(routes).await;
        assert!(matches!(
            self_check(&engine.base_url, "secret").await,
            Err(LocalError::EngineInsecure { .. })
        ));
    }

    #[tokio::test]
    async fn self_check_fails_closed_when_props_cannot_confirm_the_launch() {
        let mut routes = hardened_routes();
        routes[3] = (
            "AUTH /props",
            (
                "HTTP/1.1 200 OK\r\nContent-Type: application/json",
                serde_json::json!({"something_else": true}).to_string(),
            ),
        );
        let engine = fake_engine(routes).await;
        let err = self_check(&engine.base_url, "secret").await.unwrap_err();
        assert!(
            matches!(err, LocalError::EngineInsecure { ref detail } if detail.contains("n_ctx")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn self_check_fails_closed_when_the_engine_is_not_listening_at_all() {
        // Nothing has ever listened on this port in this test: every check
        // errors rather than answering, and an unverifiable engine is not a
        // trusted one.
        let dead = format!("http://127.0.0.1:{}", harden::pick_free_port().unwrap());
        assert!(matches!(
            self_check(&dead, "secret").await,
            Err(LocalError::EngineInsecure { .. })
        ));
    }

    #[tokio::test]
    async fn a_huge_response_body_is_read_only_up_to_the_cap() {
        // A `--static-path` serving something enormous must not make the
        // self-check allocate it.
        let huge = "x".repeat(MAX_PROBE_BODY_BYTES * 3);
        let mut routes = hardened_routes();
        routes[2] = (
            "AUTH /",
            ("HTTP/1.1 200 OK\r\nContent-Type: text/plain", huge),
        );
        let engine = fake_engine(routes).await;
        // Still passes (it is not HTML), and the body never lands in full.
        assert_eq!(self_check(&engine.base_url, "secret").await, Ok(32768));
    }

    #[test]
    fn n_ctx_is_read_from_either_shape_props_can_report_it_in() {
        assert_eq!(
            n_ctx_from_props(&serde_json::json!({"default_generation_settings": {"n_ctx": 8192}})),
            Some(8192)
        );
        assert_eq!(
            n_ctx_from_props(&serde_json::json!({"n_ctx": 4096})),
            Some(4096)
        );
        assert_eq!(n_ctx_from_props(&serde_json::json!({})), None);
    }

    // --- bind-failure classification -----------------------------------------

    #[test]
    fn a_bind_failure_in_stderr_is_recognized_as_a_port_collision() {
        assert!(looks_like_bind_failure(
            "srv    load: error: failed to bind to 127.0.0.1:8080"
        ));
        assert!(looks_like_bind_failure(
            "bind: Address already in use (os error 98)"
        ));
        assert!(!looks_like_bind_failure(
            "CUDA error: out of memory (device 0)"
        ));
    }

    // --- lock / pid identity ----------------------------------------------------

    #[test]
    fn a_second_holder_of_the_engine_lock_is_told_the_machine_is_busy() {
        let root = tempfile::tempdir().unwrap();
        let first = acquire_engine_lock(root.path()).expect("the first holder takes the lock");
        // `unwrap_err`, not `assert_eq!` on the whole `Result`: the Ok side
        // is a `std::fs::File`, which is deliberately not `PartialEq`.
        assert_eq!(
            acquire_engine_lock(root.path()).unwrap_err(),
            LocalError::Busy
        );
        drop(first);
        acquire_engine_lock(root.path()).expect("the lock is free once the first holder drops it");
    }

    #[test]
    fn this_processs_own_identity_is_readable_and_names_its_executable() {
        let me = live_identity(std::process::id()).expect("a live process has an identity");
        assert_eq!(me.pid, std::process::id());
        assert!(!me.start_token.is_empty(), "a creation token is required");
        assert!(
            me.exe_path.to_ascii_lowercase().contains("nevoflux"),
            "exe_path should name this test binary, got {:?}",
            me.exe_path
        );
    }

    #[test]
    fn an_identity_round_trips_through_the_pid_file() {
        let root = tempfile::tempdir().unwrap();
        write_engine_pid(root.path(), std::process::id());
        let bytes = std::fs::read(engine_pid_path(root.path())).unwrap();
        let recorded: EngineIdentity = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(Some(recorded), live_identity(std::process::id()));
    }

    #[test]
    fn a_recycled_pid_is_never_killed() {
        // The §16.2 case this exists for: the pid is live, but it is not the
        // process we recorded. Written with THIS process's pid and a
        // deliberately wrong creation token — so if the identity check were
        // reduced to "is a process with this pid alive", the test runner
        // itself would be killed and this test would never report.
        let root = tempfile::tempdir().unwrap();
        let mut forged = live_identity(std::process::id()).unwrap();
        forged.start_token = format!("{}-not-the-same-process", forged.start_token);
        std::fs::write(
            engine_pid_path(root.path()),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();

        reap_stale_engine(root.path());

        // Surviving to this line IS the assertion.
        assert!(!engine_pid_path(root.path()).exists());
    }

    #[test]
    fn a_pid_record_naming_this_process_exactly_is_still_not_killed() {
        // Belt and braces for the same guarantee: `reap_stale_engine`
        // refuses to act on our own pid before it ever compares identities.
        let root = tempfile::tempdir().unwrap();
        write_engine_pid(root.path(), std::process::id());
        reap_stale_engine(root.path());
        assert!(!engine_pid_path(root.path()).exists());
    }

    #[test]
    fn a_garbage_pid_file_is_cleaned_up_rather_than_parsed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(engine_pid_path(root.path()), "not-a-pid").unwrap();
        reap_stale_engine(root.path());
        assert!(!engine_pid_path(root.path()).exists());
    }

    // --- supervisor lifecycle --------------------------------------------------

    #[tokio::test]
    async fn a_fresh_supervisor_is_idle_and_stopping_it_is_a_no_op() {
        let _g = crate::local::endpoint::test_serial_async().await;
        let root = tempfile::tempdir().unwrap();
        let supervisor = EngineSupervisor::new(root.path().to_path_buf());
        assert_eq!(supervisor.state(), LocalState::Idle);
        supervisor.stop().await;
        assert_eq!(
            supervisor.state(),
            LocalState::Idle,
            "stopping an engine that was never started must not invent a Stopped state"
        );
        assert_eq!(endpoint::current(), None);
    }

    /// Rooted at a tempdir, so it is structurally incapable of reading or
    /// rewriting `marker.json` in the real user cache — the supervisor owns
    /// which directory it may touch, rather than a test having to remember
    /// to redirect an environment variable.
    #[tokio::test]
    async fn retry_backend_forgets_that_a_backend_failed() {
        let root = tempfile::tempdir().unwrap();
        let supervisor = EngineSupervisor::new(root.path().to_path_buf());
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        let vulkan = kind(Backend::Vulkan, None);
        {
            let mut inner = supervisor.lock();
            inner.session_bad.insert(cuda.clone());
            inner.session_bad.insert(vulkan.clone());
            inner
                .crashes
                .insert(("t".to_string(), Backend::Cuda), vec![now_unix()]);
            inner
                .crashes
                .insert(("t".to_string(), Backend::Vulkan), vec![now_unix()]);
        }

        supervisor.retry_backend(Backend::Cuda).await;

        let inner = supervisor.lock();
        assert!(
            !inner.session_bad.contains(&cuda),
            "the retried backend's sticky downgrade must be cleared"
        );
        assert!(
            inner.session_bad.contains(&vulkan),
            "another backend's downgrade must be left alone"
        );
        assert!(
            !inner
                .crashes
                .contains_key(&("t".to_string(), Backend::Cuda)),
            "the retried backend's crash budget is reset"
        );
        assert!(
            inner
                .crashes
                .contains_key(&("t".to_string(), Backend::Vulkan)),
            "another backend's crash budget must survive"
        );
    }

    #[test]
    fn clearing_bad_marks_touches_only_the_named_backend_under_the_given_root() {
        let root = tempfile::tempdir().unwrap();
        let bad = || {
            Some(BadMark {
                by_build: "0.3.15".to_string(),
                reason: "self-check".to_string(),
            })
        };
        let cuda = kind(Backend::Cuda, Some("cuda13-older"));
        let vulkan = kind(Backend::Vulkan, None);
        let tag = release::ENGINE_PINNED.tag;
        let cuda_dir = fake_install(root.path(), tag, &cuda, bad());
        let vulkan_dir = fake_install(root.path(), tag, &vulkan, bad());

        clear_bad_marks_for_backend(root.path(), Backend::Cuda);

        assert!(marker::read_marker(&cuda_dir).unwrap().bad.is_none());
        assert!(
            marker::read_marker(&vulkan_dir).unwrap().bad.is_some(),
            "another backend's mark must be left alone"
        );
    }

    #[test]
    fn retryability_matches_what_a_user_can_actually_act_on() {
        assert!(retryable(&LocalError::Busy));
        assert!(retryable(&LocalError::EngineCrash {
            detail: String::new()
        }));
        assert!(retryable(&LocalError::BackendUnavailable));
        assert!(!retryable(&LocalError::EngineUpdateRequired));
        assert!(!retryable(&LocalError::EngineInsecure {
            detail: String::new()
        }));
        assert!(!retryable(&LocalError::EngineCorrupt {
            detail: String::new()
        }));
        assert!(!retryable(&LocalError::EngineUnreadable {
            detail: String::new()
        }));
    }

    // --- live engine (this machine) ---------------------------------------------

    /// Counts `llama-server` processes on this machine.
    fn llama_server_process_count() -> usize {
        #[cfg(windows)]
        {
            let Ok(output) = std::process::Command::new("tasklist")
                .args(["/FI", "IMAGENAME eq llama-server.exe", "/NH"])
                .output()
            else {
                return 0;
            };
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| l.to_ascii_lowercase().contains("llama-server.exe"))
                .count()
        }
        #[cfg(not(windows))]
        {
            let Ok(output) = std::process::Command::new("pgrep")
                .args(["-c", "-f", "llama-server"])
                .output()
            else {
                return 0;
            };
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .unwrap_or(0)
        }
    }

    /// The whole supervisor against a real `llama-server` and a real model
    /// on a real GPU: cold start, admission budget, a chat through the
    /// production provider path, and a stop that leaves nothing behind.
    ///
    /// Run with the staged engine and model:
    /// ```text
    /// NEVOFLUX_LOCAL_LIVE_ENGINE_DIR=…/v0-experiment/engine/bin \
    /// NEVOFLUX_LOCAL_LIVE_MODEL=…/models/Qwen3-4B-Instruct-2507-Q4_K_M.gguf \
    /// cargo test -p nevoflux-daemon --lib -- --ignored live_engine
    /// ```
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "spawns a real llama-server and loads a multi-GB model onto the GPU; run explicitly"]
    async fn live_engine_cold_starts_serves_a_chat_and_leaves_nothing_running() {
        let Some((engine_dir, model_path)) = live_override() else {
            panic!(
                "set NEVOFLUX_LOCAL_LIVE_ENGINE_DIR and NEVOFLUX_LOCAL_LIVE_MODEL to the staged \
                 engine directory and model file"
            );
        };
        assert!(
            engine_dir.is_dir(),
            "{} is not a directory",
            engine_dir.display()
        );
        assert!(
            model_path.is_file(),
            "{} is not a file",
            model_path.display()
        );

        // Holds the endpoint registry for the whole test (R35) and clears it
        // afterwards, so a failure partway cannot leave a dead endpoint
        // published for every later test in this process.
        let _g = crate::local::endpoint::test_serial_async().await;
        struct EndpointGuard;
        impl Drop for EndpointGuard {
            fn drop(&mut self) {
                endpoint::publish(None);
            }
        }
        let _reset = EndpointGuard;

        // A temporary engine root: `engine.lock` and `engine.pid` go here,
        // never in the real user cache.
        let root = tempfile::tempdir().unwrap();
        let supervisor = EngineSupervisor::new(root.path().to_path_buf());

        let started = supervisor.ensure_started().await;
        // Whatever happens below, the process this test spawned must not
        // outlive it. Compares the full identity before killing, exactly as
        // production does.
        struct KillOnPanic(Option<EngineIdentity>);
        impl Drop for KillOnPanic {
            fn drop(&mut self) {
                let Some(recorded) = self.0.as_ref() else {
                    return;
                };
                if live_identity(recorded.pid).as_ref() == Some(recorded) {
                    eprintln!("live test: killing surviving engine pid {}", recorded.pid);
                    kill_pid(recorded.pid);
                }
            }
        }
        let _cleanup = KillOnPanic(supervisor.running_pid().and_then(live_identity));

        let endpoint_value = started.expect("the staged engine should cold start on this machine");
        println!("endpoint: {endpoint_value:#?}");
        println!("state: {:#?}", supervisor.state());

        let (backend, ctx) = match supervisor.state() {
            LocalState::Ready {
                backend,
                ctx,
                degraded,
                ref reason,
                ..
            } => {
                println!(
                    "ready: backend={backend:?} ctx={ctx} degraded={degraded} reason={reason:?}"
                );
                (backend, ctx)
            }
            other => panic!("expected Ready, got {other:?}"),
        };
        assert_eq!(
            backend,
            Backend::Cuda,
            "this machine is a Tesla T4 with a CUDA 13 driver; the supervisor must select CUDA"
        );
        assert_eq!(endpoint_value.n_ctx, ctx);

        // Diagnostics, not assertions: the hardened surface `self_check`
        // just passed, recorded with its real status codes so a live run
        // leaves evidence of what this engine build actually answers. The
        // `/props` n_ctx in particular is the number the admission budget is
        // cross-checked against — `--kv-unified` makes the budget the whole
        // shared pool (the launched ctx), never this value times `parallel`.
        let root_url = endpoint_value.base_url.trim_end_matches("/v1").to_string();
        if let Ok(client) = engine_http_client(Duration::from_secs(10)) {
            if let Ok(r) = client
                .get(format!("{root_url}/props"))
                .bearer_auth(&endpoint_value.api_key)
                .send()
                .await
            {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                println!(
                    "/props: n_ctx={:?} total_slots={:?}",
                    n_ctx_from_props(&body),
                    body.get("total_slots")
                );
            }
            if let Ok(r) = client.get(format!("{root_url}/v1/models")).send().await {
                println!("unauthenticated /v1/models: {}", r.status());
            }
            if let Ok(r) = client
                .get(format!("{root_url}/slots"))
                .bearer_auth(&endpoint_value.api_key)
                .send()
                .await
            {
                println!("/slots: {}", r.status());
            }
            if let Ok(r) = client
                .get(format!("{root_url}/"))
                .bearer_auth(&endpoint_value.api_key)
                .send()
                .await
            {
                println!(
                    "/: {} content-type={:?}",
                    r.status(),
                    r.headers().get(reqwest::header::CONTENT_TYPE)
                );
            }
        }

        // The admission budget must now be the engine's real context pool,
        // not the boot-time placeholder (`CTX_FLOOR`). A request larger than
        // the placeholder is the observable difference: it would have been
        // refused as `TooLarge` before `set_capacity_tokens` ran.
        assert!(
            ctx > CTX_FLOOR,
            "this assertion only means something if the launch ctx exceeds the placeholder"
        );
        let permit = admission()
            .acquire(
                crate::local::admission::Priority::Interactive,
                CTX_FLOOR + 1_000,
                true,
            )
            .await
            .expect("admission must have been given the engine's real capacity");
        drop(permit);

        // A real 16-token generation through the production provider path —
        // the same call the agent makes, not a hand-built request.
        let response = crate::wasm::llm::execute_llm_chat(
            nevoflux_llm::ProviderType::Local,
            "",
            &endpoint_value.model_id,
            crate::wasm::llm::LlmChatRequest {
                messages: vec![crate::wasm::llm::LlmMessage::user(
                    "Reply with the single word: ready",
                )],
                max_tokens: Some(16),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("a 16-token chat must succeed against the live engine");
        println!("chat response: {response:#?}");
        assert!(
            !response.content.trim().is_empty(),
            "the engine returned an empty completion"
        );

        supervisor.stop().await;
        assert_eq!(endpoint::current(), None);
        assert!(matches!(supervisor.state(), LocalState::Stopped { .. }));

        // The process is gone, and so is its pid file.
        assert!(!engine_pid_path(root.path()).exists());
        let survivors = llama_server_process_count();
        assert_eq!(
            survivors, 0,
            "stop() left {survivors} llama-server process(es) running"
        );
    }
}
