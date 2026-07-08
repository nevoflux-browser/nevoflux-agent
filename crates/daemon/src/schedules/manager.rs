//! ScheduleManager — the `/schedule` (routines-style) engine.
//!
//! ## Engine shape (deliberately unlike `/loop`)
//!
//! `/loop` spawns one sleeping timer task per loop. Schedules instead run a
//! **single** tick task on a 30s [`tokio::time::interval`] with
//! [`MissedTickBehavior::Delay`]. Every tick calls
//! [`ScheduleRepository::list_due`] and dispatches each due schedule. This is
//! robust to system sleep and wall-clock jumps (a laptop that slept through a
//! fire wakes up, sees `next_fire_at <= now`, and fires once) and, at a 1h
//! minimum cadence, a 30s poll is negligible load. `run_now` funnels a manual
//! fire through the same dispatch via an mpsc channel.
//!
//! ## Idle inhibitor
//!
//! [`ScheduleManager::pending_work`] counts **active schedules + in-flight
//! runs**. The managed-mode idle-suicide guard consults
//! [`ScheduleManager::has_pending_work`] so the daemon does not terminate while
//! a schedule could still fire (active) or is mid-run. Paused / ran / cancelled
//! schedules do *not* count — a paused schedule will never fire, so it does not
//! need to keep the daemon alive; resuming re-arms the counter.
//!
//! ## Divergence from `/loop`: no 3-strike auto-cancel
//!
//! Loops self-cancel after 3 consecutive failures. Schedules do **not**: a
//! recurring job (e.g. a nightly report) should keep trying on transient
//! failures; failure visibility comes from the UI badge
//! (`last_run_status` / `consecutive_failures`), not teardown.

use crate::event_bus::EventBus;
use crate::schedules::cron;
use crate::schedules::events::ScheduleEvents;
use crate::schedules::runner::{self, RunResult};
use crate::schedules::types::ScheduleId;
use crate::wasm::services::HostServices;
use nevoflux_storage::models::current_timestamp;
use nevoflux_storage::models::schedule::{
    ScheduleRecord, ScheduleRun, ScheduleRunStatus, ScheduleStatus,
};
use nevoflux_storage::repositories::ScheduleRepository;
use nevoflux_storage::Database;
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

/// Poll cadence for the due-tick engine.
const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum length of a `goal_condition` (characters) — mirrors the goals table
/// bound and the `schedules` DB CHECK, enforced eagerly for a clean error.
const GOAL_CONDITION_MAX_CHARS: usize = 4000;

/// Validated argument bundle for [`ScheduleManager::create`]. Field names and
/// types are binding — the `/schedule` tool + server layers construct this
/// verbatim.
#[derive(Debug, Clone)]
pub struct CreateScheduleArgs {
    pub creator_session_id: Option<String>,
    pub name: String,
    /// Cron expression (XOR with `at_ts`).
    pub cron_expr: Option<String>,
    /// One-off fire time as unix seconds (XOR with `cron_expr`).
    pub at_ts: Option<i64>,
    /// Prompt body (XOR with `wrapped_skill`).
    pub prompt_text: Option<String>,
    /// Wrapped-skill JSON `{name, args}` (XOR with `prompt_text`).
    pub wrapped_skill: Option<String>,
    pub mode: nevoflux_builtin_wasm::AgentMode,
    /// `"none" | "live" | "headless"` (validated).
    pub browser_policy: String,
    /// `"defer" | "skip"` when set (validated). Acted on in P4.
    pub on_unavailable: Option<String>,
    pub headless_profile: Option<String>,
    pub catch_up: bool,
    /// Stored in P1, evaluated in P3.
    pub goal_condition: Option<String>,
    pub goal_max_turns: Option<i64>,
    pub max_tokens_per_run: Option<i64>,
    /// Explicit evaluator model (else the active model). Resolved at create
    /// time when `goal_condition` is present.
    pub evaluator_model: Option<String>,
    /// Explicit direct-API evaluator provider (else the active provider).
    /// Resolved at create time when `goal_condition` is present.
    pub evaluator_provider: Option<String>,
}

/// A dispatch request funneled through the mpsc channel into the tick task
/// (used by `run_now`; the tick branch dispatches `scheduled` fires directly).
struct DispatchRequest {
    id: String,
    fire_kind: String,
}

/// Shared state cloned into the tick task and every spawned run.
struct Inner {
    db: Database,
    events: ScheduleEvents,
    /// Live host services for production runs; `None` ⇒ stub run path.
    services: Option<HostServices>,
    /// Schedule ids with a run in flight — the concurrency gate (one run per
    /// schedule at a time; tick and `run_now` both consult it).
    running: Mutex<HashSet<String>>,
    /// Active schedules + in-flight runs. See the module-level idle-inhibitor
    /// note for the exact transitions.
    pending_work: Arc<AtomicUsize>,
}

impl Inner {
    /// Spawn a detached task to run schedule `id` once. The running-set gate is
    /// re-checked inside the task, so a duplicate dispatch (tick racing a
    /// `run_now`) is a no-op rather than a double fire.
    fn spawn_run(self: &Arc<Self>, id: String, fire_kind: String) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.run_one(id, fire_kind).await;
        });
    }

    async fn run_one(self: Arc<Self>, id: String, fire_kind: String) {
        // Concurrency gate: skip if a run for this schedule is already in
        // flight. A manual `run_now` that lands here mid-run is a silent no-op
        // (documented: `run_now` still returns `Ok`).
        {
            let mut running = self.running.lock().await;
            if running.contains(&id) {
                return;
            }
            running.insert(id.clone());
        }
        // Run start: +1 in-flight.
        self.pending_work.fetch_add(1, Ordering::SeqCst);
        self.emit_snapshot().await;

        let rec = match ScheduleRepository::new(&self.db).get(&id) {
            Ok(Some(r)) => r,
            _ => {
                self.cleanup_run(&id).await;
                return;
            }
        };
        // Only fire active schedules. `list_due` already filters to active, but
        // a manual/catchup fire could target a schedule paused/cancelled in the
        // meantime; skip those without touching the active-schedule counter.
        if rec.status != ScheduleStatus::Active {
            self.cleanup_run(&id).await;
            return;
        }

        let result =
            runner::execute_run(&self.services, &self.events, &self.db, &rec, &fire_kind).await;
        self.finish_fire(&rec, &result).await;
        self.cleanup_run(&id).await;
    }

    /// Release the concurrency slot and account the run's end (-1 in-flight).
    /// Paired with the `+1` in [`Self::run_one`]; the early "already running"
    /// return happens *before* that `+1`, so it must not reach here.
    async fn cleanup_run(self: &Arc<Self>, id: &str) {
        self.running.lock().await.remove(id);
        self.pending_work.fetch_sub(1, Ordering::SeqCst);
        self.emit_snapshot().await;
    }

    /// Apply post-fire bookkeeping: failure counting, `last_run_status`, the
    /// next-fire recomputation, and the one-off `Active → Ran` transition.
    async fn finish_fire(&self, rec: &ScheduleRecord, result: &RunResult) {
        let now = current_timestamp();
        let repo = ScheduleRepository::new(&self.db);

        // Failure bookkeeping mirrors loops (reset on ok, bump on error) but
        // there is NO 3-strike auto-cancel — see the module-level note.
        if result.ok {
            let _ = repo.set_consecutive_failures(&rec.id, 0, now);
        } else {
            let _ = repo.set_consecutive_failures(&rec.id, rec.consecutive_failures + 1, now);
        }
        let _ = repo.set_last_run_status(&rec.id, result.status.as_str(), now);

        if rec.cron_expr.is_some() {
            // Recurring: advance to the next occurrence after `now`. Re-read the
            // LIVE status first — `rec` is a snapshot taken at fire time, and a
            // cancel/pause that landed mid-run must not get a future fire time
            // written onto its (now non-active) row. In that case this writes
            // next_fire_at = NULL (run_count/last_run_at still advance — the
            // run did happen); `resume` recomputes the fire time from scratch,
            // and `list_due` filters on status either way. If the cron can no
            // longer produce a fire (degenerate), clear next_fire rather than
            // leaving a past time.
            let still_active = matches!(
                repo.get(&rec.id).ok().flatten().map(|r| r.status),
                Some(ScheduleStatus::Active)
            );
            let next = if still_active {
                rec.cron_expr
                    .as_deref()
                    .and_then(|e| cron::next_after(e, now).ok())
            } else {
                None
            };
            if still_active && next.is_none() {
                tracing::warn!(
                    schedule_id = %rec.id,
                    "cron produced no next fire after run; clearing next_fire_at"
                );
            }
            let _ = repo.update_after_fire(&rec.id, next, now);
        } else {
            // One-off: consumed. run_count/last_run_at always advance (the run
            // DID happen), but the Active → Ran transition is atomic against the
            // LIVE database status — never the `rec` snapshot captured at fire
            // time. A cancel/pause that landed while this run was in flight has
            // already taken the schedule's pending_work decrement; deciding on
            // the stale snapshot would clobber its terminal status back to Ran
            // AND double-decrement, underflowing the counter and pinning
            // has_pending_work() true forever. `retire_one_off` flips the row
            // only if still active and reports whether it did, so exactly one
            // party ever decrements for the schedule.
            let _ = repo.update_after_fire(&rec.id, None, now);
            if repo.retire_one_off(&rec.id, now).unwrap_or(false) {
                self.pending_work.fetch_sub(1, Ordering::SeqCst);
                self.events
                    .state_changed(&rec.id, &rec.name, "ran", "active", None)
                    .await;
            }
        }
    }

    /// Publish the sticky aggregate snapshot the sidebar's `/schedule` icon
    /// consumes: `{active, running, failed_recent, next_fire_at}`.
    async fn emit_snapshot(&self) {
        let running = { self.running.lock().await.len() };
        let all = match ScheduleRepository::new(&self.db).list_all() {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut active = 0usize;
        let mut failed_recent = 0usize;
        let mut next_fire_at: Option<i64> = None;
        for rec in &all {
            if rec.status == ScheduleStatus::Active {
                active += 1;
                if let Some(nf) = rec.next_fire_at {
                    next_fire_at = Some(next_fire_at.map_or(nf, |cur| cur.min(nf)));
                }
            }
            if matches!(
                rec.last_run_status.as_deref(),
                Some("error") | Some("missed")
            ) {
                failed_recent += 1;
            }
        }
        self.events
            .snapshot(json!({
                "active": active,
                "running": running,
                "failed_recent": failed_recent,
                "next_fire_at": next_fire_at,
            }))
            .await;
    }
}

/// Boot-time work computed synchronously (DB writes) and the async emissions /
/// catchups deferred into the tick task's startup.
struct BootPlan {
    /// `(id, name, fire_was_at)` for each missed schedule.
    missed: Vec<(String, String, i64)>,
    /// `(id, name, prev_status, new_status)` state transitions (one-off → ran).
    state_changes: Vec<(String, String, String, String)>,
    /// Schedule ids to fire once as `catchup` after the tick task starts.
    catchups: Vec<String>,
}

pub struct ScheduleManager {
    inner: Arc<Inner>,
    /// Manual-fire channel into the tick task.
    run_tx: mpsc::Sender<DispatchRequest>,
    /// The single tick task; aborted on [`ScheduleManager::shutdown`].
    tick_handle: StdMutex<Option<JoinHandle<()>>>,
}

impl ScheduleManager {
    /// Boot the schedules engine: crash-sweep orphaned runs, detect and rearm
    /// missed fires, spawn the due-tick task, then return the handle.
    ///
    /// `bus = None` silences all events; `services = None` selects the stub run
    /// path (no LLM) — both used by unit tests.
    pub fn start_with_bus(
        db: Database,
        bus: Option<Arc<EventBus>>,
        services: Option<HostServices>,
    ) -> Arc<Self> {
        let (run_tx, run_rx) = mpsc::channel::<DispatchRequest>(64);
        let inner = Arc::new(Inner {
            db: db.clone(),
            events: ScheduleEvents::new(bus),
            services,
            running: Mutex::new(HashSet::new()),
            pending_work: Arc::new(AtomicUsize::new(0)),
        });

        let boot = boot_recover(&inner);

        // Spawn the single due-tick task. Boot emissions + catchups run once at
        // its head (deferred out of this synchronous fn), then it polls.
        let inner_task = Arc::clone(&inner);
        let handle = tokio::spawn(tick_loop(inner_task, run_rx, boot));

        Arc::new(Self {
            inner,
            run_tx,
            tick_handle: StdMutex::new(Some(handle)),
        })
    }

    /// Validate + persist a new schedule (state `Active`), arm its first fire,
    /// and emit `created` + `snapshot`. Returns the new [`ScheduleId`].
    pub async fn create(&self, args: CreateScheduleArgs) -> Result<ScheduleId, String> {
        if args.name.trim().is_empty() {
            return Err("schedule name must not be empty".into());
        }
        // XOR trigger + XOR body (also CHECK-enforced in sqlite; checked here
        // for a clean error message).
        if args.cron_expr.is_some() == args.at_ts.is_some() {
            return Err("exactly one of cron_expr or at_ts is required".into());
        }
        if args.prompt_text.is_some() == args.wrapped_skill.is_some() {
            return Err("exactly one of prompt_text or wrapped_skill is required".into());
        }
        if !matches!(args.browser_policy.as_str(), "none" | "live" | "headless") {
            return Err(format!("invalid browser_policy: {}", args.browser_policy));
        }
        if let Some(ou) = &args.on_unavailable {
            if !matches!(ou.as_str(), "defer" | "skip") {
                return Err(format!("invalid on_unavailable: {ou}"));
            }
        }

        // Goal validation + eager evaluator resolution. When a goal condition is
        // present we (a) enforce the same non-empty / ≤4000-char bound the DB
        // CHECK holds (so a bad condition fails with a clean message rather than
        // a constraint error), and (b) resolve the evaluator NOW against the
        // live agent config — an ACP provider or a missing key fails creation
        // fast, and the resolved (provider, model) strings are persisted so the
        // run-time evaluator call needs no re-resolution. `services` (production)
        // carries the config; without it, goal evaluation is unavailable.
        let (goal_condition, resolved_provider, resolved_model) = match &args.goal_condition {
            Some(condition) => {
                let trimmed = condition.trim();
                if trimmed.is_empty() {
                    return Err("goal_condition must not be empty".into());
                }
                let char_count = trimmed.chars().count();
                if char_count > GOAL_CONDITION_MAX_CHARS {
                    return Err(format!(
                        "goal_condition too long: {char_count} characters (max {GOAL_CONDITION_MAX_CHARS})"
                    ));
                }
                let config = self
                    .inner
                    .services
                    .as_ref()
                    .and_then(|s| s.agent_config.as_ref())
                    .ok_or("goal evaluation unavailable (no agent config)")?;
                let choice = crate::goals::evaluator::resolve_evaluator(
                    config,
                    args.evaluator_provider.as_deref(),
                    args.evaluator_model.as_deref(),
                )?;
                (
                    Some(trimmed.to_string()),
                    Some(choice.provider),
                    Some(choice.model),
                )
            }
            // No goal: carry any evaluator hints verbatim (inert without a goal).
            None => (None, args.evaluator_provider, args.evaluator_model),
        };

        let now = current_timestamp();
        // Compute the first fire; cron validation surfaces `TooFrequent` (with
        // the "use /loop" redirect) as a plain error string.
        let next_fire_at = if let Some(expr) = &args.cron_expr {
            cron::validate_and_next(expr, now).map_err(|e| e.to_string())?
        } else {
            // XOR above guarantees at_ts is Some here.
            let at = args.at_ts.unwrap_or(0);
            if at <= now {
                return Err("one-off schedule time must be in the future".into());
            }
            at
        };

        let id = ScheduleId::generate();
        let rec = ScheduleRecord {
            id: id.0.clone(),
            creator_session_id: args.creator_session_id,
            name: args.name,
            cron_expr: args.cron_expr,
            at_ts: args.at_ts,
            prompt_text: args.prompt_text,
            wrapped_skill: args.wrapped_skill,
            mode: crate::loops::manager::agent_mode_to_db_str(args.mode).to_string(),
            browser_policy: args.browser_policy,
            on_unavailable: args.on_unavailable,
            headless_profile: args.headless_profile,
            catch_up: args.catch_up,
            goal_condition,
            goal_max_turns: args.goal_max_turns,
            max_tokens_per_run: args.max_tokens_per_run,
            evaluator_model: resolved_model,
            evaluator_provider: resolved_provider,
            status: ScheduleStatus::Active,
            next_fire_at: Some(next_fire_at),
            last_run_status: None,
            last_run_at: None,
            consecutive_failures: 0,
            run_count: 0,
            created_at: now,
            updated_at: now,
        };
        ScheduleRepository::new(&self.inner.db)
            .create(&rec)
            .map_err(|e| e.to_string())?;
        // Created active: +1.
        self.inner.pending_work.fetch_add(1, Ordering::SeqCst);
        self.inner.events.created(&rec).await;
        self.inner.emit_snapshot().await;
        Ok(id)
    }

    /// Cancel a schedule (terminal). The Active→Cancelled / Paused→Cancelled
    /// flips are atomic against the live DB status (never a `get` snapshot),
    /// so racing writers — a second concurrent cancel, or a one-off run
    /// completing via `retire_one_off` in the same instant — can never pair
    /// two pending_work decrements with one logical transition. A schedule
    /// already in a terminal state (`ran`/`cancelled`, possibly flipped by
    /// the racing writer) is an idempotent no-op `Ok`: there is nothing left
    /// to stop, and the winner already did the accounting.
    pub async fn cancel(&self, id: &str) -> Result<(), String> {
        let repo = ScheduleRepository::new(&self.inner.db);
        let rec = repo
            .get(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("schedule not found: {id}"))?;
        let now = current_timestamp();
        if repo
            .transition_status(id, ScheduleStatus::Active, ScheduleStatus::Cancelled, now)
            .map_err(|e| e.to_string())?
        {
            // We won the Active→Cancelled flip: exactly one decrement.
            self.inner.pending_work.fetch_sub(1, Ordering::SeqCst);
            self.inner
                .events
                .state_changed(id, &rec.name, "cancelled", "active", None)
                .await;
            self.inner.emit_snapshot().await;
            return Ok(());
        }
        if repo
            .transition_status(id, ScheduleStatus::Paused, ScheduleStatus::Cancelled, now)
            .map_err(|e| e.to_string())?
        {
            // Paused schedules are not counted — no counter change.
            self.inner
                .events
                .state_changed(id, &rec.name, "cancelled", "paused", None)
                .await;
            self.inner.emit_snapshot().await;
            return Ok(());
        }
        // Already ran/cancelled: idempotent no-op (no event, no counter).
        Ok(())
    }

    /// Pause an active schedule (stops firing; `list_due` filters on status).
    /// The Active→Paused flip is atomic against the live DB status, so pause
    /// can never clobber a schedule that concurrently ran to completion
    /// (`Ran`) or was cancelled, and the counter decrement pairs 1:1 with the
    /// flip. Pausing an already-paused schedule is an idempotent `Ok` with no
    /// event re-emitted; pausing a ran/cancelled schedule is an error.
    ///
    /// Pause itself does not modify `next_fire_at` (`list_due`'s status
    /// filter is what stops the firing). Note that a run already in flight
    /// when the pause lands will still clear/advance `next_fire_at` in its
    /// `finish_fire` — harmless, since `resume` recomputes it from scratch.
    pub async fn pause(&self, id: &str) -> Result<(), String> {
        let repo = ScheduleRepository::new(&self.inner.db);
        let rec = repo
            .get(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("schedule not found: {id}"))?;
        let now = current_timestamp();
        if repo
            .transition_status(id, ScheduleStatus::Active, ScheduleStatus::Paused, now)
            .map_err(|e| e.to_string())?
        {
            // Active → Paused: -1, paired with the flip we won.
            self.inner.pending_work.fetch_sub(1, Ordering::SeqCst);
            self.inner
                .events
                .state_changed(id, &rec.name, "paused", "active", rec.next_fire_at)
                .await;
            self.inner.emit_snapshot().await;
            return Ok(());
        }
        // No flip — re-read the live status for an accurate verdict.
        match repo.get(id).map_err(|e| e.to_string())?.map(|r| r.status) {
            Some(ScheduleStatus::Paused) => Ok(()),
            Some(other) => Err(format!("cannot pause a {} schedule", other.as_str())),
            None => Err(format!("schedule not found: {id}")),
        }
    }

    /// Resume a paused schedule, recomputing `next_fire_at` from *now*. A
    /// one-off whose `at_ts` has already passed cannot be resumed (its moment
    /// is gone) — returns an error rather than firing immediately.
    ///
    /// Ordering: the new fire time is computed and persisted BEFORE the
    /// atomic Paused→Active flip. Recompute failures therefore error out with
    /// the row still paused and the counter untouched, and the schedule only
    /// becomes visible to the due-tick once it already carries the fresh fire
    /// time (a paused row's `next_fire_at` is invisible to `list_due`). The
    /// +1 is applied only when this call wins the flip, so a concurrent
    /// double resume increments exactly once. If a concurrent cancel wins
    /// instead, the pre-written `next_fire_at` remains on the cancelled row —
    /// harmless (status-filtered) but noted for consistency audits.
    pub async fn resume(&self, id: &str) -> Result<(), String> {
        let repo = ScheduleRepository::new(&self.inner.db);
        let rec = repo
            .get(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("schedule not found: {id}"))?;
        match rec.status {
            ScheduleStatus::Paused => {}
            // Already active (e.g. a concurrent resume won): idempotent.
            ScheduleStatus::Active => return Ok(()),
            other => return Err(format!("cannot resume a {} schedule", other.as_str())),
        }
        let now = current_timestamp();
        // cron_expr / at_ts are immutable after create, so the snapshot is
        // authoritative for the recompute even under status races.
        let next = if let Some(expr) = &rec.cron_expr {
            cron::next_after(expr, now).map_err(|e| e.to_string())?
        } else if let Some(at) = rec.at_ts {
            if at <= now {
                return Err("one-off time already passed".into());
            }
            at
        } else {
            return Err("schedule has neither cron_expr nor at_ts".into());
        };
        repo.update_next_fire(id, Some(next), now)
            .map_err(|e| e.to_string())?;
        if repo
            .transition_status(id, ScheduleStatus::Paused, ScheduleStatus::Active, now)
            .map_err(|e| e.to_string())?
        {
            // Paused → Active: +1, paired with the flip we won.
            self.inner.pending_work.fetch_add(1, Ordering::SeqCst);
            self.inner
                .events
                .state_changed(id, &rec.name, "active", "paused", Some(next))
                .await;
            self.inner.emit_snapshot().await;
            return Ok(());
        }
        // No flip — a racing writer changed the status since our snapshot.
        match repo.get(id).map_err(|e| e.to_string())?.map(|r| r.status) {
            // A concurrent resume won and did the accounting: idempotent.
            Some(ScheduleStatus::Active) => Ok(()),
            Some(other) => Err(format!("cannot resume a {} schedule", other.as_str())),
            None => Err(format!("schedule not found: {id}")),
        }
    }

    /// Enqueue a manual fire (`fire_kind = "manual"`). Respects the concurrency
    /// gate — if a run is already in flight for this schedule the fire is a
    /// silent no-op and this still returns `Ok`.
    pub async fn run_now(&self, id: &str) -> Result<(), String> {
        let repo = ScheduleRepository::new(&self.inner.db);
        let rec = repo
            .get(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("schedule not found: {id}"))?;
        if rec.status != ScheduleStatus::Active {
            return Err(format!("cannot run a {} schedule", rec.status.as_str()));
        }
        self.run_tx
            .send(DispatchRequest {
                id: id.to_string(),
                fire_kind: "manual".to_string(),
            })
            .await
            .map_err(|_| "schedule dispatcher is not running".to_string())
    }

    /// All schedules (any status), oldest first.
    pub async fn list(&self) -> Result<Vec<ScheduleRecord>, String> {
        ScheduleRepository::new(&self.inner.db)
            .list_all()
            .map_err(|e| e.to_string())
    }

    /// Recent runs for a schedule, newest first.
    pub async fn runs(&self, id: &str, limit: i64) -> Result<Vec<ScheduleRun>, String> {
        ScheduleRepository::new(&self.inner.db)
            .list_runs(id, limit)
            .map_err(|e| e.to_string())
    }

    /// True while any schedule is active or a run is in flight. Consulted by the
    /// managed-mode idle-suicide inhibitor. Paused schedules do not count.
    pub fn has_pending_work(&self) -> bool {
        self.inner.pending_work.load(Ordering::SeqCst) > 0
    }

    /// Shared handle to the pending-work counter, for the idle guard to observe
    /// without holding the manager.
    pub fn pending_work_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.inner.pending_work)
    }

    /// Abort the tick task, mark any still-running run rows cancelled, and clear
    /// the concurrency gate. Already-spawned run tasks finish naturally (they
    /// are not aborted), so their `+1/-1` pending_work pairing stays balanced.
    pub async fn shutdown(&self) {
        if let Some(handle) = self.tick_handle.lock().unwrap().take() {
            handle.abort();
        }
        let now = current_timestamp();
        let _ = ScheduleRepository::new(&self.inner.db)
            .sweep_orphaned_runs(now, "cancelled by daemon shutdown");
        self.inner.running.lock().await.clear();
    }

    pub fn events(&self) -> &ScheduleEvents {
        &self.inner.events
    }
}

/// Synchronous boot recovery (DB writes only). Sweeps orphaned run rows,
/// detects + rearms missed fires, retires spent/stranded one-offs, and only
/// THEN counts the surviving active schedules into pending_work — retire
/// decisions never touch the counter, so there is no boot-path decrement to
/// keep balanced. Returns the async work (events + catchups) to run at the
/// tick task's head so [`ScheduleManager::start_with_bus`] can stay non-async.
fn boot_recover(inner: &Arc<Inner>) -> BootPlan {
    let now = current_timestamp();
    let repo = ScheduleRepository::new(&inner.db);

    // (1) Crash-recovery: any run row left `running` was orphaned by a restart.
    let _ = repo.sweep_orphaned_runs(now, "orphaned by daemon restart");

    let mut plan = BootPlan {
        missed: Vec::new(),
        state_changes: Vec::new(),
        catchups: Vec::new(),
    };

    // (2) Rearm/retire pass over the active set (counter untouched here).
    let active = repo.list_active().unwrap_or_default();
    for rec in &active {
        let Some(nf) = rec.next_fire_at else {
            // Active with no armed fire. For a one-off this means a prior daemon
            // died between a boot catchup's next_fire clear and the catchup
            // run's finish_fire: without intervention it would never fire again
            // yet keep counting as pending work forever. Retire it, with a
            // cancelled run row for operator visibility. (A cron row can only
            // get here in a degenerate way — next_after failed at finish_fire —
            // and is deliberately left alone.)
            if rec.cron_expr.is_none() && repo.retire_one_off(&rec.id, now).unwrap_or(false) {
                // Visibility row: fire_kind "scheduled" (the fire that never
                // happened was the schedule's own), status `cancelled` with an
                // explanatory error — mirroring the sweep's "orphaned by
                // daemon restart" shape rather than claiming a catchup ran.
                if let Ok(run_id) = repo.record_run_start(&rec.id, now, "scheduled") {
                    let _ = repo.record_run_end(
                        run_id,
                        now,
                        ScheduleRunStatus::Cancelled,
                        Some("stranded by prior shutdown"),
                        None,
                        None,
                        None,
                    );
                }
                plan.state_changes.push((
                    rec.id.clone(),
                    rec.name.clone(),
                    "active".to_string(),
                    "ran".to_string(),
                ));
            }
            continue;
        };
        if nf >= now {
            continue; // future fire — nothing to recover.
        }

        // (3) Missed fire: record it and set the badge status.
        let _ = repo.record_missed(&rec.id, nf, now);
        let _ = repo.set_last_run_status(&rec.id, "missed", now);
        plan.missed.push((rec.id.clone(), rec.name.clone(), nf));

        if let Some(expr) = &rec.cron_expr {
            // Recurring: rearm to the next occurrence after now.
            let next = cron::next_after(expr, now).ok();
            let _ = repo.update_next_fire(&rec.id, next, now);
            if rec.catch_up {
                plan.catchups.push(rec.id.clone());
            }
        } else if rec.catch_up {
            // One-off with catch_up: clear next_fire so the tick can't also fire
            // it, keep it Active (so it IS counted below), and run it once as a
            // catchup. The catchup run's finish_fire retires it via
            // retire_one_off (which pairs the pending_work -1 with the flip).
            let _ = repo.update_next_fire(&rec.id, None, now);
            plan.catchups.push(rec.id.clone());
        } else {
            // One-off, no catch_up: the moment is gone — retire to `Ran`.
            let _ = repo.update_next_fire(&rec.id, None, now);
            if repo.retire_one_off(&rec.id, now).unwrap_or(false) {
                plan.state_changes.push((
                    rec.id.clone(),
                    rec.name.clone(),
                    "active".to_string(),
                    "ran".to_string(),
                ));
            }
        }
    }

    // (4) Count pending work AFTER the pass: schedules retired above never
    // enter the counter at all. Nothing can interleave — the tick task is not
    // spawned yet and the manager handle has not been returned to callers.
    let still_active = repo.list_active().map(|v| v.len()).unwrap_or(0);
    inner.pending_work.fetch_add(still_active, Ordering::SeqCst);

    plan
}

/// The single due-tick task body. Runs the deferred boot emissions + catchups
/// once, then polls `list_due` every [`TICK_INTERVAL`] and services manual
/// fires arriving on `run_rx`.
async fn tick_loop(inner: Arc<Inner>, mut run_rx: mpsc::Receiver<DispatchRequest>, boot: BootPlan) {
    // Deferred boot emissions (start_with_bus is synchronous).
    for (id, name, fire_was_at) in boot.missed {
        inner.events.missed(&id, &name, fire_was_at).await;
    }
    for (id, name, prev, new) in boot.state_changes {
        inner
            .events
            .state_changed(&id, &name, &new, &prev, None)
            .await;
    }
    // Boot catchups: fire once each. next_fire was cleared/rearmed above so the
    // first `list_due` poll below will not double-fire them.
    for id in boot.catchups {
        inner.spawn_run(id, "catchup".to_string());
    }
    inner.emit_snapshot().await;

    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = current_timestamp();
                if let Ok(due) = ScheduleRepository::new(&inner.db).list_due(now) {
                    for rec in due {
                        inner.spawn_run(rec.id, "scheduled".to_string());
                    }
                }
            }
            maybe = run_rx.recv() => {
                match maybe {
                    Some(req) => inner.spawn_run(req.id, req.fire_kind),
                    // All senders dropped (manager gone) — stop the task.
                    None => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> CreateScheduleArgs {
        CreateScheduleArgs {
            creator_session_id: None,
            name: "t".into(),
            cron_expr: Some("0 9 * * *".into()),
            at_ts: None,
            prompt_text: Some("p".into()),
            wrapped_skill: None,
            mode: nevoflux_builtin_wasm::AgentMode::Chat,
            browser_policy: "none".into(),
            on_unavailable: None,
            headless_profile: None,
            catch_up: false,
            goal_condition: None,
            goal_max_turns: None,
            max_tokens_per_run: None,
            evaluator_model: None,
            evaluator_provider: None,
        }
    }

    /// Seed a schedule row directly (bypassing `create`) so tests can plant a
    /// past `next_fire_at` for boot-recovery assertions.
    fn seed(
        id: &str,
        cron: Option<&str>,
        at: Option<i64>,
        next_fire: Option<i64>,
    ) -> ScheduleRecord {
        ScheduleRecord {
            id: id.into(),
            creator_session_id: None,
            name: "seed".into(),
            cron_expr: cron.map(|s| s.to_string()),
            at_ts: at,
            prompt_text: Some("p".into()),
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
            status: ScheduleStatus::Active,
            next_fire_at: next_fire,
            last_run_status: None,
            last_run_at: None,
            consecutive_failures: 0,
            run_count: 0,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[tokio::test]
    async fn create_validates_and_persists() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let id = mgr.create(base_args()).await.expect("created");

        let all = mgr.list().await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].next_fire_at.is_some());
        assert_eq!(all[0].status, ScheduleStatus::Active);
        assert!(mgr.has_pending_work());

        mgr.cancel(&id.0).await.unwrap();
        assert!(!mgr.has_pending_work());
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn sub_hourly_cron_rejected() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let mut args = base_args();
        args.cron_expr = Some("*/30 * * * *".into());
        let err = mgr.create(args).await.unwrap_err();
        // TooFrequent surfaced as a String, carrying the /loop redirect.
        assert!(
            err.contains("more often") || err.to_lowercase().contains("loop"),
            "unexpected error: {err}"
        );
        // Nothing persisted, nothing counted.
        assert_eq!(mgr.list().await.unwrap().len(), 0);
        assert!(!mgr.has_pending_work());
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn create_rejects_xor_violations() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);

        // Both cron and at → error.
        let mut a = base_args();
        a.at_ts = Some(current_timestamp() + 100_000);
        assert!(mgr
            .create(a)
            .await
            .unwrap_err()
            .contains("cron_expr or at_ts"));

        // Neither prompt nor skill → error.
        let mut b = base_args();
        b.prompt_text = None;
        b.wrapped_skill = None;
        assert!(mgr
            .create(b)
            .await
            .unwrap_err()
            .contains("prompt_text or wrapped_skill"));

        // Past one-off → error.
        let mut c = base_args();
        c.cron_expr = None;
        c.at_ts = Some(current_timestamp() - 10);
        assert!(mgr.create(c).await.unwrap_err().contains("future"));

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn missed_detection_at_boot() {
        let db = Database::open_in_memory().unwrap();
        let now = current_timestamp();
        // Seed a cron schedule whose next_fire_at is well in the past.
        {
            let repo = ScheduleRepository::new(&db);
            repo.create(&seed(
                "sch00001",
                Some("0 9 * * *"),
                None,
                Some(now - 100_000),
            ))
            .unwrap();
        }
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);

        // Boot recovery is synchronous, so the DB reflects it immediately.
        let runs = mgr.runs("sch00001", 10).await.unwrap();
        assert!(
            runs.iter().any(|r| r.status == ScheduleRunStatus::Missed),
            "expected a missed run row"
        );
        let rec = mgr
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == "sch00001")
            .unwrap();
        assert_eq!(rec.status, ScheduleStatus::Active, "cron stays active");
        assert!(
            rec.next_fire_at.unwrap() > now,
            "next_fire_at must be rearmed into the future, got {:?}",
            rec.next_fire_at
        );
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn one_off_missed_no_catchup_marks_ran() {
        let db = Database::open_in_memory().unwrap();
        let now = current_timestamp();
        {
            let repo = ScheduleRepository::new(&db);
            repo.create(&seed("sch00009", None, Some(now - 100), Some(now - 100)))
                .unwrap();
        }
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let rec = mgr
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == "sch00009")
            .unwrap();
        assert_eq!(rec.status, ScheduleStatus::Ran);
        // Retired during the boot pass, so it was never counted at all.
        assert!(!mgr.has_pending_work());
        let runs = mgr.runs("sch00009", 10).await.unwrap();
        assert!(runs.iter().any(|r| r.status == ScheduleRunStatus::Missed));
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn pause_resume_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let id = mgr.create(base_args()).await.unwrap();

        mgr.pause(&id.0).await.unwrap();
        let paused = mgr.list().await.unwrap();
        assert_eq!(paused[0].status, ScheduleStatus::Paused);
        // Paused schedule is not due.
        let due = ScheduleRepository::new(&db)
            .list_due(current_timestamp() + 10_000_000)
            .unwrap();
        assert!(due.is_empty(), "paused schedule must not be due");
        assert!(!mgr.has_pending_work());

        mgr.resume(&id.0).await.unwrap();
        let resumed = mgr.list().await.unwrap();
        assert_eq!(resumed[0].status, ScheduleStatus::Active);
        assert!(resumed[0].next_fire_at.is_some());
        assert!(mgr.has_pending_work());
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn pending_work_counter_balances_across_lifecycle() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let counter = mgr.pending_work_handle();

        assert_eq!(counter.load(Ordering::SeqCst), 0);
        let id = mgr.create(base_args()).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "create active → 1");
        mgr.pause(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0, "pause → 0");
        mgr.resume(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "resume → 1");
        mgr.cancel(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0, "cancel → 0");
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn run_now_stub_records_ok_run() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let id = mgr.create(base_args()).await.unwrap();

        mgr.run_now(&id.0).await.unwrap();

        // Wait for the spawned stub run to persist its ok row.
        let mut ok = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let runs = mgr.runs(&id.0, 10).await.unwrap();
            if runs.iter().any(|r| r.status == ScheduleRunStatus::Ok) {
                ok = true;
                break;
            }
        }
        assert!(ok, "manual run should record an ok run");

        let runs = mgr.runs(&id.0, 10).await.unwrap();
        assert_eq!(runs[0].fire_kind, "manual");
        // Cron schedule still active after a manual fire; run advanced count.
        let rec = mgr.list().await.unwrap().into_iter().next().unwrap();
        assert_eq!(rec.status, ScheduleStatus::Active);
        assert_eq!(rec.run_count, 1);
        assert_eq!(rec.last_run_status.as_deref(), Some("ok"));
        // Back to just the active schedule (in-flight run ended).
        assert!(mgr.has_pending_work());
        assert_eq!(mgr.pending_work_handle().load(Ordering::SeqCst), 1);
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn run_now_skips_when_already_running_returns_ok() {
        // Two back-to-back manual fires: the concurrency gate makes at most one
        // run per schedule at a time, and run_now still returns Ok either way.
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let id = mgr.create(base_args()).await.unwrap();

        mgr.run_now(&id.0).await.unwrap();
        mgr.run_now(&id.0).await.unwrap();

        // Let the dispatcher settle.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !mgr.runs(&id.0, 10).await.unwrap().is_empty() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        // run_count reflects completed fires; the gate prevents a runaway count.
        let rec = mgr.list().await.unwrap().into_iter().next().unwrap();
        assert!(rec.run_count >= 1, "at least one manual fire completed");
        mgr.shutdown().await;
    }

    /// Regression (review CRITICAL 1): a one-off cancelled while its run is in
    /// flight must NOT be clobbered back to `Ran` by `finish_fire`'s stale
    /// record snapshot, and pending_work must NOT be double-decremented (which
    /// underflowed to usize::MAX and pinned has_pending_work() true forever).
    /// The stub runner is instant, so the race window is reproduced by driving
    /// the internals directly: snapshot the record, simulate run-start
    /// bookkeeping, cancel, then apply finish_fire with the stale snapshot.
    #[tokio::test]
    async fn cancel_mid_run_does_not_double_decrement_one_off() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let counter = mgr.pending_work_handle();

        let mut args = base_args();
        args.cron_expr = None;
        args.at_ts = Some(current_timestamp() + 100_000);
        let id = mgr.create(args).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "active one-off → 1");

        // Fire time: run_one snapshots the record...
        let stale_rec = ScheduleRepository::new(&db).get(&id.0).unwrap().unwrap();
        assert_eq!(stale_rec.status, ScheduleStatus::Active);
        // ...takes the concurrency slot and counts the in-flight run.
        mgr.inner.running.lock().await.insert(id.0.clone());
        mgr.inner.pending_work.fetch_add(1, Ordering::SeqCst);
        let run_id = ScheduleRepository::new(&db)
            .record_run_start(&id.0, current_timestamp(), "manual")
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2, "active + in-flight");

        // User cancels mid-run: sees Active, takes the schedule's decrement.
        mgr.cancel(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "in-flight run only");

        // Run completes with the STALE snapshot (status still Active in it).
        let result = RunResult {
            run_id,
            ok: true,
            status: ScheduleRunStatus::Ok,
            error: None,
            final_text: None,
        };
        mgr.inner.finish_fire(&stale_rec, &result).await;
        mgr.inner.cleanup_run(&id.0).await;

        // Exactly zero — no double decrement, no underflow.
        assert_eq!(counter.load(Ordering::SeqCst), 0, "balanced at 0");
        assert!(!mgr.has_pending_work());
        // Terminal status not clobbered back to Ran.
        let after = ScheduleRepository::new(&db).get(&id.0).unwrap().unwrap();
        assert_eq!(after.status, ScheduleStatus::Cancelled);
        mgr.shutdown().await;
    }

    /// Regression (review IMPORTANT 2): a daemon that died between a boot
    /// catchup's next_fire clear and the catchup run's finish_fire leaves an
    /// Active one-off with next_fire_at=NULL. The next boot must retire it
    /// (it can never fire again) instead of counting it as pending work
    /// forever, and record a visible cancelled run row.
    #[tokio::test]
    async fn boot_retires_stranded_active_one_off() {
        let db = Database::open_in_memory().unwrap();
        let now = current_timestamp();
        {
            let repo = ScheduleRepository::new(&db);
            let mut rec = seed("sch00077", None, Some(now - 500), None);
            rec.catch_up = true;
            repo.create(&rec).unwrap();
        }
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);

        let rec = mgr
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == "sch00077")
            .unwrap();
        assert_eq!(rec.status, ScheduleStatus::Ran, "stranded one-off retired");
        assert!(!mgr.has_pending_work(), "retired before counting");
        assert_eq!(mgr.pending_work_handle().load(Ordering::SeqCst), 0);

        let runs = mgr.runs("sch00077", 10).await.unwrap();
        let stranded = runs
            .iter()
            .find(|r| r.status == ScheduleRunStatus::Cancelled)
            .expect("visibility run row");
        assert_eq!(
            stranded.error_message.as_deref(),
            Some("stranded by prior shutdown")
        );
        assert_eq!(stranded.fire_kind, "scheduled");
        mgr.shutdown().await;
    }

    /// Regression (re-review): cancel arriving after a racing writer already
    /// took the Active→Cancelled flip (and its decrement) must be an
    /// idempotent no-op — the old read-then-write shape let both cancels
    /// snapshot Active and double-decrement. The winner is emulated exactly
    /// as the new cancel path behaves: atomic flip + one decrement.
    #[tokio::test]
    async fn cancel_race_decrements_once() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let counter = mgr.pending_work_handle();
        let id = mgr.create(base_args()).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Racing cancel wins the flip and does its accounting.
        let repo = ScheduleRepository::new(&db);
        let now = current_timestamp();
        assert!(repo
            .transition_status(
                &id.0,
                ScheduleStatus::Active,
                ScheduleStatus::Cancelled,
                now
            )
            .unwrap());
        mgr.inner.pending_work.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Our cancel arrives second: Ok, but NO second decrement (pre-fix this
        // shape underflowed to usize::MAX) and no status churn.
        mgr.cancel(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0, "no double decrement");
        assert!(!mgr.has_pending_work());
        assert_eq!(
            repo.get(&id.0).unwrap().unwrap().status,
            ScheduleStatus::Cancelled
        );
        // Sequential third cancel is equally idempotent.
        mgr.cancel(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        mgr.shutdown().await;
    }

    /// Regression (re-review): cancel landing after a one-off's run completed
    /// (finish_fire won the Active→Ran flip and decremented) must not clobber
    /// `Ran` back to `Cancelled` — the old code overwrote any non-cancelled
    /// status — and must not decrement again.
    #[tokio::test]
    async fn cancel_after_retire_is_noop() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let counter = mgr.pending_work_handle();
        let mut args = base_args();
        args.cron_expr = None;
        args.at_ts = Some(current_timestamp() + 100_000);
        let id = mgr.create(args).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // finish_fire wins: retires the one-off and takes the decrement.
        let repo = ScheduleRepository::new(&db);
        assert!(repo.retire_one_off(&id.0, current_timestamp()).unwrap());
        mgr.inner.pending_work.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Late cancel: idempotent Ok, Ran preserved, counter untouched.
        mgr.cancel(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(
            repo.get(&id.0).unwrap().unwrap().status,
            ScheduleStatus::Ran,
            "terminal Ran must not be clobbered by a late cancel"
        );
        mgr.shutdown().await;
    }

    /// Regression (re-review): a resume arriving after a racing resume already
    /// won the Paused→Active flip (and its increment) must not increment a
    /// second time — the old read-then-write shape let both snapshot Paused
    /// and leak the counter upward so the daemon never idles.
    #[tokio::test]
    async fn resume_race_increments_once() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let counter = mgr.pending_work_handle();
        let id = mgr.create(base_args()).await.unwrap();
        mgr.pause(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Racing resume wins: arms the fire time, flips, increments — exactly
        // what the new resume path does.
        let repo = ScheduleRepository::new(&db);
        let now = current_timestamp();
        repo.update_next_fire(&id.0, Some(now + 3_600), now)
            .unwrap();
        assert!(repo
            .transition_status(&id.0, ScheduleStatus::Paused, ScheduleStatus::Active, now)
            .unwrap());
        mgr.inner.pending_work.fetch_add(1, Ordering::SeqCst);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Our resume arrives second: idempotent Ok, counter stays exactly 1.
        mgr.resume(&id.0).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1, "no double increment");
        // The losing flip reports false, so no caller can pair a +1 with it.
        assert!(!repo
            .transition_status(&id.0, ScheduleStatus::Paused, ScheduleStatus::Active, now)
            .unwrap());
        mgr.shutdown().await;
    }

    /// Regression (re-review): pause must never clobber a terminal `Ran`
    /// status (old shape: snapshot Active → blind UPDATE could overwrite a
    /// concurrent run-completion's Ran with Paused and double-decrement).
    #[tokio::test]
    async fn pause_on_ran_does_not_clobber() {
        let db = Database::open_in_memory().unwrap();
        let now = current_timestamp();
        {
            let repo = ScheduleRepository::new(&db);
            let mut rec = seed("sch00088", None, Some(now + 500), Some(now + 500));
            rec.status = ScheduleStatus::Ran;
            repo.create(&rec).unwrap();
        }
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        assert_eq!(mgr.pending_work_handle().load(Ordering::SeqCst), 0);

        let err = mgr.pause("sch00088").await.unwrap_err();
        assert!(err.contains("ran"), "unexpected error: {err}");
        let rec = ScheduleRepository::new(&db)
            .get("sch00088")
            .unwrap()
            .unwrap();
        assert_eq!(rec.status, ScheduleStatus::Ran, "Ran must not be clobbered");
        assert_eq!(mgr.pending_work_handle().load(Ordering::SeqCst), 0);
        mgr.shutdown().await;
    }

    // ---- goal validation + evaluator resolution at create time -------------

    /// A direct-API config fixture (mirrors the goals-manager test fixture) so
    /// goal-bearing schedules can resolve an evaluator.
    fn config_anthropic() -> Arc<crate::config::AgentConfig> {
        let mut cfg = crate::config::AgentConfig::default();
        cfg.llm.provider = Some("anthropic".to_string());
        cfg.llm.anthropic.api_key = Some("sk-ant-test".to_string());
        cfg.llm.anthropic.model = Some("claude-haiku-4-5".to_string());
        Arc::new(cfg)
    }

    /// `HostServices` carrying `agent_config` — the create-side goal path reads
    /// the evaluator config from here (production always has it).
    fn services_with_config(
        db: &Database,
        config: Arc<crate::config::AgentConfig>,
    ) -> HostServices {
        HostServices::new(Arc::new(db.clone())).with_agent_config(config)
    }

    fn goal_args(condition: &str) -> CreateScheduleArgs {
        let mut args = base_args();
        args.goal_condition = Some(condition.to_string());
        args
    }

    #[tokio::test]
    async fn create_goal_requires_agent_config() {
        // No services ⇒ no agent config ⇒ goal evaluation is unavailable.
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);

        let err = mgr
            .create(goal_args("the report is posted"))
            .await
            .unwrap_err();
        assert!(
            err.contains("goal evaluation unavailable"),
            "unexpected error: {err}"
        );
        // Nothing persisted, nothing counted.
        assert!(mgr.list().await.unwrap().is_empty());
        assert!(!mgr.has_pending_work());
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn create_goal_rejects_acp_evaluator() {
        // An ACP active provider cannot act as an evaluator — fail fast.
        let db = Database::open_in_memory().unwrap();
        let mut cfg = crate::config::AgentConfig::default();
        cfg.llm.provider = Some("claude-code".to_string());
        let services = services_with_config(&db, Arc::new(cfg));
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, Some(services));

        let err = mgr.create(goal_args("done")).await.unwrap_err();
        assert!(
            err.contains("ACP") && err.to_lowercase().contains("direct-api"),
            "unexpected error: {err}"
        );
        assert!(mgr.list().await.unwrap().is_empty());
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn create_goal_rejects_empty_and_over_length_condition() {
        let db = Database::open_in_memory().unwrap();
        let services = services_with_config(&db, config_anthropic());
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, Some(services));

        let err = mgr.create(goal_args("   \n\t ")).await.unwrap_err();
        assert!(err.contains("empty"), "unexpected error: {err}");

        let too_long = "x".repeat(GOAL_CONDITION_MAX_CHARS + 1);
        let err = mgr.create(goal_args(&too_long)).await.unwrap_err();
        assert!(err.contains("too long"), "unexpected error: {err}");

        // Exactly at the limit is allowed and resolves.
        let at_limit = "y".repeat(GOAL_CONDITION_MAX_CHARS);
        assert!(mgr.create(goal_args(&at_limit)).await.is_ok());
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn create_goal_resolves_and_persists_evaluator() {
        let db = Database::open_in_memory().unwrap();
        let services = services_with_config(&db, config_anthropic());
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, Some(services));

        let id = mgr
            .create(goal_args("  the PR is merged  "))
            .await
            .expect("goal schedule created");

        let rec = mgr
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == id.0)
            .unwrap();
        // Condition is trimmed; the resolved (provider, model) are persisted.
        assert_eq!(rec.goal_condition.as_deref(), Some("the PR is merged"));
        assert_eq!(rec.evaluator_provider.as_deref(), Some("anthropic"));
        assert_eq!(rec.evaluator_model.as_deref(), Some("claude-haiku-4-5"));
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn create_goal_honors_explicit_evaluator_provider_and_model() {
        // Explicit (provider, model) override the active provider and persist.
        let db = Database::open_in_memory().unwrap();
        let mut cfg = crate::config::AgentConfig::default();
        cfg.llm.provider = Some("anthropic".to_string());
        cfg.llm.anthropic.api_key = Some("sk-ant".to_string());
        cfg.llm.anthropic.model = Some("claude-haiku-4-5".to_string());
        cfg.llm.openrouter.api_key = Some("sk-or".to_string());
        cfg.llm.openrouter.model = Some("config/model".to_string());
        let services = services_with_config(&db, Arc::new(cfg));
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, Some(services));

        let mut args = goal_args("done");
        args.evaluator_provider = Some("openrouter".to_string());
        args.evaluator_model = Some("explicit/model".to_string());
        let id = mgr.create(args).await.expect("created");

        let rec = mgr
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == id.0)
            .unwrap();
        assert_eq!(rec.evaluator_provider.as_deref(), Some("openrouter"));
        assert_eq!(rec.evaluator_model.as_deref(), Some("explicit/model"));
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn create_without_goal_ignores_config_absence() {
        // A goal-less create still works with no services (zero behavior change).
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let id = mgr.create(base_args()).await.expect("created");
        let rec = mgr
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == id.0)
            .unwrap();
        assert!(rec.goal_condition.is_none());
        assert!(rec.evaluator_provider.is_none());
        mgr.shutdown().await;
    }
}
