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
//! ## Browser policy (P1)
//!
//! - `none`  → `borrow_proxy = false` (no browser routing).
//! - `live`  → `borrow_proxy = true` (borrow the creator session's most recent
//!   sidebar proxy; if none exists `browser_*` tools fail per-call — full
//!   defer/skip semantics land in P4).
//! - `headless` → the run ends immediately as `Error("headless policy lands in
//!   P4")`; a bound headless browser is a P4 concern.

use crate::agent_exec::{run_agent_once, AgentExecRequest};
use crate::schedules::events::ScheduleEvents;
use crate::wasm::services::HostServices;
use nevoflux_storage::models::current_timestamp;
use nevoflux_storage::models::schedule::{ScheduleRecord, ScheduleRunStatus};
use nevoflux_storage::repositories::ScheduleRepository;
use nevoflux_storage::Database;

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
    /// The persisted run status (`Ok` or `Error` in P1).
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
/// | `catchup`). See the module docs for the stub/production split.
pub async fn execute_run(
    services: &Option<HostServices>,
    events: &ScheduleEvents,
    db: &Database,
    rec: &ScheduleRecord,
    fire_kind: &str,
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

    // Headless browser policy is a P4 concern; refuse the run cleanly for now.
    if rec.browser_policy == "headless" {
        return end_run(
            &repo,
            events,
            rec,
            run_id,
            ScheduleRunStatus::Error,
            Some("headless policy lands in P4".to_string()),
            None,
        )
        .await;
    }

    let user_message = build_user_message(rec, fire_kind, services.as_ref()).await;

    // Stub path: no services wired (unit tests) → immediate ok, no text.
    let services = match services.as_ref() {
        Some(s) => s,
        None => {
            return end_run(
                &repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Ok,
                None,
                None,
            )
            .await;
        }
    };

    // Production path. Session scoping: reuse the creator's session so artifacts
    // and (for `live`) the borrowed sidebar proxy resolve; fall back to a
    // per-schedule synthetic session id when the schedule was created without a
    // creator session.
    let session_id = rec
        .creator_session_id
        .clone()
        .unwrap_or_else(|| format!("schedule:{}", rec.id));
    let borrow_proxy = rec.browser_policy == "live";
    let mode = crate::loops::manager::db_str_to_agent_mode(&rec.mode);

    let req = AgentExecRequest {
        session_id,
        mode,
        user_message,
        forbidden_tools: forbidden_tools(),
        forbidden_prefixes: Vec::new(),
        unattended: true,
        iteration_loop_id: None,
        borrow_proxy,
        bound_browser: None,
        history: Vec::new(),
        // Token budgeting is a P3 concern (`max_tokens_per_run`).
        token_budget: None,
    };

    match run_agent_once(services, req).await {
        Ok(outcome) => {
            end_run(
                &repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Ok,
                None,
                Some(outcome.text),
            )
            .await
        }
        Err(e) => {
            end_run(
                &repo,
                events,
                rec,
                run_id,
                ScheduleRunStatus::Error,
                Some(e),
                None,
            )
            .await
        }
    }
}

/// Persist `run_end`, emit the `run_end` event, and build the [`RunResult`].
/// Centralizes the exit paths (headless refusal, stub ok, production ok/error)
/// so the persisted row and the emitted event never diverge.
async fn end_run(
    repo: &ScheduleRepository<'_>,
    events: &ScheduleEvents,
    rec: &ScheduleRecord,
    run_id: i64,
    status: ScheduleRunStatus,
    error: Option<String>,
    final_text: Option<String>,
) -> RunResult {
    let end = current_timestamp();
    let _ = repo.record_run_end(
        run_id,
        end,
        status,
        error.as_deref(),
        final_text.as_deref(),
        None,
        None,
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
        let res = execute_run(&None, &events, &db, &rec, "scheduled").await;

        assert!(res.ok);
        assert_eq!(res.status, ScheduleRunStatus::Ok);
        assert!(res.final_text.is_none());

        let runs = repo.list_runs("sch00001", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, ScheduleRunStatus::Ok);
        assert_eq!(runs[0].fire_kind, "scheduled");
    }

    #[tokio::test]
    async fn headless_policy_run_errors_immediately() {
        let db = Database::open_in_memory().unwrap();
        let repo = ScheduleRepository::new(&db);
        let mut rec = sample("sch00002");
        rec.browser_policy = "headless".into();
        repo.create(&rec).unwrap();

        let events = ScheduleEvents::new(None);
        let res = execute_run(&None, &events, &db, &rec, "manual").await;

        assert!(!res.ok);
        assert_eq!(res.status, ScheduleRunStatus::Error);
        assert!(res.error.as_deref().unwrap().contains("headless"));

        let runs = repo.list_runs("sch00002", 10).unwrap();
        assert_eq!(runs[0].status, ScheduleRunStatus::Error);
    }
}
