//! Knowledge Base install wizard (M4-2).
//!
//! Backend-driven, user-transparent install of `bun` + `gbrain`, then a
//! one-shot `init_brain` bootstrap. The browser drives the wizard by
//! calling these RPCs in order:
//!
//! 1. `kb.wizard.status`         — probe current install state
//! 2. `kb.wizard.install_bun`    — downloads + installs bun if missing
//! 3. `kb.wizard.install_gbrain` — `bun add github:garrytan/gbrain#<pinned>`
//! 4. `kb.wizard.init_brain`     — `gbrain init --pglite` + spawn supervisor
//!
//! Each step that does work emits frames on the EventBus topic
//! `system:kb-wizard:progress`:
//!
//! ```json
//! {
//!   "step": "install_bun",
//!   "status": "running" | "ok" | "failed" | "cancelled",
//!   "progress_pct": 0..=100,
//!   "log": "<stdout/stderr line>"
//! }
//! ```
//!
//! `kb.wizard.cancel` aborts any in-flight step.
//!
//! The wizard reuses M3-3 path-resolution conventions:
//!
//! - bun:    `which::which("bun")` → fallback to `~/.bun/bin/bun(.exe)`
//! - gbrain: `~/.nevoflux/brain-tool/node_modules/gbrain/src/cli.ts`
//! - brain:  gbrain ignores `--brain-dir` and reads `~/.gbrain` from the
//!           environment (spike S0 / 附录 B operational quirk #1)

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::event_bus::{types::PublisherIdentity, BusEvent, EventBus};

/// EventBus topic that wizard progress frames are published to. The
/// `system:` prefix is permitted because only the daemon (Internal)
/// publishes — see `event_bus::permissions::PermissionChecker`.
pub const PROGRESS_TOPIC: &str = "system:kb-wizard:progress";

/// Gbrain pin captured by spike S0 (semver 0.40.8.1).
pub const GBRAIN_PIN: &str = "github:garrytan/gbrain#af5ee1e";

/// Frame published on the [`PROGRESS_TOPIC`] EventBus channel.
#[derive(Debug, Clone, Serialize)]
pub struct WizardProgress {
    pub step: WizardStep,
    pub status: WizardStatus,
    /// 0..=100. Roughly indicative; exact values are step-specific.
    pub progress_pct: u8,
    /// One stdout/stderr line, or a human-readable status update.
    pub log: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WizardStep {
    DetectBun,
    InstallBun,
    InstallGbrain,
    InitBrain,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WizardStatus {
    Running,
    Ok,
    Failed,
    Cancelled,
}

/// Snapshot of install state, returned by `kb.wizard.status`.
#[derive(Debug, Clone, Serialize)]
pub struct WizardStatusReport {
    pub bun_installed: bool,
    pub bun_path: Option<PathBuf>,
    pub bun_version: Option<String>,
    pub gbrain_installed: bool,
    pub gbrain_cli_path: Option<PathBuf>,
    pub gbrain_version: Option<String>,
    /// `~/.gbrain/brain.pglite` exists.
    pub brain_initialized: bool,
    pub brain_dir: PathBuf,
    pub overall: WizardOverall,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WizardOverall {
    /// Everything installed + brain initialized.
    Ready,
    /// Bun and/or gbrain missing.
    NeedsInstall,
    /// Both installed but `brain.pglite` missing.
    NeedsInit,
    /// A wizard step is currently running.
    InProgress,
    /// Last attempt failed; user must retry.
    Failed,
}

/// Per-process state: tracks the currently running wizard task so
/// `kb.wizard.cancel` can abort it. Also stores the last failure-or-
/// progress overall state so a fresh `kb.wizard.status` call after an
/// abort can surface it.
pub struct WizardState {
    /// `Some(handle)` while a step is running; `None` otherwise.
    current_task: Mutex<Option<tokio::task::AbortHandle>>,
    /// Overlaid on top of [`status_probe`]'s computed `overall` when
    /// non-`Ready` and a step is currently in flight.
    last_overall: Mutex<Option<WizardOverall>>,
}

impl WizardState {
    pub fn new() -> Self {
        Self {
            current_task: Mutex::new(None),
            last_overall: Mutex::new(None),
        }
    }

    /// Set the active task handle; replaces any previous handle (the
    /// previous one is dropped, which on `AbortHandle` is harmless — it
    /// does NOT abort the previous task).
    pub async fn set_current(&self, handle: tokio::task::AbortHandle) {
        *self.current_task.lock().await = Some(handle);
    }

    /// Clear the active task handle without aborting.
    pub async fn clear_current(&self) {
        *self.current_task.lock().await = None;
    }

    /// Abort the active step (if any). Returns `true` if a task was
    /// aborted, `false` if there was nothing to abort.
    pub async fn cancel(&self) -> bool {
        let mut guard = self.current_task.lock().await;
        if let Some(handle) = guard.take() {
            handle.abort();
            true
        } else {
            false
        }
    }

    pub async fn set_overall(&self, overall: WizardOverall) {
        *self.last_overall.lock().await = Some(overall);
    }

    pub async fn clear_overall(&self) {
        *self.last_overall.lock().await = None;
    }

    pub async fn current_overall(&self) -> Option<WizardOverall> {
        *self.last_overall.lock().await
    }
}

impl Default for WizardState {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-global wizard state, set once at daemon startup.
pub static CURRENT_WIZARD_STATE: std::sync::OnceLock<Arc<WizardState>> =
    std::sync::OnceLock::new();

/// Process-global EventBus handle, set once at daemon startup by
/// `server.rs`. The wizard publishes progress frames through this; if
/// it is unset (e.g., in unit tests that don't boot the full daemon),
/// progress emission falls back to a tracing log line.
pub static CURRENT_EVENT_BUS: std::sync::OnceLock<Arc<EventBus>> =
    std::sync::OnceLock::new();

/// Process-global gateway snapshot, set once at daemon startup by
/// `server.rs` (or never, if `knowledge_base.enabled = false`). The
/// `kb.wizard.init_brain` step reads this to thread the gateway URL +
/// bearer token into gbrain as `OPENROUTER_*` env vars.
pub static CURRENT_GATEWAY_SNAPSHOT: std::sync::OnceLock<
    crate::llm_gateway::GatewayHandleSnapshot,
> = std::sync::OnceLock::new();

/// Path the daemon's TOML config is read from / persisted to.
///
/// `dirs::config_dir()` resolves to:
/// - Linux:   `~/.config/nevoflux/config.toml`
/// - macOS:   `~/Library/Application Support/nevoflux/config.toml`
/// - Windows: `%APPDATA%\nevoflux\config.toml`
///
/// Returns `None` only when `dirs::config_dir()` itself fails (extremely
/// unusual — a misconfigured CI box, for instance). All callers must
/// treat that as "cannot persist, log + continue".
fn config_toml_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("nevoflux").join("config.toml"))
}

/// Idempotently set `[knowledge_base.brain] enabled = true` in the given
/// config file, preserving existing comments / whitespace / formatting
/// via `toml_edit`. Creates the file (and parent dir) if missing.
///
/// Exposed for tests; the production wrapper [`persist_brain_enabled`]
/// resolves the platform-specific path and delegates here.
pub(crate) async fn persist_brain_enabled_at(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir config dir failed: {e}"))?;
        }
    }

    // Read existing content; an absent file or empty content is treated
    // as a fresh document so the wizard works on first install.
    let existing = tokio::fs::read_to_string(path).await.unwrap_or_default();
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("could not parse existing config.toml: {e}"))?;

    if !doc.contains_key("knowledge_base") {
        doc["knowledge_base"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let kb = doc["knowledge_base"]
        .as_table_mut()
        .ok_or_else(|| "knowledge_base is not a table".to_string())?;
    if !kb.contains_key("brain") {
        kb["brain"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let brain = kb["brain"]
        .as_table_mut()
        .ok_or_else(|| "knowledge_base.brain is not a table".to_string())?;
    brain["enabled"] = toml_edit::value(true);

    tokio::fs::write(path, doc.to_string())
        .await
        .map_err(|e| format!("write config.toml failed: {e}"))?;
    tracing::info!(
        path = %path.display(),
        "persisted knowledge_base.brain.enabled = true"
    );
    Ok(())
}

/// Production wrapper: persist `[knowledge_base.brain] enabled = true`
/// to the user's standard daemon config (see [`config_toml_path`]).
async fn persist_brain_enabled() -> Result<(), String> {
    let path = config_toml_path().ok_or_else(|| "could not resolve config dir".to_string())?;
    persist_brain_enabled_at(&path).await
}

/// Hot-reload the brain subsystem after a successful
/// `kb.wizard.init_brain`. The function:
///
/// 1. Persists `[knowledge_base.brain] enabled = true` to the user's
///    config.toml so the change survives daemon restarts.
/// 2. Spawns the gbrain supervisor + builds a fresh
///    [`crate::init_brain::BrainBoot`] via [`crate::init_brain::init_brain`].
/// 3. Installs the boot into the shared brain slot (registered at
///    daemon startup via [`set_current_brain_slot`]) so subsequent
///    `services.brain_supervisor()` / `Server::brain()` calls see the
///    live engine without a daemon restart.
///
/// Step 1 failure is logged but does NOT abort the in-memory install:
/// the user still gets a working brain for this session and we'll
/// surface the failure in logs so it can be fixed manually.
async fn hot_reload_brain() -> Result<(), String> {
    // 1. Persist config.
    if let Err(e) = persist_brain_enabled().await {
        tracing::warn!(
            error = %e,
            "persist brain config failed; proceeding with in-memory hot-reload \
             (user will need to re-enable in config.toml manually to survive daemon restart)"
        );
    }

    // 2. Build the minimum `KnowledgeBaseConfig` needed for init_brain.
    //    Both `enabled` flags true; everything else default so
    //    init_brain falls back to the canonical paths
    //    (`~/.bun/bin/bun`, `~/.nevoflux/brain-tool/...`, `~/.gbrain/`).
    let kb_config = crate::config::KnowledgeBaseConfig {
        enabled: true,
        gateway: crate::config::GatewayUpstreamConfig::default(),
        brain: crate::config::BrainConfig {
            enabled: true,
            ..Default::default()
        },
    };

    // 3. Resolve the gateway snapshot published at daemon startup.
    let gateway_snapshot = CURRENT_GATEWAY_SNAPSHOT
        .get()
        .cloned()
        .ok_or_else(|| "gateway snapshot unavailable; cannot hot-reload brain".to_string())?;

    // 4. Spawn the supervisor + build the engine.
    let boot = match crate::init_brain::init_brain(&kb_config, &Some(gateway_snapshot)).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            // init_brain returns Ok(None) when it intentionally skipped
            // (disabled flag, missing bun, missing cli.ts). We just set
            // enabled=true above; a None return here means bun or
            // gbrain cli isn't where we expect, which is a wizard bug.
            return Err(
                "init_brain returned Ok(None) despite enabled=true (bun or gbrain cli missing?)"
                    .into(),
            );
        }
        Err(e) => return Err(format!("init_brain failed: {e}")),
    };

    // 5. Install into the shared slot.
    let slot = crate::init_brain::CURRENT_BRAIN_SLOT
        .get()
        .ok_or_else(|| "brain slot not registered (daemon startup bug?)".to_string())?;
    let mut guard = slot.write().await;
    if let Some(old) = guard.take() {
        // Graceful shutdown of any previous supervisor — detached so the
        // wizard task doesn't block on subprocess teardown.
        let old_sup = old.supervisor.clone();
        tokio::spawn(async move {
            old_sup.shutdown().await;
        });
    }
    *guard = Some(crate::init_brain::BrainSlot {
        supervisor: boot.supervisor,
        engine: boot.engine,
    });

    tracing::info!("brain hot-reload OK: supervisor running, engine installed");
    Ok(())
}

/// Helper: return the (possibly initialized) global wizard state. If the
/// daemon never called `init_wizard_state`, a fresh `WizardState` is
/// returned — useful for tests.
pub fn current_wizard_state() -> Arc<WizardState> {
    CURRENT_WIZARD_STATE
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(WizardState::new()))
}

/// Fallback bun install path used when `which::which("bun")` misses.
/// Matches the location used by `bun.sh/install.{ps1,sh}`.
fn default_bun_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".bun").join("bin").join(if cfg!(windows) {
            "bun.exe"
        } else {
            "bun"
        })
    })
}

/// Resolve bun's binary path. Checks `which::which("bun")` first; falls
/// back to the platform-default install location if that exists.
pub fn resolve_bun_path() -> Option<PathBuf> {
    if let Ok(p) = which::which("bun") {
        return Some(p);
    }
    default_bun_path().filter(|p| p.exists())
}

/// Default location for the gbrain CLI inside `~/.nevoflux/brain-tool`.
/// Returns `None` only when `dirs::home_dir()` fails (effectively never
/// on a configured workstation).
pub fn default_gbrain_cli_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".nevoflux")
            .join("brain-tool")
            .join("node_modules")
            .join("gbrain")
            .join("src")
            .join("cli.ts")
    })
}

/// Probe install state. Pure read; does not mutate the filesystem.
///
/// `brain_dir` should be the same directory `init_brain` will eventually
/// pass to gbrain (typically `~/.gbrain` — see [`init_brain`] in
/// `init_brain.rs`).
pub async fn status_probe(brain_dir: &Path) -> WizardStatusReport {
    let bun_path = resolve_bun_path();
    let bun_installed = bun_path.is_some();
    let bun_version = if let Some(p) = &bun_path {
        Command::new(p)
            .arg("--version")
            .output()
            .await
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let gbrain_cli_path = default_gbrain_cli_path().filter(|p| p.exists());
    let gbrain_installed = gbrain_cli_path.is_some();
    // gbrain version: read package.json next to cli.ts
    //   .../node_modules/gbrain/src/cli.ts -> .../node_modules/gbrain/package.json
    let gbrain_version = gbrain_cli_path.as_ref().and_then(|p| {
        let pkg = p.parent()?.parent()?.join("package.json");
        let raw = std::fs::read_to_string(pkg).ok()?;
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        v.get("version")?.as_str().map(String::from)
    });

    let brain_pglite = brain_dir.join("brain.pglite");
    let brain_initialized = brain_pglite.exists();

    let computed_overall = if brain_initialized && bun_installed && gbrain_installed {
        WizardOverall::Ready
    } else if bun_installed && gbrain_installed {
        WizardOverall::NeedsInit
    } else {
        WizardOverall::NeedsInstall
    };

    // If a step is currently running, surface InProgress in preference
    // to the static probe result. If the wizard state recorded a Failed
    // overall after a previous step, surface that instead so the user
    // gets a chance to retry.
    let overall = match current_wizard_state().current_overall().await {
        Some(WizardOverall::InProgress) => WizardOverall::InProgress,
        Some(WizardOverall::Failed) => WizardOverall::Failed,
        _ => computed_overall,
    };

    WizardStatusReport {
        bun_installed,
        bun_path,
        bun_version,
        gbrain_installed,
        gbrain_cli_path,
        gbrain_version,
        brain_initialized,
        brain_dir: brain_dir.to_path_buf(),
        overall,
    }
}

/// Publish a wizard progress frame onto the EventBus, if a bus is
/// available. Errors are logged and swallowed — a flaky EventBus must
/// not break the install.
pub async fn publish_progress(bus: &Arc<EventBus>, frame: &WizardProgress) {
    let payload = match serde_json::to_value(frame) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize wizard progress frame");
            return;
        }
    };
    let event = BusEvent::ephemeral(PROGRESS_TOPIC, payload, PublisherIdentity::Internal);
    if let Err(e) = bus.publish(event).await {
        tracing::warn!(error = %e, topic = PROGRESS_TOPIC, "failed to publish wizard progress");
    }
}

/// Run a child process and emit its stdout/stderr lines as `Running`
/// progress frames via `emit`. Returns `Ok(())` on a successful exit
/// status; an `Err` if the process exited non-zero or IO failed.
async fn run_with_progress<F>(
    mut cmd: Command,
    step: WizardStep,
    emit: &F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(WizardProgress) + Send + Sync,
{
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;

    let mut stdout_reader = tokio::io::BufReader::new(stdout).lines();
    let mut stderr_reader = tokio::io::BufReader::new(stderr).lines();

    let mut stdout_done = false;
    let mut stderr_done = false;

    while !stdout_done || !stderr_done {
        tokio::select! {
            line = stdout_reader.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(text)) => emit(WizardProgress {
                        step,
                        status: WizardStatus::Running,
                        progress_pct: 50,
                        log: text,
                    }),
                    Ok(None) => stdout_done = true,
                    Err(e) => return Err(format!("stdout read: {e}").into()),
                }
            }
            line = stderr_reader.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(text)) => emit(WizardProgress {
                        step,
                        status: WizardStatus::Running,
                        progress_pct: 50,
                        log: text,
                    }),
                    Ok(None) => stderr_done = true,
                    Err(e) => return Err(format!("stderr read: {e}").into()),
                }
            }
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(format!("process exited with status {status:?}").into());
    }
    Ok(())
}

/// Install bun via the official installer:
///   - Windows: `powershell -NoProfile -Command "irm bun.sh/install.ps1 | iex"`
///   - Unix:    `sh -c "curl -fsSL https://bun.sh/install | bash"`
///
/// Returns the path to the installed `bun` binary on success.
pub async fn install_bun<F>(emit: &F) -> Result<PathBuf, String>
where
    F: Fn(WizardProgress) + Send + Sync,
{
    emit(WizardProgress {
        step: WizardStep::InstallBun,
        status: WizardStatus::Running,
        progress_pct: 0,
        log: "starting bun installer".into(),
    });

    let install_cmd: Command = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.args([
            "-NoProfile",
            "-Command",
            "irm bun.sh/install.ps1 | iex",
        ]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", "curl -fsSL https://bun.sh/install | bash"]);
        c
    };

    match run_with_progress(install_cmd, WizardStep::InstallBun, emit).await {
        Ok(()) => {
            // Verify the binary now resolves
            if let Some(p) = resolve_bun_path() {
                emit(WizardProgress {
                    step: WizardStep::InstallBun,
                    status: WizardStatus::Ok,
                    progress_pct: 100,
                    log: format!("bun installed at {}", p.display()),
                });
                Ok(p)
            } else {
                let msg = "bun install reported success but binary not found";
                emit(WizardProgress {
                    step: WizardStep::InstallBun,
                    status: WizardStatus::Failed,
                    progress_pct: 100,
                    log: msg.into(),
                });
                Err(msg.into())
            }
        }
        Err(e) => {
            let msg = format!("bun install failed: {e}");
            emit(WizardProgress {
                step: WizardStep::InstallBun,
                status: WizardStatus::Failed,
                progress_pct: 100,
                log: msg.clone(),
            });
            Err(msg)
        }
    }
}

/// Run `bun add github:garrytan/gbrain#<pin>` inside
/// `~/.nevoflux/brain-tool`. Returns the path to the installed
/// `cli.ts` on success.
pub async fn install_gbrain<F>(bun_path: &Path, emit: &F) -> Result<PathBuf, String>
where
    F: Fn(WizardProgress) + Send + Sync,
{
    emit(WizardProgress {
        step: WizardStep::InstallGbrain,
        status: WizardStatus::Running,
        progress_pct: 0,
        log: "starting gbrain install (may take 60-180s)".into(),
    });

    let install_dir = dirs::home_dir()
        .ok_or_else(|| "no home dir".to_string())?
        .join(".nevoflux")
        .join("brain-tool");
    std::fs::create_dir_all(&install_dir)
        .map_err(|e| format!("mkdir brain-tool failed: {e}"))?;

    // 1. bun init -y  (creates package.json so `bun add` has somewhere
    //    to record the dependency)
    let mut init_cmd = Command::new(bun_path);
    init_cmd.args(["init", "-y"]).current_dir(&install_dir);
    run_with_progress(init_cmd, WizardStep::InstallGbrain, emit)
        .await
        .map_err(|e| format!("bun init failed: {e}"))?;

    // 2. bun add github:garrytan/gbrain#<pinned>
    let mut add_cmd = Command::new(bun_path);
    add_cmd
        .args(["add", GBRAIN_PIN])
        .current_dir(&install_dir);
    run_with_progress(add_cmd, WizardStep::InstallGbrain, emit)
        .await
        .map_err(|e| format!("bun add gbrain failed: {e}"))?;

    let cli_path = install_dir
        .join("node_modules")
        .join("gbrain")
        .join("src")
        .join("cli.ts");
    if !cli_path.exists() {
        let msg = format!(
            "gbrain cli.ts not at expected path: {}",
            cli_path.display()
        );
        emit(WizardProgress {
            step: WizardStep::InstallGbrain,
            status: WizardStatus::Failed,
            progress_pct: 100,
            log: msg.clone(),
        });
        return Err(msg);
    }

    emit(WizardProgress {
        step: WizardStep::InstallGbrain,
        status: WizardStatus::Ok,
        progress_pct: 100,
        log: format!("gbrain installed at {}", cli_path.display()),
    });
    Ok(cli_path)
}

/// Run `gbrain init --pglite --embedding-dimensions 512 \
///         --embedding-model openrouter:claude-haiku-4-5`.
///
/// `gateway_url` + `gateway_token` are wired in as `OPENROUTER_*` env
/// vars so the gbrain init step can call the upstream LLM through the
/// daemon's in-process gateway (附录 B operational quirk #2).
///
/// `GBRAIN_BRAIN_DIR` is also set so the initialization writes
/// `brain.pglite` into the daemon's configured location rather than
/// gbrain's hard-coded `~/.gbrain` (附录 B operational quirk #1).
pub async fn init_brain_repo<F>(
    bun_path: &Path,
    cli_path: &Path,
    brain_dir: &Path,
    gateway_url: &str,
    gateway_token: &str,
    emit: &F,
) -> Result<(), String>
where
    F: Fn(WizardProgress) + Send + Sync,
{
    emit(WizardProgress {
        step: WizardStep::InitBrain,
        status: WizardStatus::Running,
        progress_pct: 0,
        log: "gbrain init --pglite (one-time setup)".into(),
    });

    std::fs::create_dir_all(brain_dir)
        .map_err(|e| format!("mkdir brain_dir failed: {e}"))?;

    let mut cmd = Command::new(bun_path);
    cmd.arg("run")
        .arg(cli_path)
        .arg("init")
        .arg("--pglite")
        .arg("--embedding-dimensions")
        .arg("512")
        .arg("--embedding-model")
        .arg("openrouter:claude-haiku-4-5")
        .env("OPENROUTER_BASE_URL", gateway_url)
        .env("OPENROUTER_API_KEY", gateway_token)
        .env("GBRAIN_BRAIN_DIR", brain_dir)
        .current_dir(brain_dir);

    run_with_progress(cmd, WizardStep::InitBrain, emit)
        .await
        .map_err(|e| format!("gbrain init failed: {e}"))?;

    let pglite = brain_dir.join("brain.pglite");
    if !pglite.exists() {
        let msg = format!(
            "brain.pglite not at expected path: {}",
            pglite.display()
        );
        emit(WizardProgress {
            step: WizardStep::InitBrain,
            status: WizardStatus::Failed,
            progress_pct: 100,
            log: msg.clone(),
        });
        return Err(msg);
    }

    emit(WizardProgress {
        step: WizardStep::InitBrain,
        status: WizardStatus::Ok,
        progress_pct: 100,
        log: format!("brain initialized at {}", pglite.display()),
    });
    Ok(())
}

/// Build the canonical `~/.gbrain` directory path. Returns `.gbrain`
/// in the cwd as a last-ditch fallback when `dirs::home_dir()` is
/// unavailable.
pub fn default_brain_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gbrain")
}

/// Spawn a wizard step on a tokio task so it runs in the background
/// while the RPC returns immediately. The returned future resolves
/// once the task has been spawned; the actual step continues in the
/// background, publishing progress on the EventBus.
///
/// Registers the abort handle with [`WizardState`] so
/// `kb.wizard.cancel` can interrupt it.
pub async fn spawn_step<F, Fut>(state: Arc<WizardState>, bus: Arc<EventBus>, work: F)
where
    F: FnOnce(Arc<EventBus>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    state.set_overall(WizardOverall::InProgress).await;
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        work(bus).await;
        // After the work future returns, clear the active-task slot.
        // (cancel() can race with this; that's harmless — both branches
        // just set the slot to None.)
        state_clone.clear_current().await;
    });
    state.set_current(handle.abort_handle()).await;
}

// ── RPC dispatch glue ────────────────────────────────────────────────
//
// These helpers are called from `server.rs`'s `system_command` /
// `agent:command` dispatcher. Each returns a fully-formed
// `system_response` envelope so the dispatcher only has to forward it.

/// Build a `system_response` success envelope for command `cmd`.
fn ok_response(request_id: &str, cmd: &str, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": cmd,
            "success": true,
            "data": data,
        }
    })
}

/// Build a `system_response` error envelope for command `cmd`.
fn err_response(
    request_id: &str,
    cmd: &str,
    code: &str,
    message: impl Into<String>,
) -> serde_json::Value {
    serde_json::json!({
        "type": "system_response",
        "payload": {
            "request_id": request_id,
            "command": cmd,
            "success": false,
            "error": {
                "code": code,
                "message": message.into(),
            }
        }
    })
}

/// Handle `kb.wizard.status`. Pure read; no side effects.
pub async fn handle_status(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let brain_dir = default_brain_dir();
    let report = status_probe(&brain_dir).await;
    match serde_json::to_value(&report) {
        Ok(data) => ok_response(request_id, "kb.wizard.status", data),
        Err(e) => err_response(
            request_id,
            "kb.wizard.status",
            "SERIALIZE_ERROR",
            format!("failed to serialize status report: {e}"),
        ),
    }
}

/// Build an emit closure that publishes wizard progress frames onto the
/// process-global EventBus (if set). Falls back to a tracing log line.
fn make_emit_for_global() -> impl Fn(WizardProgress) + Send + Sync + 'static {
    let bus = CURRENT_EVENT_BUS.get().cloned();
    move |frame: WizardProgress| {
        if let Some(bus) = bus.as_ref() {
            // We can't await in a sync closure — spawn a tiny task. The
            // EventBus's `publish` is cheap (mostly in-memory delivery).
            let bus = bus.clone();
            tokio::spawn(async move {
                publish_progress(&bus, &frame).await;
            });
        } else {
            tracing::info!(
                step = ?frame.step,
                status = ?frame.status,
                progress_pct = frame.progress_pct,
                log = %frame.log,
                "kb-wizard progress (no EventBus bound)",
            );
        }
    }
}

/// Handle `kb.wizard.install_bun`. Spawns the install in the background
/// and returns immediately so the caller can subscribe to
/// `system:kb-wizard:progress` for live updates.
pub async fn handle_install_bun(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let state = current_wizard_state();
    let bus = match CURRENT_EVENT_BUS.get().cloned() {
        Some(b) => b,
        None => {
            return err_response(
                &request_id,
                "kb.wizard.install_bun",
                "NO_EVENT_BUS",
                "EventBus not initialized; cannot stream progress",
            );
        }
    };

    spawn_step(state.clone(), bus, move |_bus| async move {
        let emit = make_emit_for_global();
        match install_bun(&emit).await {
            Ok(_) => {
                // Clear last_overall so subsequent kb.wizard.status calls
                // fall through to the disk-derived computed_overall. Setting
                // it via status_probe() would re-read last_overall (still
                // InProgress here) and stick at InProgress forever.
                state.clear_overall().await;
            }
            Err(_) => {
                state.set_overall(WizardOverall::Failed).await;
            }
        }
    })
    .await;

    ok_response(
        &request_id,
        "kb.wizard.install_bun",
        serde_json::json!({ "started": true }),
    )
}

/// Handle `kb.wizard.install_gbrain`. Resolves bun_path on the fly.
pub async fn handle_install_gbrain(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let bun_path = match resolve_bun_path() {
        Some(p) => p,
        None => {
            return err_response(
                &request_id,
                "kb.wizard.install_gbrain",
                "BUN_NOT_FOUND",
                "bun is not installed; run kb.wizard.install_bun first",
            );
        }
    };
    let state = current_wizard_state();
    let bus = match CURRENT_EVENT_BUS.get().cloned() {
        Some(b) => b,
        None => {
            return err_response(
                &request_id,
                "kb.wizard.install_gbrain",
                "NO_EVENT_BUS",
                "EventBus not initialized; cannot stream progress",
            );
        }
    };

    spawn_step(state.clone(), bus, move |_bus| async move {
        let emit = make_emit_for_global();
        match install_gbrain(&bun_path, &emit).await {
            Ok(_) => {
                // Clear last_overall (see install_bun for rationale).
                state.clear_overall().await;
            }
            Err(_) => {
                state.set_overall(WizardOverall::Failed).await;
            }
        }
    })
    .await;

    ok_response(
        &request_id,
        "kb.wizard.install_gbrain",
        serde_json::json!({ "started": true }),
    )
}

/// Handle `kb.wizard.init_brain`. Resolves bun + cli + gateway snapshot.
pub async fn handle_init_brain(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let bun_path = match resolve_bun_path() {
        Some(p) => p,
        None => {
            return err_response(
                &request_id,
                "kb.wizard.init_brain",
                "BUN_NOT_FOUND",
                "bun is not installed; run kb.wizard.install_bun first",
            );
        }
    };
    let cli_path = match default_gbrain_cli_path().filter(|p| p.exists()) {
        Some(p) => p,
        None => {
            return err_response(
                &request_id,
                "kb.wizard.init_brain",
                "GBRAIN_NOT_FOUND",
                "gbrain cli.ts not found; run kb.wizard.install_gbrain first",
            );
        }
    };
    let gateway = match CURRENT_GATEWAY_SNAPSHOT.get().cloned() {
        Some(g) => g,
        None => {
            return err_response(
                &request_id,
                "kb.wizard.init_brain",
                "GATEWAY_NOT_RUNNING",
                "in-process llm-gateway is not running; enable knowledge_base in config and restart the daemon",
            );
        }
    };
    let state = current_wizard_state();
    let bus = match CURRENT_EVENT_BUS.get().cloned() {
        Some(b) => b,
        None => {
            return err_response(
                &request_id,
                "kb.wizard.init_brain",
                "NO_EVENT_BUS",
                "EventBus not initialized; cannot stream progress",
            );
        }
    };

    let brain_dir = default_brain_dir();
    spawn_step(state.clone(), bus, move |_bus| async move {
        let emit = make_emit_for_global();
        match init_brain_repo(
            &bun_path,
            &cli_path,
            &brain_dir,
            &gateway.url,
            &gateway.bearer_token,
            &emit,
        )
        .await
        {
            Ok(()) => {
                // Clear last_overall (see install_bun for rationale).
                state.clear_overall().await;
                // M4-2.5: persist `[knowledge_base.brain] enabled = true`
                // AND bring the brain online without requiring a daemon
                // restart. A hot-reload failure is non-fatal here: the
                // disk artifacts are sound (badge will show Ready),
                // worst case the user has to restart the daemon to use
                // brain tools.
                if let Err(e) = hot_reload_brain().await {
                    tracing::error!(
                        error = %e,
                        "brain hot-reload failed; wizard succeeded but agent \
                         cannot query brain until daemon restart"
                    );
                }
            }
            Err(_) => {
                state.set_overall(WizardOverall::Failed).await;
            }
        }
    })
    .await;

    ok_response(
        &request_id,
        "kb.wizard.init_brain",
        serde_json::json!({ "started": true }),
    )
}

/// Handle `kb.wizard.cancel`. Aborts any in-flight wizard step and emits
/// a `Cancelled` progress frame so subscribers know to stop waiting.
pub async fn handle_cancel(params: &serde_json::Value) -> serde_json::Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let state = current_wizard_state();
    let cancelled = state.cancel().await;
    if cancelled {
        // Best-effort: surface a Cancelled frame so the UI can stop the
        // progress spinner without waiting for the next probe.
        if let Some(bus) = CURRENT_EVENT_BUS.get() {
            let frame = WizardProgress {
                step: WizardStep::InstallBun, // step name is informational; UI
                status: WizardStatus::Cancelled, // can reset all step states.
                progress_pct: 0,
                log: "wizard step cancelled".into(),
            };
            publish_progress(bus, &frame).await;
        }
        // Clear the in-progress overall flag so the next status probe
        // surfaces the real install state.
        state.clear_overall().await;
    }
    ok_response(
        request_id,
        "kb.wizard.cancel",
        serde_json::json!({ "cancelled": cancelled }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::tempdir;

    #[tokio::test]
    async fn status_probe_returns_not_initialized_for_empty_brain_dir() {
        let tmp = tempdir().unwrap();
        let report = status_probe(tmp.path()).await;
        // Bun MIGHT be in PATH on dev machines — don't assert
        // bun_installed is false. The temp dir is empty so the brain
        // pglite cannot exist.
        assert!(
            !report.brain_initialized,
            "fresh tmp dir must report brain_initialized=false; got {report:?}"
        );
        assert_eq!(report.brain_dir, tmp.path());
    }

    #[tokio::test]
    async fn wizard_progress_serializes_to_expected_shape() {
        let p = WizardProgress {
            step: WizardStep::InstallBun,
            status: WizardStatus::Running,
            progress_pct: 42,
            log: "downloading".into(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["step"], "install_bun");
        assert_eq!(json["status"], "running");
        assert_eq!(json["progress_pct"], 42);
        assert_eq!(json["log"], "downloading");
    }

    #[test]
    fn wizard_overall_serializes_snake_case() {
        let v = serde_json::to_value(WizardOverall::NeedsInstall).unwrap();
        assert_eq!(v, serde_json::json!("needs_install"));
        let v2 = serde_json::to_value(WizardOverall::InProgress).unwrap();
        assert_eq!(v2, serde_json::json!("in_progress"));
        let v3 = serde_json::to_value(WizardOverall::Ready).unwrap();
        assert_eq!(v3, serde_json::json!("ready"));
    }

    #[test]
    fn wizard_step_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(WizardStep::DetectBun).unwrap(),
            serde_json::json!("detect_bun")
        );
        assert_eq!(
            serde_json::to_value(WizardStep::InstallGbrain).unwrap(),
            serde_json::json!("install_gbrain")
        );
        assert_eq!(
            serde_json::to_value(WizardStep::InitBrain).unwrap(),
            serde_json::json!("init_brain")
        );
    }

    #[test]
    fn wizard_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(WizardStatus::Running).unwrap(),
            serde_json::json!("running")
        );
        assert_eq!(
            serde_json::to_value(WizardStatus::Cancelled).unwrap(),
            serde_json::json!("cancelled")
        );
    }

    #[tokio::test]
    async fn run_with_progress_streams_output() {
        // `echo hello` should succeed on both Windows (cmd /c) and Unix
        // (sh -c) and emit at least one frame containing 'hello'.
        let frames: Arc<StdMutex<Vec<WizardProgress>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let frames_clone = frames.clone();
        let emit = move |p: WizardProgress| {
            frames_clone.lock().unwrap().push(p);
        };

        let cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "echo hello"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "echo hello"]);
            c
        };

        let result = run_with_progress(cmd, WizardStep::DetectBun, &emit).await;
        assert!(result.is_ok(), "echo hello should succeed: {result:?}");
        let frames = frames.lock().unwrap();
        assert!(
            frames.iter().any(|f| f.log.contains("hello")),
            "expected a frame containing 'hello'; got {:?}",
            *frames
        );
        assert!(
            frames.iter().all(|f| f.status == WizardStatus::Running),
            "all in-progress frames must have status=Running"
        );
    }

    #[tokio::test]
    async fn run_with_progress_returns_err_on_nonzero_exit() {
        let frames: Arc<StdMutex<Vec<WizardProgress>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let frames_clone = frames.clone();
        let emit = move |p: WizardProgress| {
            frames_clone.lock().unwrap().push(p);
        };

        let cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "exit 1"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "exit 1"]);
            c
        };

        let result = run_with_progress(cmd, WizardStep::DetectBun, &emit).await;
        assert!(
            result.is_err(),
            "non-zero exit must return Err; got {result:?}"
        );
    }

    #[tokio::test]
    async fn wizard_state_cancel_reports_active() {
        let state = WizardState::new();
        // No active task -> cancel returns false
        assert!(!state.cancel().await);

        // Spawn a long task and set its handle
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        state.set_current(handle.abort_handle()).await;
        assert!(state.cancel().await);
        // A second cancel finds no task
        assert!(!state.cancel().await);
    }

    #[tokio::test]
    async fn default_brain_dir_is_under_home_or_cwd() {
        let p = default_brain_dir();
        assert!(p.ends_with(".gbrain"));
    }

    // ── M4-2.5: persist_brain_enabled_at ─────────────────────────────
    //
    // These exercise the format-preserving config writer used by the
    // wizard's hot-reload path. We don't unit-test `hot_reload_brain`
    // itself because it requires a live bun + gbrain install; the
    // existing init_brain tests cover those code paths, and the
    // end-to-end install wizard flow is the integration proof.

    #[tokio::test]
    async fn persist_brain_enabled_creates_section_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nevoflux").join("config.toml");
        persist_brain_enabled_at(&path)
            .await
            .expect("persist should succeed against a fresh path");

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            content.contains("[knowledge_base.brain]"),
            "missing [knowledge_base.brain] header; got:\n{content}"
        );
        assert!(
            content.contains("enabled = true"),
            "missing enabled = true; got:\n{content}"
        );
    }

    #[tokio::test]
    async fn persist_brain_enabled_preserves_existing_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nevoflux").join("config.toml");
        tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let existing = "\
# user comment about daemon
[daemon]
port_range_start = 19500

[llm.openai]
api_key = \"user-key\"
";
        tokio::fs::write(&path, existing).await.unwrap();

        persist_brain_enabled_at(&path)
            .await
            .expect("persist should succeed against an existing config");

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            content.contains("# user comment about daemon"),
            "user comment was lost; got:\n{content}"
        );
        assert!(
            content.contains("api_key = \"user-key\""),
            "existing llm.openai.api_key was lost; got:\n{content}"
        );
        assert!(
            content.contains("port_range_start = 19500"),
            "existing daemon.port_range_start was lost; got:\n{content}"
        );
        assert!(
            content.contains("[knowledge_base.brain]"),
            "[knowledge_base.brain] header was not appended; got:\n{content}"
        );
        assert!(
            content.contains("enabled = true"),
            "enabled = true was not written; got:\n{content}"
        );
    }

    #[tokio::test]
    async fn persist_brain_enabled_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_brain_enabled_at(&path).await.unwrap();
        let first = tokio::fs::read_to_string(&path).await.unwrap();
        persist_brain_enabled_at(&path).await.unwrap();
        let second = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            first, second,
            "second persist should be a no-op; first:\n{first}\n\nsecond:\n{second}"
        );
    }

    #[tokio::test]
    async fn persist_brain_enabled_flips_existing_false_to_true() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "\
[knowledge_base.brain]
enabled = false
bun_path = \"/some/custom/bun\"
";
        tokio::fs::write(&path, existing).await.unwrap();

        persist_brain_enabled_at(&path).await.unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            content.contains("enabled = true"),
            "false was not flipped to true; got:\n{content}"
        );
        assert!(
            !content.contains("enabled = false"),
            "stale enabled = false still present; got:\n{content}"
        );
        assert!(
            content.contains("bun_path = \"/some/custom/bun\""),
            "sibling key bun_path was lost; got:\n{content}"
        );
    }
}
