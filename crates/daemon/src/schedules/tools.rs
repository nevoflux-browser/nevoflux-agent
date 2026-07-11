//! LLM-callable tool handlers for `/schedule` (Task 1.6, P1 scope).
//!
//! Seven tools share one dispatcher, mirroring `crate::loops::tools`' shape
//! so registration on both dispatch surfaces (builtin-wasm `HostFunctions`
//! for direct-API providers, `mcp_tool_executor` for ACP-bridge providers)
//! is a single name-list per call site.

use crate::schedules::manager::{CreateScheduleArgs, ScheduleManager};
use serde_json::{json, Value};

/// Execution context for a schedule tool call.
///
/// `is_unattended: true` means the call originates from inside an unattended
/// run (a `/loop` iteration, or a schedule's own scheduled run) — those
/// contexts have nobody present to confirm a new schedule's name/cadence, so
/// `schedule_create` is rejected there (mirrors `loop_create`'s no-nesting
/// rule and the `runner::forbidden_tools` list).
#[derive(Debug, Clone)]
pub struct ScheduleToolContext {
    pub session_id: String,
    pub is_unattended: bool,
}

/// Dispatch one of the seven `schedule_*` tools. `args` is the tool call's
/// JSON arguments (already parsed — callers on the direct-API surface parse
/// the args string once before calling this).
pub async fn execute_schedule_tool(
    name: &str,
    args: &Value,
    ctx: &ScheduleToolContext,
    mgr: &ScheduleManager,
) -> Result<Value, String> {
    match name {
        "schedule_create" => schedule_create(args, ctx, mgr).await,
        "schedule_list" => schedule_list(mgr).await,
        "schedule_cancel" => schedule_cancel(args, mgr).await,
        "schedule_pause" => schedule_pause(args, mgr).await,
        "schedule_resume" => schedule_resume(args, mgr).await,
        "schedule_run_now" => schedule_run_now(args, mgr).await,
        "schedule_runs" => schedule_runs(args, mgr).await,
        _ => Err(format!("unknown schedule tool: {name}")),
    }
}

async fn schedule_create(
    args: &Value,
    ctx: &ScheduleToolContext,
    mgr: &ScheduleManager,
) -> Result<Value, String> {
    if ctx.is_unattended {
        return Err("schedule_create is not available inside unattended runs".into());
    }

    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("name (string) required")?
        .to_string();

    // XOR trigger, checked here with the LLM-facing arg names (`cron`/`at`)
    // rather than deferring to the manager's `cron_expr`/`at_ts`-worded error.
    let cron_expr = args.get("cron").and_then(|v| v.as_str()).map(String::from);
    let at_val = args.get("at");
    if cron_expr.is_some() == at_val.is_some() {
        return Err("exactly one of cron or at is required".into());
    }
    let at_ts = match at_val {
        Some(v) => Some(parse_at(v)?),
        None => None,
    };

    // XOR prompt body — arg names already match the manager's internal field
    // names, so its "exactly one of prompt_text or wrapped_skill" error is
    // left to surface as-is.
    let prompt_text = args
        .get("prompt_text")
        .and_then(|v| v.as_str())
        .map(String::from);
    // Accept either a JSON-stringified blob (the schema's declared shape) or
    // an already-parsed object (older/direct callers), mirroring loop_create.
    let wrapped_skill = args.get("wrapped_skill").map(|v| {
        v.as_str()
            .map(String::from)
            .unwrap_or_else(|| v.to_string())
    });

    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .map(crate::loops::manager::db_str_to_agent_mode)
        .unwrap_or(nevoflux_builtin_wasm::AgentMode::Chat);

    let browser_policy = args
        .get("browser")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();

    let on_unavailable = args
        .get("on_unavailable")
        .and_then(|v| v.as_str())
        .map(String::from);
    let headless_profile = args
        .get("headless_profile")
        .and_then(|v| v.as_str())
        .map(String::from);
    let catch_up = args
        .get("catch_up")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Stored in P1, evaluated in P3 (goal engine) — see manager.rs docs.
    let goal_condition = args
        .get("goal_condition")
        .and_then(|v| v.as_str())
        .map(String::from);
    let goal_max_turns = args.get("goal_max_turns").and_then(|v| v.as_i64());
    let max_tokens_per_run = args.get("max_tokens_per_run").and_then(|v| v.as_i64());
    let evaluator_model = args
        .get("evaluator_model")
        .and_then(|v| v.as_str())
        .map(String::from);
    let evaluator_provider = args
        .get("evaluator_provider")
        .and_then(|v| v.as_str())
        .map(String::from);

    let creator_session_id = if ctx.session_id.is_empty() {
        None
    } else {
        Some(ctx.session_id.clone())
    };

    let id = mgr
        .create(CreateScheduleArgs {
            creator_session_id,
            name,
            cron_expr,
            at_ts,
            prompt_text,
            wrapped_skill,
            mode,
            browser_policy,
            on_unavailable,
            headless_profile,
            catch_up,
            goal_condition,
            goal_max_turns,
            max_tokens_per_run,
            evaluator_model,
            evaluator_provider,
        })
        .await?;

    // ScheduleManager::create only returns the id; look the record back up
    // for next_fire_at (there is no manager-level `get`, only `list`/`runs`).
    let next_fire_at = mgr
        .list()
        .await?
        .into_iter()
        .find(|r| r.id == id.0)
        .and_then(|r| r.next_fire_at);
    Ok(json!({ "schedule_id": id.0, "next_fire_at": next_fire_at }))
}

async fn schedule_list(mgr: &ScheduleManager) -> Result<Value, String> {
    let rows = mgr.list().await?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "schedule_id": r.id,
                "name": r.name,
                "status": r.status.as_str(),
                "cron": r.cron_expr,
                "at": r.at_ts,
                "next_fire_at": r.next_fire_at,
                "last_run_status": r.last_run_status,
                "last_run_at": r.last_run_at,
                "run_count": r.run_count,
                "browser": r.browser_policy,
                "mode": r.mode,
            })
        })
        .collect();
    Ok(json!(out))
}

fn require_schedule_id(args: &Value) -> Result<&str, String> {
    args.get("schedule_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "schedule_id (string) required".to_string())
}

async fn schedule_cancel(args: &Value, mgr: &ScheduleManager) -> Result<Value, String> {
    let id = require_schedule_id(args)?;
    mgr.cancel(id).await?;
    Ok(json!({ "cancelled": true }))
}

async fn schedule_pause(args: &Value, mgr: &ScheduleManager) -> Result<Value, String> {
    let id = require_schedule_id(args)?;
    mgr.pause(id).await?;
    Ok(json!({ "status": "paused" }))
}

async fn schedule_resume(args: &Value, mgr: &ScheduleManager) -> Result<Value, String> {
    let id = require_schedule_id(args)?.to_string();
    mgr.resume(&id).await?;
    let next_fire_at = mgr
        .list()
        .await?
        .into_iter()
        .find(|r| r.id == id)
        .and_then(|r| r.next_fire_at);
    Ok(json!({ "status": "active", "next_fire_at": next_fire_at }))
}

async fn schedule_run_now(args: &Value, mgr: &ScheduleManager) -> Result<Value, String> {
    let id = require_schedule_id(args)?;
    mgr.run_now(id).await?;
    Ok(json!({ "started": true }))
}

async fn schedule_runs(args: &Value, mgr: &ScheduleManager) -> Result<Value, String> {
    let id = require_schedule_id(args)?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(20)
        .clamp(1, 100);
    // `final_text` is deliberately excluded by default — too big for an LLM
    // tool result (history is for browsing, not re-consuming the last output).
    // The Jobs panel opts in via `include_final_text: true` so completed cards
    // and run-history rows can render the run's output; the LLM never sets it.
    let include_final_text = args
        .get("include_final_text")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let rows = mgr.runs(id, limit).await?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut obj = json!({
                "run_id": r.id,
                "started_at": r.started_at,
                "ended_at": r.ended_at,
                "status": r.status.as_str(),
                "fire_kind": r.fire_kind,
                "error": r.error_message,
                "tokens_used": r.tokens_used,
                "goal_turns": r.goal_turns,
            });
            if include_final_text {
                obj["final_text"] = json!(r.final_text);
            }
            obj
        })
        .collect();
    Ok(json!(out))
}

/// Accept an RFC3339 timestamp with offset, or a bare unix-seconds integer.
fn parse_at(v: &Value) -> Result<i64, String> {
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    if let Some(s) = v.as_str() {
        if let Ok(n) = s.parse::<i64>() {
            return Ok(n);
        }
        return chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.timestamp())
            .map_err(|e| {
                format!(
                    "invalid `at` timestamp {s:?}: {e} \
                     (expected RFC3339 with offset, or unix seconds)"
                )
            });
    }
    Err("`at` must be an RFC3339 string or a unix-seconds integer".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_storage::models::current_timestamp;
    use nevoflux_storage::models::CreateSessionParams;
    use nevoflux_storage::{Database, Storage};
    use std::time::Duration;

    fn ctx(session_id: &str, is_unattended: bool) -> ScheduleToolContext {
        ScheduleToolContext {
            session_id: session_id.to_string(),
            is_unattended,
        }
    }

    fn base_create_args() -> Value {
        json!({
            "name": "nightly report",
            "cron": "0 9 * * *",
            "prompt_text": "generate the nightly report",
        })
    }

    /// `creator_session_id` is FK-constrained to `sessions(id)`, so any test
    /// that creates a schedule with a non-empty `ctx.session_id` needs the
    /// session row to actually exist. Returns the `Database` handle to pass
    /// into `ScheduleManager::start_with_bus`.
    fn db_with_session(id: &str) -> Database {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id(id).with_title("t"))
            .unwrap();
        storage.database().clone()
    }

    #[tokio::test]
    async fn create_list_pause_resume_cancel_roundtrip() {
        let db = db_with_session("s1");
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", false);

        let created = execute_schedule_tool("schedule_create", &base_create_args(), &c, &mgr)
            .await
            .unwrap();
        let id = created["schedule_id"].as_str().unwrap().to_string();
        assert!(created["next_fire_at"].is_i64());

        let listed = execute_schedule_tool("schedule_list", &json!({}), &c, &mgr)
            .await
            .unwrap();
        let arr = listed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["schedule_id"], id);
        assert_eq!(arr[0]["status"], "active");
        assert_eq!(arr[0]["name"], "nightly report");
        assert_eq!(arr[0]["mode"], "chat");
        assert_eq!(arr[0]["browser"], "none");
        assert_eq!(arr[0]["cron"], "0 9 * * *");

        let paused =
            execute_schedule_tool("schedule_pause", &json!({ "schedule_id": id }), &c, &mgr)
                .await
                .unwrap();
        assert_eq!(paused["status"], "paused");

        let resumed =
            execute_schedule_tool("schedule_resume", &json!({ "schedule_id": id }), &c, &mgr)
                .await
                .unwrap();
        assert_eq!(resumed["status"], "active");
        assert!(resumed["next_fire_at"].is_i64());

        let cancelled =
            execute_schedule_tool("schedule_cancel", &json!({ "schedule_id": id }), &c, &mgr)
                .await
                .unwrap();
        assert_eq!(cancelled["cancelled"], true);

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn sub_hourly_cron_rejected_with_message() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", false);

        let mut args = base_create_args();
        args["cron"] = json!("*/30 * * * *");
        let err = execute_schedule_tool("schedule_create", &args, &c, &mgr)
            .await
            .unwrap_err();
        assert!(
            err.contains("more often") || err.to_lowercase().contains("loop"),
            "unexpected error: {err}"
        );
        assert!(mgr.list().await.unwrap().is_empty());

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn at_accepts_rfc3339_and_unix_seconds() {
        let db = db_with_session("s1");
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", false);

        let future = current_timestamp() + 100_000;
        let rfc3339 = chrono::DateTime::from_timestamp(future, 0)
            .unwrap()
            .to_rfc3339();

        let a1 = json!({ "name": "one-off a", "at": rfc3339, "prompt_text": "p" });
        let r1 = execute_schedule_tool("schedule_create", &a1, &c, &mgr)
            .await
            .unwrap();
        assert!(r1["schedule_id"].as_str().is_some());

        let a2 = json!({ "name": "one-off b", "at": future, "prompt_text": "p" });
        let r2 = execute_schedule_tool("schedule_create", &a2, &c, &mgr)
            .await
            .unwrap();
        assert!(r2["schedule_id"].as_str().is_some());

        assert_eq!(mgr.list().await.unwrap().len(), 2);

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn schedule_create_blocked_when_unattended() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", true);

        let err = execute_schedule_tool("schedule_create", &base_create_args(), &c, &mgr)
            .await
            .unwrap_err();
        assert!(err.contains("unattended"), "unexpected error: {err}");
        assert!(mgr.list().await.unwrap().is_empty());

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn runs_listing_reflects_manual_fire() {
        let db = db_with_session("s1");
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", false);

        let created = execute_schedule_tool("schedule_create", &base_create_args(), &c, &mgr)
            .await
            .unwrap();
        let id = created["schedule_id"].as_str().unwrap().to_string();

        let started =
            execute_schedule_tool("schedule_run_now", &json!({ "schedule_id": id }), &c, &mgr)
                .await
                .unwrap();
        assert_eq!(started["started"], true);

        // The stub run fires via tokio::spawn (fire-and-forget); poll until
        // it lands rather than assuming it's synchronous.
        let mut runs = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let r = execute_schedule_tool("schedule_runs", &json!({ "schedule_id": id }), &c, &mgr)
                .await
                .unwrap();
            if r.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                runs = r;
                break;
            }
        }
        let arr = runs.as_array().expect("runs should have been recorded");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["status"], "ok");
        assert_eq!(arr[0]["fire_kind"], "manual");
        assert!(arr[0]["run_id"].is_i64());
        assert!(arr[0]["started_at"].is_i64());
        assert!(arr[0]["ended_at"].is_i64());
        assert!(
            arr[0].get("final_text").is_none(),
            "final_text must not be included in the tool result"
        );

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn schedule_runs_includes_final_text_when_flagged() {
        let db = db_with_session("s1");
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", false);

        let created = execute_schedule_tool("schedule_create", &base_create_args(), &c, &mgr)
            .await
            .unwrap();
        let id = created["schedule_id"].as_str().unwrap().to_string();

        execute_schedule_tool("schedule_run_now", &json!({ "schedule_id": id }), &c, &mgr)
            .await
            .unwrap();

        let mut runs = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let r = execute_schedule_tool(
                "schedule_runs",
                &json!({ "schedule_id": id, "include_final_text": true }),
                &c,
                &mgr,
            )
            .await
            .unwrap();
            if r.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                runs = r;
                break;
            }
        }
        let arr = runs.as_array().expect("runs should have been recorded");
        assert_eq!(arr.len(), 1);
        // With the flag set, the panel-facing result carries the `final_text`
        // key (value may be null for a stub run, but the key is present).
        assert!(
            arr[0].get("final_text").is_some(),
            "final_text must be included when include_final_text is true"
        );

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn missing_schedule_id_is_actionable_error() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", false);

        let err = execute_schedule_tool("schedule_cancel", &json!({}), &c, &mgr)
            .await
            .unwrap_err();
        assert!(err.contains("schedule_id"), "unexpected error: {err}");

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn create_requires_name_and_exactly_one_trigger() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", false);

        let err = execute_schedule_tool(
            "schedule_create",
            &json!({ "cron": "0 9 * * *", "prompt_text": "p" }),
            &c,
            &mgr,
        )
        .await
        .unwrap_err();
        assert!(err.contains("name"), "unexpected error: {err}");

        let err = execute_schedule_tool(
            "schedule_create",
            &json!({ "name": "x", "prompt_text": "p" }),
            &c,
            &mgr,
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("cron") || err.contains("at"),
            "unexpected error: {err}"
        );

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_tool_name_errors() {
        let db = Database::open_in_memory().unwrap();
        let mgr = ScheduleManager::start_with_bus(db.clone(), None, None);
        let c = ctx("s1", false);

        let err = execute_schedule_tool("schedule_bogus", &json!({}), &c, &mgr)
            .await
            .unwrap_err();
        assert!(err.contains("unknown schedule tool"));

        mgr.shutdown().await;
    }
}
