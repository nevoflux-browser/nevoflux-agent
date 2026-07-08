//! Single-run executor for schedules (P1 scope).
//!
//! [`execute_run`] performs exactly ONE run of a schedule: it records the
//! `running` row, emits `run_start`, builds the `<SCHEDULE-CONTEXT>`-prefixed
//! user message, invokes the shared unattended agent kernel
//! ([`crate::agent_exec::run_agent_once`]), records `run_end`, emits `run_end`,
//! and returns a [`RunResult`]. It deliberately owns *no* schedule-level
//! bookkeeping: next-fire recomputation, status transitions, failure counting,
//! and `last_run_status` all live in `manager::ScheduleManager::finish_fire`.
//!
//! ## Stub path (unit tests)
//!
//! When `services` is `None` the run short-circuits to an immediate `ok` with
//! `final_text = None` — mirroring `loops::executor`'s stub — so the manager's
//! due-tick, boot-rearm and counter logic are testable without an LLM.
//!
//! ## Browser policy (P4)
//!
//! The pure decision core [`plan_browser`] maps `(policy, on_unavailable,
//! browser_available, env_bin_set)` onto a [`BrowserPlan`]; `execute_run`
//! dispatches on it BEFORE the stub short-circuit (so defer/skip verdicts are
//! recorded even without an LLM):
//!
//! - `none`  → `borrow_proxy = false` AND the `browser_*` / `computer_*` tools
//!   are stripped from the run's allowlist via `forbidden_prefixes` (the stored
//!   mode's non-browser tools stay available).
//! - `live`  → at fire time, `CURRENT_BROWSER_REGISTRY.any()`. Present ⇒
//!   `borrow_proxy = true` (borrow the creator session's most recent sidebar
//!   proxy — the P1 path, unchanged). Absent ⇒ `on_unavailable`: `skip` records
//!   the run `Skipped`; `defer` (the default) records it `Deferred` and parks
//!   the schedule id in the manager's deferred set for a coalesced re-fire once
//!   a browser returns. Neither bumps `consecutive_failures`.
//! - `headless` → guarded by `NEVOFLUX_BROWSER_BIN` (unset ⇒ run `Error`). Per
//!   run: clone a base profile, inject the automation pref, hold the global
//!   [`headless_launch_lock`] across `proxy_ids()` snapshot → spawn →
//!   `wait_for_new_browser`, bind the new instance, run the kernel with
//!   `bound_browser: Some(entry)`, and ALWAYS tear the browser + clone down on
//!   every exit path.
//!
//! ## Token budget + goal loop (P3)
//!
//! A `max_tokens_per_run` builds a shared [`TokenBudget`] threaded through the
//! LLM boundary for *every* run (plain or goal-wrapped); the spent total lands
//! in the run row's `tokens_used`.
//!
//! When `goal_condition` is set, the run becomes a **goal loop**: each turn
//! runs the kernel, the accumulated `(user, assistant)` transcript is judged by
//! the evaluator (reusing `goals::evaluator`), and the pure decision core
//! [`next_goal_step`] decides met → `Ok`, exhausted turns / budget → `Error`,
//! else a `<GOAL-CONTINUATION>` message for the next turn. The loop's control
//! flow is isolated behind the [`GoalTurnDriver`] seam so it is unit-testable
//! without a network (see the tests).

use crate::agent_exec::{run_agent_once, AgentExecRequest, TokenBudget};
use crate::browser_launch::{
    kill_profile_processes, spawn_and_supervise_excluding, BrowserLaunchConfig,
};
use crate::goals::evaluator::{evaluate, resolve_evaluator, EvaluatorChoice, Verdict};
use crate::profile::ProfileManager;
use crate::registry::{BrowserEntry, BrowserRegistry};
use crate::schedules::events::ScheduleEvents;
use crate::wasm::services::HostServices;
use nevoflux_storage::models::current_timestamp;
use nevoflux_storage::models::schedule::{ScheduleRecord, ScheduleRunStatus};
use nevoflux_storage::repositories::ScheduleRepository;
use nevoflux_storage::Database;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// Default per-run turn budget for a goal-wrapped schedule when
/// `goal_max_turns` is unset (or non-positive). Mirrors the goals engine.
const DEFAULT_GOAL_MAX_TURNS: i64 = 20;

/// Env var naming the nevoflux (Gecko fork) binary a headless-policy run
/// launches. Required for the `headless` policy — unset ⇒ the run errors.
const HEADLESS_BROWSER_BIN_ENV: &str = "NEVOFLUX_BROWSER_BIN";

/// Readiness barrier for a spawned headless browser: how long to wait for its
/// extension to auto-connect + register before the run fails.
const HEADLESS_REGISTER_TIMEOUT: Duration = Duration::from_secs(60);

/// Tool-name prefixes stripped from a `none`-policy run's allowlist: a run with
/// no browser must not see `browser_*` / `computer_*` tools (they would hang on
/// a routing target that does not exist). The stored mode's other tools stay.
fn none_policy_forbidden_prefixes() -> Vec<String> {
    vec!["browser_".to_string(), "computer_".to_string()]
}

/// Process-global serialization lock for headless launches. Two schedules
/// launching a headless browser concurrently would each snapshot the other's
/// registration as part of its `exclude` set — or, worse, cross-bind the
/// other's just-registered instance as "new". Holding this from the
/// `proxy_ids()` snapshot through `wait_for_new_browser` completion makes each
/// launch see a stable before/after picture. At the ≥1h schedule cadence
/// contention is negligible.
fn headless_launch_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Resolve the base-profiles directory: `NEVOFLUX_BASE_PROFILES` if set, else
/// `<config_dir>/base-profiles` (config_dir resolved the same way the daemon
/// loads config — see `crate::paths`). A missing base yields an empty clone
/// (documented `ProfileManager` behavior — the run proceeds without login state).
fn resolve_base_profiles_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("NEVOFLUX_BASE_PROFILES") {
        return PathBuf::from(v);
    }
    crate::config::AgentConfig::default_config_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("base-profiles")
}

/// Resolve the ephemeral clone work directory: `NEVOFLUX_PROFILE_WORK` if set,
/// else `$TMPDIR/nevoflux-profiles`.
fn resolve_profile_work_dir() -> PathBuf {
    std::env::var_os("NEVOFLUX_PROFILE_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("nevoflux-profiles"))
}

/// Browser routing for one run: whether to borrow the session's sidebar proxy
/// (`live`) and/or route to an explicitly-bound headless browser. Threaded
/// through both the plain and goal-wrapped execution paths.
#[derive(Clone, Default)]
struct BrowserRouting {
    /// Borrow the session's most-recent sidebar proxy (the `live` P1 path).
    borrow_proxy: bool,
    /// Route `browser_*` tools to this explicitly-bound browser (headless).
    bound_browser: Option<BrowserEntry>,
}

/// The pure browser-policy decision core. Maps a schedule's `(policy,
/// on_unavailable)` plus the fire-time facts (`browser_available` from the
/// registry, `env_bin_set` for headless) onto the plan `execute_run` dispatches
/// on. Total + side-effect free, so it is unit-tested exhaustively.
///
/// `browser_available` is only consulted for `live`; `env_bin_set` only for
/// `headless`. An unrecognized policy is a defensive `Error` (create-time
/// validation should already have rejected it).
pub(crate) fn plan_browser(
    policy: &str,
    on_unavailable: Option<&str>,
    browser_available: bool,
    env_bin_set: bool,
) -> BrowserPlan {
    match policy {
        "none" => BrowserPlan::UseNone,
        "live" => {
            if browser_available {
                BrowserPlan::UseLive
            } else if matches!(on_unavailable, Some("skip")) {
                BrowserPlan::Skip
            } else {
                // Default (None) and explicit "defer" both defer.
                BrowserPlan::Defer
            }
        }
        "headless" => {
            if env_bin_set {
                BrowserPlan::LaunchHeadless
            } else {
                BrowserPlan::Error(format!(
                    "headless browser policy requires the {HEADLESS_BROWSER_BIN_ENV} env var to be set"
                ))
            }
        }
        other => BrowserPlan::Error(format!("invalid browser_policy: {other}")),
    }
}

/// Outcome of [`plan_browser`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BrowserPlan {
    /// Run with no browser; strip `browser_*` / `computer_*` from the allowlist.
    UseNone,
    /// Run with the live sidebar-proxy borrow (a browser is available).
    UseLive,
    /// No live browser + `on_unavailable=defer`: record `Deferred`, park for
    /// a coalesced re-fire when a browser returns.
    Defer,
    /// No live browser + `on_unavailable=skip`: record `Skipped`.
    Skip,
    /// Launch + bind a headless browser for the duration of the run.
    LaunchHeadless,
    /// The policy cannot run as configured (headless without the env var, or an
    /// invalid policy string) — record `Error` with this message.
    Error(String),
}

/// Outcome of a single schedule run, consumed by
/// `manager::ScheduleManager::finish_fire` to apply next-fire/status
/// bookkeeping. The run row itself is already persisted (via `record_run_end`)
/// by the time this is returned.
#[derive(Debug)]
pub struct RunResult {
    /// `schedule_runs` row id (0 if the run could not even be recorded).
    pub run_id: i64,
    /// True on a successful agent turn (or the stub path).
    pub ok: bool,
    /// The persisted run status (`Ok` / `Error` / `Skipped` / `Deferred`).
    pub status: ScheduleRunStatus,
    /// Short error string when `ok == false`.
    pub error: Option<String>,
    /// Final assistant text on success (always `None` on the stub path).
    pub final_text: Option<String>,
}

/// Tools a scheduled run must never call: interactive prompts (there is no
/// user to answer), recursive job creation (a scheduled job spawning more
/// jobs/loops is out of P1 scope), and `goal_set` — a scheduled (unattended)
/// run must not hijack the creator session's active goal, since goals are
/// session-scoped and single-active.
fn forbidden_tools() -> Vec<String> {
    vec![
        "ask_user".to_string(),
        "browser_ask_user".to_string(),
        "loop_create".to_string(),
        "schedule_create".to_string(),
        "goal_set".to_string(),
    ]
}

/// Execute one run of `rec` with the given `fire_kind` (`scheduled` | `manual`
/// | `catchup`). `registry` is the browser registry the fire-time policy
/// decision consults (the manager threads its injected-or-global registry
/// here); `None` means no registry is available (live ⇒ unavailable, headless
/// ⇒ error). See the module docs for the policy dispatch and the stub split.
pub async fn execute_run(
    services: &Option<HostServices>,
    events: &ScheduleEvents,
    db: &Database,
    rec: &ScheduleRecord,
    fire_kind: &str,
    registry: Option<Arc<BrowserRegistry>>,
) -> RunResult {
    let repo = ScheduleRepository::new(db);
    let start = current_timestamp();

    let run_id = match repo.record_run_start(&rec.id, start, fire_kind) {
        Ok(id) => id,
        Err(e) => {
            // Could not even open a run row — surface as an error result so the
            // manager still applies failure bookkeeping. No run_end event
            // because there is no run row to reference.
            return RunResult {
                run_id: 0,
                ok: false,
                status: ScheduleRunStatus::Error,
                error: Some(e.to_string()),
                final_text: None,
            };
        }
    };
    events
        .run_start(&rec.id, &rec.name, run_id, fire_kind, start)
        .await;

    // Browser policy decision — BEFORE the stub short-circuit, so a defer/skip
    // verdict is recorded even on the (LLM-less) stub path used by the
    // manager's coalesce tests.
    let browser_available = registry.as_ref().and_then(|r| r.any()).is_some();
    let env_bin_set = std::env::var_os(HEADLESS_BROWSER_BIN_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let plan = plan_browser(
        &rec.browser_policy,
        rec.on_unavailable.as_deref(),
        browser_available,
        env_bin_set,
    );

    // Non-executing verdicts: record the terminal status + return. None of
    // these bump `consecutive_failures` in the manager (skip/defer are waiting
    // states, not failures; the manager keys on `result.status`).
    match &plan {
        BrowserPlan::Error(msg) => {
            return end_run(
                &repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Error,
                Some(msg.clone()),
                None,
                None,
                None,
            )
            .await;
        }
        BrowserPlan::Skip => {
            return end_run(
                &repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Skipped,
                Some("live browser unavailable; on_unavailable=skip".to_string()),
                None,
                None,
                None,
            )
            .await;
        }
        BrowserPlan::Defer => {
            return end_run(
                &repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Deferred,
                Some("live browser unavailable; deferred until one returns".to_string()),
                None,
                None,
                None,
            )
            .await;
        }
        // Executing plans fall through.
        BrowserPlan::UseNone | BrowserPlan::UseLive | BrowserPlan::LaunchHeadless => {}
    }

    let user_message = build_user_message(rec, fire_kind, services.as_ref()).await;

    // Session scoping: reuse the creator's session so artifacts and (for `live`)
    // the borrowed sidebar proxy resolve; fall back to a per-schedule synthetic
    // session id when the schedule was created without a creator session.
    let session_id = rec
        .creator_session_id
        .clone()
        .unwrap_or_else(|| format!("schedule:{}", rec.id));
    let mode = crate::loops::manager::db_str_to_agent_mode(&rec.mode);

    // Per-run token budget: applies to plain AND goal-wrapped runs. A
    // non-positive limit is treated as unbounded (no budget installed).
    let budget = rec
        .max_tokens_per_run
        .filter(|l| *l > 0)
        .map(|limit| TokenBudget::new(limit as u64));

    match plan {
        BrowserPlan::UseNone => {
            run_executing(
                &repo,
                events,
                rec,
                run_id,
                services,
                session_id,
                mode,
                budget,
                BrowserRouting::default(),
                none_policy_forbidden_prefixes(),
                user_message,
            )
            .await
        }
        BrowserPlan::UseLive => {
            run_executing(
                &repo,
                events,
                rec,
                run_id,
                services,
                session_id,
                mode,
                budget,
                BrowserRouting {
                    borrow_proxy: true,
                    bound_browser: None,
                },
                Vec::new(),
                user_message,
            )
            .await
        }
        BrowserPlan::LaunchHeadless => {
            // Headless is a production-only path — it spawns a real browser
            // process. Guard the stub path so tests never launch a browser.
            let Some(services) = services.as_ref() else {
                return end_run(
                    &repo,
                    events,
                    rec,
                    run_id,
                    ScheduleRunStatus::Error,
                    Some("headless run requires host services".to_string()),
                    None,
                    None,
                    None,
                )
                .await;
            };
            run_headless(
                &repo,
                events,
                rec,
                run_id,
                services,
                registry,
                session_id,
                mode,
                budget,
                user_message,
            )
            .await
        }
        // Non-executing verdicts already returned above.
        BrowserPlan::Error(_) | BrowserPlan::Skip | BrowserPlan::Defer => unreachable!(),
    }
}

/// Run an executing plan (`UseNone` / `UseLive` / headless-bound) to a persisted
/// run row. Handles the stub short-circuit (no services ⇒ immediate `Ok`), then
/// dispatches to the goal loop or a single plain kernel call. `routing` +
/// `forbidden_prefixes` carry the browser policy into the kernel request(s).
#[allow(clippy::too_many_arguments)]
async fn run_executing(
    repo: &ScheduleRepository<'_>,
    events: &ScheduleEvents,
    rec: &ScheduleRecord,
    run_id: i64,
    services: &Option<HostServices>,
    session_id: String,
    mode: nevoflux_builtin_wasm::AgentMode,
    budget: Option<Arc<TokenBudget>>,
    routing: BrowserRouting,
    forbidden_prefixes: Vec<String>,
    user_message: String,
) -> RunResult {
    // Stub path: no services wired (unit tests) → immediate ok, no text.
    let services = match services.as_ref() {
        Some(s) => s,
        None => {
            return end_run(
                repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Ok,
                None,
                None,
                None,
                None,
            )
            .await;
        }
    };

    // Goal-wrapped run: drive the goal turn loop.
    if let Some(condition) = rec.goal_condition.clone() {
        return run_goal_wrapped(
            repo,
            events,
            rec,
            run_id,
            services,
            session_id,
            mode,
            routing,
            forbidden_prefixes,
            budget,
            condition,
            user_message,
        )
        .await;
    }

    // Plain run: a single kernel call, with the budget threaded through so the
    // LLM boundary enforces it and the spend is recorded.
    let req = AgentExecRequest {
        session_id,
        mode,
        user_message,
        forbidden_tools: forbidden_tools(),
        forbidden_prefixes,
        unattended: true,
        iteration_loop_id: None,
        borrow_proxy: routing.borrow_proxy,
        bound_browser: routing.bound_browser,
        history: Vec::new(),
        token_budget: budget.clone(),
    };

    match run_agent_once(services, req).await {
        Ok(outcome) => {
            end_run(
                repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Ok,
                None,
                Some(outcome.text),
                budget_spent(&budget),
                None,
            )
            .await
        }
        Err(e) => {
            end_run(
                repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Error,
                Some(e),
                None,
                budget_spent(&budget),
                None,
            )
            .await
        }
    }
}

/// The headless-policy run: clone + prep a profile, launch and bind a dedicated
/// browser, run the kernel against it, and ALWAYS tear the browser + clone down
/// on every exit path.
///
/// Teardown discipline: before a browser handle exists (clone/inject/launch
/// failures) we still `kill_profile_processes` + `cleanup` the clone (the launch
/// may have spawned a process that never registered). Once the handle exists,
/// every subsequent exit runs the full `handle.terminate()` +
/// `kill_profile_processes` + `cleanup`. `run_executing` always returns a
/// `RunResult` (kernel error included), so the teardown after it is unconditional.
#[allow(clippy::too_many_arguments)]
async fn run_headless(
    repo: &ScheduleRepository<'_>,
    events: &ScheduleEvents,
    rec: &ScheduleRecord,
    run_id: i64,
    services: &HostServices,
    registry: Option<Arc<BrowserRegistry>>,
    session_id: String,
    mode: nevoflux_builtin_wasm::AgentMode,
    budget: Option<Arc<TokenBudget>>,
    user_message: String,
) -> RunResult {
    // env guard already passed via `plan_browser`; re-read the concrete path.
    let browser_bin = match std::env::var_os(HEADLESS_BROWSER_BIN_ENV) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            return headless_error(
                repo,
                events,
                rec,
                run_id,
                format!(
                "headless browser policy requires the {HEADLESS_BROWSER_BIN_ENV} env var to be set"
            ),
            )
            .await;
        }
    };
    let Some(registry) = registry else {
        return headless_error(
            repo,
            events,
            rec,
            run_id,
            "browser registry unavailable".to_string(),
        )
        .await;
    };

    let profile_mgr = ProfileManager {
        base_dir: resolve_base_profiles_dir(),
        work_dir: resolve_profile_work_dir(),
    };
    let base_name = rec.headless_profile.as_deref().unwrap_or("default");
    let clone = match profile_mgr.clone_base(base_name) {
        Ok(c) => c,
        Err(e) => {
            return headless_error(
                repo,
                events,
                rec,
                run_id,
                format!("profile clone failed: {e}"),
            )
            .await;
        }
    };
    if let Err(e) = profile_mgr.inject_automation_pref(&clone) {
        // Clone dir exists; no browser spawned yet — clean up the clone.
        teardown_clone(&profile_mgr, &clone).await;
        return headless_error(
            repo,
            events,
            rec,
            run_id,
            format!("profile pref injection failed: {e}"),
        )
        .await;
    }

    let cfg = BrowserLaunchConfig {
        browser_bin,
        profile_dir: clone.clone(),
        display: std::env::var("DISPLAY").ok(),
        register_timeout: HEADLESS_REGISTER_TIMEOUT,
    };

    // Serialize the snapshot→spawn→bind window against other headless launches.
    let spawn_result = {
        let _guard = headless_launch_lock().lock().await;
        let snapshot = registry.proxy_ids();
        spawn_and_supervise_excluding(cfg, registry.clone(), &snapshot).await
    };
    let (mut handle, entry) = match spawn_result {
        Ok(pair) => pair,
        Err(e) => {
            // Spawn error OR register timeout: a process may be running under the
            // clone profile — kill it and clean the clone.
            teardown_clone(&profile_mgr, &clone).await;
            return headless_error(
                repo,
                events,
                rec,
                run_id,
                format!("headless browser launch failed: {e}"),
            )
            .await;
        }
    };

    // Run the kernel bound to the spawned browser. `run_executing` always
    // returns (kernel error included) so teardown below is unconditional.
    let result = run_executing(
        repo,
        events,
        rec,
        run_id,
        &Some(services.clone()),
        session_id,
        mode,
        budget,
        BrowserRouting {
            borrow_proxy: false,
            bound_browser: Some(entry),
        },
        Vec::new(),
        user_message,
    )
    .await;

    // Unconditional teardown (kernel ok AND kernel error reach here).
    handle.terminate().await;
    teardown_clone(&profile_mgr, &clone).await;

    result
}

/// Kill any process still holding the clone profile and remove the clone dir.
/// Best-effort; used on every headless exit path.
async fn teardown_clone(profile_mgr: &ProfileManager, clone: &Path) {
    kill_profile_processes(clone).await;
    profile_mgr.cleanup(clone);
}

/// Record a headless failure as an `Error` run row + event.
async fn headless_error(
    repo: &ScheduleRepository<'_>,
    events: &ScheduleEvents,
    rec: &ScheduleRecord,
    run_id: i64,
    msg: String,
) -> RunResult {
    end_run(
        repo,
        events,
        rec,
        run_id,
        ScheduleRunStatus::Error,
        Some(msg),
        None,
        None,
        None,
    )
    .await
}

/// Snapshot the tokens spent so far on a budget (for `record_run_end`). `None`
/// when the run has no budget installed.
fn budget_spent(budget: &Option<Arc<TokenBudget>>) -> Option<i64> {
    budget
        .as_ref()
        .map(|b| b.spent.load(Ordering::Relaxed) as i64)
}

// ---------------------------------------------------------------------------
// Goal turn loop
// ---------------------------------------------------------------------------

/// The pure decision core of the goal loop. Given the just-completed `turn`
/// (1-based), the `max_turns` budget, the evaluator `verdict`, and whether the
/// token budget is now exhausted, decide the next step. Total and side-effect
/// free, so it is unit-tested exhaustively.
///
/// Precedence (matches the plan): a `met` verdict wins even at/over budget;
/// otherwise turns exhaustion wins over budget exhaustion; otherwise the
/// budget stops the loop; otherwise continue.
fn next_goal_step(turn: i64, max_turns: i64, verdict: &Verdict, budget_exceeded: bool) -> GoalStep {
    if verdict.met {
        GoalStep::Met
    } else if turn >= max_turns {
        GoalStep::Failed(format!(
            "goal not met after {turn} turns: {}",
            verdict.reason
        ))
    } else if budget_exceeded {
        GoalStep::Failed("token budget exhausted before goal met".to_string())
    } else {
        GoalStep::Continue(format!(
            "<GOAL-CONTINUATION>\n{}\nContinue. Turn {}/{}.\n</GOAL-CONTINUATION>",
            verdict.reason, turn, max_turns
        ))
    }
}

/// Outcome of the pure decision core.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GoalStep {
    /// Goal met at this turn — the loop succeeds with its captured final text.
    Met,
    /// Loop ends without meeting the goal (turns/budget exhausted).
    Failed(String),
    /// Run another turn with this `<GOAL-CONTINUATION>` user message.
    Continue(String),
}

/// Seam over the two network operations the goal loop performs, so the loop's
/// control flow (history pairing, continuation threading, budget accounting,
/// break conditions) is testable without a network. Production wires
/// [`run_agent_once`] + [`evaluate`]; tests supply canned replies.
#[allow(async_fn_in_trait)]
trait GoalTurnDriver {
    /// Run one unattended agent turn and return its final assistant text.
    async fn run_turn(
        &mut self,
        user_message: String,
        history: Vec<(String, String)>,
    ) -> Result<String, String>;

    /// Judge the goal condition against the accumulated `(role, text)`
    /// transcript. `Err` is a transport failure (handled as its own break).
    async fn evaluate_goal(&mut self, transcript: &[(String, String)]) -> Result<Verdict, String>;
}

/// The terminal outcome of [`drive_goal_loop`], mapped 1:1 onto a run row.
struct GoalLoopResult {
    ok: bool,
    error: Option<String>,
    final_text: Option<String>,
    tokens_used: Option<i64>,
    goal_turns: i64,
}

impl GoalLoopResult {
    fn spent(budget: Option<&TokenBudget>) -> Option<i64> {
        budget.map(|b| b.spent.load(Ordering::Relaxed) as i64)
    }
    fn met(text: String, turns: i64, budget: Option<&TokenBudget>) -> Self {
        Self {
            ok: true,
            error: None,
            final_text: Some(text),
            tokens_used: Self::spent(budget),
            goal_turns: turns,
        }
    }
    fn failed(msg: String, turns: i64, budget: Option<&TokenBudget>) -> Self {
        Self {
            ok: false,
            error: Some(msg),
            final_text: None,
            tokens_used: Self::spent(budget),
            goal_turns: turns,
        }
    }
}

/// Drive the goal turn loop over a [`GoalTurnDriver`]. Accumulates the
/// `(user, assistant)` transcript across turns, threads the token budget
/// (evaluator spend is added here; kernel spend is added at the LLM boundary),
/// and breaks per [`next_goal_step`] plus the two error breaks (kernel error,
/// evaluator transport error). Budget spend is recorded on ALL outcomes.
async fn drive_goal_loop<D: GoalTurnDriver>(
    mut driver: D,
    first_message: String,
    max_turns: i64,
    budget: Option<Arc<TokenBudget>>,
) -> GoalLoopResult {
    let mut history: Vec<(String, String)> = Vec::new();
    let mut turn = 0i64;
    let mut user_message = first_message;

    loop {
        turn += 1;
        let text = match driver.run_turn(user_message.clone(), history.clone()).await {
            Ok(t) => t,
            // Kernel error: budget spend so far is still recorded.
            Err(e) => return GoalLoopResult::failed(e, turn, budget.as_deref()),
        };
        // Pair this turn's (user, assistant) into the transcript BEFORE eval.
        history.push(("user".to_string(), user_message.clone()));
        history.push(("assistant".to_string(), text.clone()));

        let verdict = match driver.evaluate_goal(&history).await {
            Ok(v) => v,
            // Evaluator transport error is its own break (no verdict, so no
            // evaluator tokens are added).
            Err(e) => {
                return GoalLoopResult::failed(
                    format!("evaluator error: {e}"),
                    turn,
                    budget.as_deref(),
                )
            }
        };
        // Evaluator tokens count against the budget.
        if let Some(b) = budget.as_deref() {
            b.add(verdict.tokens_used);
        }
        let budget_exceeded = budget.as_deref().map(|b| b.exceeded()).unwrap_or(false);

        match next_goal_step(turn, max_turns, &verdict, budget_exceeded) {
            GoalStep::Met => return GoalLoopResult::met(text, turn, budget.as_deref()),
            GoalStep::Failed(msg) => return GoalLoopResult::failed(msg, turn, budget.as_deref()),
            GoalStep::Continue(next) => user_message = next,
        }
    }
}

/// Production [`GoalTurnDriver`]: each turn calls the shared unattended kernel
/// (threading the budget + browser routing) and each evaluation calls the
/// resolved evaluator.
struct ProductionGoalDriver<'a> {
    services: &'a HostServices,
    session_id: String,
    mode: nevoflux_builtin_wasm::AgentMode,
    routing: BrowserRouting,
    forbidden_prefixes: Vec<String>,
    budget: Option<Arc<TokenBudget>>,
    choice: EvaluatorChoice,
    condition: String,
}

impl GoalTurnDriver for ProductionGoalDriver<'_> {
    async fn run_turn(
        &mut self,
        user_message: String,
        history: Vec<(String, String)>,
    ) -> Result<String, String> {
        let req = AgentExecRequest {
            session_id: self.session_id.clone(),
            mode: self.mode,
            user_message,
            forbidden_tools: forbidden_tools(),
            forbidden_prefixes: self.forbidden_prefixes.clone(),
            unattended: true,
            iteration_loop_id: None,
            borrow_proxy: self.routing.borrow_proxy,
            bound_browser: self.routing.bound_browser.clone(),
            history,
            token_budget: self.budget.clone(),
        };
        run_agent_once(self.services, req).await.map(|o| o.text)
    }

    async fn evaluate_goal(&mut self, transcript: &[(String, String)]) -> Result<Verdict, String> {
        evaluate(&self.choice, &self.condition, transcript).await
    }
}

/// Resolve the evaluator against the live config, drive the goal loop, and
/// record the run. The persisted row already carries the resolved
/// provider/model (set at create time); resolution here re-derives the API key
/// (kept in config, never persisted). A resolution failure or a missing config
/// ends the run as an error (with the budget spend, if any, recorded).
#[allow(clippy::too_many_arguments)]
async fn run_goal_wrapped(
    repo: &ScheduleRepository<'_>,
    events: &ScheduleEvents,
    rec: &ScheduleRecord,
    run_id: i64,
    services: &HostServices,
    session_id: String,
    mode: nevoflux_builtin_wasm::AgentMode,
    routing: BrowserRouting,
    forbidden_prefixes: Vec<String>,
    budget: Option<Arc<TokenBudget>>,
    condition: String,
    first_message: String,
) -> RunResult {
    let config = match services.agent_config.as_ref() {
        Some(c) => c.clone(),
        None => {
            return end_run(
                repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Error,
                Some("goal evaluation unavailable (no agent config)".to_string()),
                None,
                budget_spent(&budget),
                Some(0),
            )
            .await;
        }
    };
    let choice = match resolve_evaluator(
        &config,
        rec.evaluator_provider.as_deref(),
        rec.evaluator_model.as_deref(),
    ) {
        Ok(c) => c,
        Err(e) => {
            return end_run(
                repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Error,
                Some(format!("evaluator error: {e}")),
                None,
                budget_spent(&budget),
                Some(0),
            )
            .await;
        }
    };
    let max_turns = rec
        .goal_max_turns
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_GOAL_MAX_TURNS);

    let driver = ProductionGoalDriver {
        services,
        session_id,
        mode,
        routing,
        forbidden_prefixes,
        budget: budget.clone(),
        choice,
        condition,
    };
    let result = drive_goal_loop(driver, first_message, max_turns, budget).await;

    let status = if result.ok {
        ScheduleRunStatus::Ok
    } else {
        ScheduleRunStatus::Error
    };
    end_run(
        repo,
        events,
        rec,
        run_id,
        status,
        result.error,
        result.final_text,
        result.tokens_used,
        Some(result.goal_turns),
    )
    .await
}

/// Persist `run_end`, emit the `run_end` event, and build the [`RunResult`].
/// Centralizes the exit paths (headless refusal, stub ok, plain ok/error,
/// goal-loop outcomes) so the persisted row and the emitted event never
/// diverge. `tokens_used` is the budget spend (when a budget was installed);
/// `goal_turns` is the number of goal turns taken (goal-wrapped runs only).
#[allow(clippy::too_many_arguments)]
async fn end_run(
    repo: &ScheduleRepository<'_>,
    events: &ScheduleEvents,
    rec: &ScheduleRecord,
    run_id: i64,
    status: ScheduleRunStatus,
    error: Option<String>,
    final_text: Option<String>,
    tokens_used: Option<i64>,
    goal_turns: Option<i64>,
) -> RunResult {
    let end = current_timestamp();
    let _ = repo.record_run_end(
        run_id,
        end,
        status,
        error.as_deref(),
        final_text.as_deref(),
        tokens_used,
        goal_turns,
    );
    events
        .run_end(
            &rec.id,
            &rec.name,
            run_id,
            status.as_str(),
            end,
            error.as_deref(),
        )
        .await;
    RunResult {
        run_id,
        ok: matches!(status, ScheduleRunStatus::Ok),
        status,
        error,
        final_text,
    }
}

/// Build the `<SCHEDULE-CONTEXT>`-prefixed user message. `run_sequence` is the
/// 1-based ordinal of this run (`run_count + 1`). The prompt body is either the
/// verbatim `prompt_text` or the materialized wrapped skill (reusing the loop
/// executor's resolver so the `{name, args}` JSON shape stays identical).
pub(crate) async fn build_user_message(
    rec: &ScheduleRecord,
    fire_kind: &str,
    services: Option<&HostServices>,
) -> String {
    let run_sequence = rec.run_count + 1;
    let body: String = if let Some(prompt) = &rec.prompt_text {
        prompt.clone()
    } else if let Some(skill_json) = &rec.wrapped_skill {
        crate::loops::executor::materialize_wrapped_skill(skill_json, services).await
    } else {
        "(no prompt or wrapped_skill)".into()
    };

    format!(
        "<SCHEDULE-CONTEXT>\n\
         schedule_id={}\n\
         name={}\n\
         fire_kind={}\n\
         run_sequence={}\n\
         </SCHEDULE-CONTEXT>\n\
         \n\
         {}",
        rec.id, rec.name, fire_kind, run_sequence, body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> ScheduleRecord {
        ScheduleRecord {
            id: id.into(),
            creator_session_id: None,
            name: "digest".into(),
            cron_expr: Some("0 9 * * *".into()),
            at_ts: None,
            prompt_text: Some("Summarize".into()),
            wrapped_skill: None,
            mode: "chat".into(),
            browser_policy: "none".into(),
            on_unavailable: None,
            headless_profile: None,
            catch_up: false,
            goal_condition: None,
            goal_max_turns: None,
            max_tokens_per_run: None,
            evaluator_model: None,
            evaluator_provider: None,
            status: nevoflux_storage::models::schedule::ScheduleStatus::Active,
            next_fire_at: Some(1_800_000_000),
            last_run_status: None,
            last_run_at: None,
            consecutive_failures: 0,
            run_count: 3,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn forbidden_tools_block_self_replication_and_goal_hijack() {
        let list = forbidden_tools();
        // Interactive prompts, recursive job creation, and goal hijack are all
        // barred from unattended scheduled runs.
        for expected in [
            "ask_user",
            "browser_ask_user",
            "loop_create",
            "schedule_create",
            "goal_set",
        ] {
            assert!(
                list.iter().any(|t| t == expected),
                "scheduled runs must forbid {expected}"
            );
        }
        // Read-only goal/schedule tools stay available to scheduled runs.
        assert!(!list.iter().any(|t| t == "goal_status"));
        assert!(!list.iter().any(|t| t == "schedule_list"));
    }

    #[tokio::test]
    async fn schedule_context_block_has_required_fields() {
        let rec = sample("sch12345");
        let msg = build_user_message(&rec, "scheduled", None).await;
        assert!(msg.contains("<SCHEDULE-CONTEXT>"));
        assert!(msg.contains("schedule_id=sch12345"));
        assert!(msg.contains("name=digest"));
        assert!(msg.contains("fire_kind=scheduled"));
        // run_sequence is run_count + 1.
        assert!(msg.contains("run_sequence=4"));
        assert!(msg.contains("</SCHEDULE-CONTEXT>"));
        // Prompt body appended verbatim after the block.
        assert!(msg.contains("Summarize"));
    }

    #[tokio::test]
    async fn stub_run_records_ok_with_no_text() {
        let db = Database::open_in_memory().unwrap();
        let repo = ScheduleRepository::new(&db);
        let rec = sample("sch00001");
        repo.create(&rec).unwrap();

        let events = ScheduleEvents::new(None);
        let res = execute_run(&None, &events, &db, &rec, "scheduled", None).await;

        assert!(res.ok);
        assert_eq!(res.status, ScheduleRunStatus::Ok);
        assert!(res.final_text.is_none());

        let runs = repo.list_runs("sch00001", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, ScheduleRunStatus::Ok);
        assert_eq!(runs[0].fire_kind, "scheduled");
    }

    #[tokio::test]
    async fn headless_policy_stub_run_errors_without_env_or_services() {
        // Stub path (no services) + no `NEVOFLUX_BROWSER_BIN`: a headless run
        // errors before spawning anything. (If the env var happens to be set in
        // CI, the missing services guard yields an equally-`headless` error — no
        // browser process is ever launched on the stub path either way.)
        let db = Database::open_in_memory().unwrap();
        let repo = ScheduleRepository::new(&db);
        let mut rec = sample("sch00002");
        rec.browser_policy = "headless".into();
        repo.create(&rec).unwrap();

        let events = ScheduleEvents::new(None);
        let res = execute_run(&None, &events, &db, &rec, "manual", None).await;

        assert!(!res.ok);
        assert_eq!(res.status, ScheduleRunStatus::Error);
        assert!(res.error.as_deref().unwrap().contains("headless"));

        let runs = repo.list_runs("sch00002", 10).unwrap();
        assert_eq!(runs[0].status, ScheduleRunStatus::Error);
    }

    // ---- plan_browser (pure decision core) ---------------------------------

    #[test]
    fn plan_none_is_use_none_regardless_of_availability_or_env() {
        // `none` never consults the registry or the headless env var.
        for available in [false, true] {
            for env in [false, true] {
                assert_eq!(
                    plan_browser("none", None, available, env),
                    BrowserPlan::UseNone
                );
                assert_eq!(
                    plan_browser("none", Some("skip"), available, env),
                    BrowserPlan::UseNone
                );
            }
        }
    }

    #[test]
    fn plan_live_available_is_use_live() {
        // Availability wins over on_unavailable, and env is irrelevant to live.
        for ou in [None, Some("defer"), Some("skip")] {
            for env in [false, true] {
                assert_eq!(
                    plan_browser("live", ou, true, env),
                    BrowserPlan::UseLive,
                    "ou={ou:?} env={env}"
                );
            }
        }
    }

    #[test]
    fn plan_live_unavailable_defers_by_default_and_on_defer() {
        // None (default) and explicit "defer" both defer.
        assert_eq!(plan_browser("live", None, false, false), BrowserPlan::Defer);
        assert_eq!(
            plan_browser("live", Some("defer"), false, false),
            BrowserPlan::Defer
        );
        // Env has no bearing on the live path.
        assert_eq!(plan_browser("live", None, false, true), BrowserPlan::Defer);
    }

    #[test]
    fn plan_live_unavailable_skips_on_skip() {
        assert_eq!(
            plan_browser("live", Some("skip"), false, false),
            BrowserPlan::Skip
        );
        assert_eq!(
            plan_browser("live", Some("skip"), false, true),
            BrowserPlan::Skip
        );
    }

    #[test]
    fn plan_headless_launches_only_when_env_set() {
        // Env set ⇒ launch, regardless of availability or on_unavailable.
        for available in [false, true] {
            for ou in [None, Some("defer"), Some("skip")] {
                assert_eq!(
                    plan_browser("headless", ou, available, true),
                    BrowserPlan::LaunchHeadless,
                    "available={available} ou={ou:?}"
                );
            }
        }
        // Env unset ⇒ error naming the env var.
        match plan_browser("headless", None, true, false) {
            BrowserPlan::Error(msg) => {
                assert!(msg.contains(HEADLESS_BROWSER_BIN_ENV), "msg={msg}");
                assert!(msg.contains("headless"), "msg={msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn plan_unknown_policy_is_error() {
        match plan_browser("bogus", None, true, true) {
            BrowserPlan::Error(msg) => assert!(msg.contains("bogus"), "msg={msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ---- execute_run policy dispatch (stub path) ---------------------------

    fn live_rec(id: &str, on_unavailable: Option<&str>) -> ScheduleRecord {
        let mut rec = sample(id);
        rec.browser_policy = "live".into();
        rec.on_unavailable = on_unavailable.map(|s| s.to_string());
        rec
    }

    #[tokio::test]
    async fn live_defer_records_deferred_without_browser() {
        let db = Database::open_in_memory().unwrap();
        let repo = ScheduleRepository::new(&db);
        let rec = live_rec("schlive1", None); // default defer
        repo.create(&rec).unwrap();

        let events = ScheduleEvents::new(None);
        // Empty registry ⇒ no browser available ⇒ defer.
        let registry = Arc::new(BrowserRegistry::new());
        let res = execute_run(&None, &events, &db, &rec, "scheduled", Some(registry)).await;

        assert!(!res.ok);
        assert_eq!(res.status, ScheduleRunStatus::Deferred);
        let runs = repo.list_runs("schlive1", 10).unwrap();
        assert_eq!(runs[0].status, ScheduleRunStatus::Deferred);
    }

    #[tokio::test]
    async fn live_skip_records_skipped_without_browser() {
        let db = Database::open_in_memory().unwrap();
        let repo = ScheduleRepository::new(&db);
        let rec = live_rec("schlive2", Some("skip"));
        repo.create(&rec).unwrap();

        let events = ScheduleEvents::new(None);
        let registry = Arc::new(BrowserRegistry::new());
        let res = execute_run(&None, &events, &db, &rec, "scheduled", Some(registry)).await;

        assert!(!res.ok);
        assert_eq!(res.status, ScheduleRunStatus::Skipped);
        let runs = repo.list_runs("schlive2", 10).unwrap();
        assert_eq!(runs[0].status, ScheduleRunStatus::Skipped);
    }

    #[tokio::test]
    async fn live_with_browser_runs_via_stub() {
        let db = Database::open_in_memory().unwrap();
        let repo = ScheduleRepository::new(&db);
        let rec = live_rec("schlive3", None);
        repo.create(&rec).unwrap();

        let events = ScheduleEvents::new(None);
        // A registered browser ⇒ available ⇒ UseLive ⇒ stub Ok.
        let registry = Arc::new(BrowserRegistry::new());
        registry.register("proxy-b1", b"proxy-b1".to_vec());
        let res = execute_run(&None, &events, &db, &rec, "scheduled", Some(registry)).await;

        assert!(res.ok);
        assert_eq!(res.status, ScheduleRunStatus::Ok);
        let runs = repo.list_runs("schlive3", 10).unwrap();
        assert_eq!(runs[0].status, ScheduleRunStatus::Ok);
    }

    #[tokio::test]
    async fn none_policy_runs_via_stub_ignoring_registry() {
        // `none` never blocks on browser availability.
        let db = Database::open_in_memory().unwrap();
        let repo = ScheduleRepository::new(&db);
        let rec = sample("schnone1"); // browser_policy "none"
        repo.create(&rec).unwrap();

        let events = ScheduleEvents::new(None);
        let res = execute_run(&None, &events, &db, &rec, "scheduled", None).await;
        assert!(res.ok);
        assert_eq!(res.status, ScheduleRunStatus::Ok);
    }

    // ---- next_goal_step (pure decision core) -------------------------------

    fn verdict(met: bool, reason: &str, tokens: u64) -> Verdict {
        Verdict {
            met,
            reason: reason.to_string(),
            tokens_used: tokens,
        }
    }

    #[test]
    fn step_met_is_met_regardless_of_turns_or_budget() {
        assert_eq!(
            next_goal_step(1, 20, &verdict(true, "confirmed", 0), false),
            GoalStep::Met
        );
        // met wins even at the turn boundary AND with the budget exhausted.
        assert_eq!(
            next_goal_step(20, 20, &verdict(true, "confirmed", 0), true),
            GoalStep::Met
        );
    }

    #[test]
    fn step_unmet_under_budget_continues_with_directive() {
        let step = next_goal_step(3, 20, &verdict(false, "still installing", 0), false);
        assert_eq!(
            step,
            GoalStep::Continue(
                "<GOAL-CONTINUATION>\nstill installing\nContinue. Turn 3/20.\n</GOAL-CONTINUATION>"
                    .to_string()
            )
        );
    }

    #[test]
    fn step_unmet_at_and_over_max_turns_fails() {
        assert_eq!(
            next_goal_step(2, 2, &verdict(false, "nope", 0), false),
            GoalStep::Failed("goal not met after 2 turns: nope".to_string())
        );
        // turn > max (defensive) still fails as turns-exhausted.
        assert_eq!(
            next_goal_step(3, 2, &verdict(false, "nope", 0), false),
            GoalStep::Failed("goal not met after 3 turns: nope".to_string())
        );
    }

    #[test]
    fn step_budget_exceeded_after_eval_fails() {
        assert_eq!(
            next_goal_step(1, 20, &verdict(false, "working", 0), true),
            GoalStep::Failed("token budget exhausted before goal met".to_string())
        );
    }

    #[test]
    fn step_turns_exhaustion_wins_over_budget() {
        // At the turn boundary AND over budget: the turns message wins (matches
        // the plan's precedence — the budget check is last).
        assert_eq!(
            next_goal_step(2, 2, &verdict(false, "nope", 0), true),
            GoalStep::Failed("goal not met after 2 turns: nope".to_string())
        );
    }

    // ---- drive_goal_loop (full control flow, no network) -------------------

    #[derive(Default)]
    struct Recorder {
        seen_messages: Vec<String>,
        seen_histories: Vec<Vec<(String, String)>>,
    }

    /// A [`GoalTurnDriver`] with canned per-turn replies, so the loop's control
    /// flow is exercised without any network. `kernel_tokens[i]` simulates the
    /// spend the LLM boundary would accrue on turn `i` (added to `budget`).
    struct StubDriver {
        turn_replies: Vec<Result<String, String>>,
        eval_replies: Vec<Result<Verdict, String>>,
        kernel_tokens: Vec<u64>,
        budget: Option<Arc<TokenBudget>>,
        run_calls: usize,
        eval_calls: usize,
        rec: std::rc::Rc<std::cell::RefCell<Recorder>>,
    }

    impl StubDriver {
        fn new(
            turn_replies: Vec<Result<String, String>>,
            eval_replies: Vec<Result<Verdict, String>>,
        ) -> Self {
            Self {
                turn_replies,
                eval_replies,
                kernel_tokens: Vec::new(),
                budget: None,
                run_calls: 0,
                eval_calls: 0,
                rec: std::rc::Rc::new(std::cell::RefCell::new(Recorder::default())),
            }
        }
        fn with_budget(mut self, budget: Arc<TokenBudget>, kernel_tokens: Vec<u64>) -> Self {
            self.budget = Some(budget);
            self.kernel_tokens = kernel_tokens;
            self
        }
    }

    impl GoalTurnDriver for StubDriver {
        async fn run_turn(
            &mut self,
            user_message: String,
            history: Vec<(String, String)>,
        ) -> Result<String, String> {
            self.rec.borrow_mut().seen_messages.push(user_message);
            self.rec.borrow_mut().seen_histories.push(history);
            let i = self.run_calls;
            self.run_calls += 1;
            // Simulate the LLM boundary accruing this turn's kernel spend.
            if let (Some(b), Some(t)) = (self.budget.as_deref(), self.kernel_tokens.get(i)) {
                b.add(*t);
            }
            self.turn_replies[i].clone()
        }

        async fn evaluate_goal(
            &mut self,
            _transcript: &[(String, String)],
        ) -> Result<Verdict, String> {
            let i = self.eval_calls;
            self.eval_calls += 1;
            self.eval_replies[i].clone()
        }
    }

    #[tokio::test]
    async fn loop_met_on_first_turn_is_ok() {
        let driver = StubDriver::new(
            vec![Ok("did the thing".into())],
            vec![Ok(verdict(true, "confirmed", 12))],
        );
        let r = drive_goal_loop(driver, "start".into(), 20, None).await;
        assert!(r.ok);
        assert!(r.error.is_none());
        assert_eq!(r.final_text.as_deref(), Some("did the thing"));
        assert_eq!(r.goal_turns, 1);
        // No budget installed → nothing recorded.
        assert_eq!(r.tokens_used, None);
    }

    #[tokio::test]
    async fn loop_met_on_third_turn_pairs_history_and_threads_continuation() {
        let driver = StubDriver::new(
            vec![Ok("t1".into()), Ok("t2".into()), Ok("t3".into())],
            vec![
                Ok(verdict(false, "not yet", 0)),
                Ok(verdict(false, "closer", 0)),
                Ok(verdict(true, "done", 0)),
            ],
        );
        let rec = driver.rec.clone();
        let r = drive_goal_loop(driver, "start".into(), 20, None).await;
        assert!(r.ok);
        assert_eq!(r.final_text.as_deref(), Some("t3"));
        assert_eq!(r.goal_turns, 3);

        let rec = rec.borrow();
        // Continuation threading: first message is the original; turns 2 and 3
        // receive the `<GOAL-CONTINUATION>` built from the prior verdict.
        assert_eq!(rec.seen_messages[0], "start");
        assert_eq!(
            rec.seen_messages[1],
            "<GOAL-CONTINUATION>\nnot yet\nContinue. Turn 1/20.\n</GOAL-CONTINUATION>"
        );
        assert_eq!(
            rec.seen_messages[2],
            "<GOAL-CONTINUATION>\ncloser\nContinue. Turn 2/20.\n</GOAL-CONTINUATION>"
        );
        // History pairing: each turn sees the accumulated (user, assistant) pairs.
        assert!(rec.seen_histories[0].is_empty());
        assert_eq!(
            rec.seen_histories[1],
            vec![
                ("user".to_string(), "start".to_string()),
                ("assistant".to_string(), "t1".to_string()),
            ]
        );
        assert_eq!(
            rec.seen_histories[2],
            vec![
                ("user".to_string(), "start".to_string()),
                ("assistant".to_string(), "t1".to_string()),
                (
                    "user".to_string(),
                    "<GOAL-CONTINUATION>\nnot yet\nContinue. Turn 1/20.\n</GOAL-CONTINUATION>"
                        .to_string()
                ),
                ("assistant".to_string(), "t2".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn loop_unmet_until_max_turns_fails() {
        let driver = StubDriver::new(
            vec![Ok("t1".into()), Ok("t2".into())],
            vec![
                Ok(verdict(false, "nope1", 0)),
                Ok(verdict(false, "nope2", 0)),
            ],
        );
        let r = drive_goal_loop(driver, "start".into(), 2, None).await;
        assert!(!r.ok);
        assert_eq!(
            r.error.as_deref(),
            Some("goal not met after 2 turns: nope2")
        );
        assert_eq!(r.goal_turns, 2);
        assert!(r.final_text.is_none());
    }

    #[tokio::test]
    async fn loop_evaluator_error_breaks_and_records_spend() {
        let budget = TokenBudget::new(1000);
        let driver = StubDriver::new(vec![Ok("t1".into())], vec![Err("network down".into())])
            .with_budget(budget.clone(), vec![40]);
        let r = drive_goal_loop(driver, "start".into(), 20, Some(budget)).await;
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("evaluator error: network down"));
        assert_eq!(r.goal_turns, 1);
        // Kernel spend accrued before the evaluator failed is still recorded;
        // no evaluator tokens are added on the transport-error path.
        assert_eq!(r.tokens_used, Some(40));
    }

    #[tokio::test]
    async fn loop_kernel_error_breaks_and_records_spend() {
        let budget = TokenBudget::new(1000);
        // The kernel accrues 25 tokens, then returns an error.
        let driver = StubDriver::new(vec![Err("kernel boom".into())], vec![])
            .with_budget(budget.clone(), vec![25]);
        let r = drive_goal_loop(driver, "start".into(), 20, Some(budget)).await;
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("kernel boom"));
        assert_eq!(r.goal_turns, 1);
        assert_eq!(r.tokens_used, Some(25));
    }

    #[tokio::test]
    async fn loop_budget_exhausted_after_eval_breaks() {
        let budget = TokenBudget::new(100);
        // Kernel spends 30; the evaluator returns unmet + 90 tokens → 120 ≥ 100.
        let driver = StubDriver::new(
            vec![Ok("t1".into())],
            vec![Ok(verdict(false, "still working", 90))],
        )
        .with_budget(budget.clone(), vec![30]);
        let r = drive_goal_loop(driver, "start".into(), 20, Some(budget)).await;
        assert!(!r.ok);
        assert_eq!(
            r.error.as_deref(),
            Some("token budget exhausted before goal met")
        );
        assert_eq!(r.goal_turns, 1);
        // Both kernel (30) and evaluator (90) spend are counted.
        assert_eq!(r.tokens_used, Some(120));
    }

    #[tokio::test]
    async fn loop_met_wins_even_when_budget_exhausted() {
        let budget = TokenBudget::new(10);
        // The met verdict itself pushes the budget over, but met still wins.
        let driver = StubDriver::new(
            vec![Ok("finished".into())],
            vec![Ok(verdict(true, "done", 50))],
        )
        .with_budget(budget.clone(), vec![]);
        let r = drive_goal_loop(driver, "start".into(), 20, Some(budget)).await;
        assert!(r.ok);
        assert_eq!(r.final_text.as_deref(), Some("finished"));
        assert_eq!(r.goal_turns, 1);
        assert_eq!(r.tokens_used, Some(50));
    }
}
