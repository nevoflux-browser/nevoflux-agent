//! LLM-callable tool handlers for `/goal` (Task 2.3).
//!
//! Three tools share one dispatcher, mirroring `crate::schedules::tools`'
//! shape so registration on both dispatch surfaces (builtin-wasm
//! `HostFunctions` for direct-API providers, `mcp_tool_executor` for
//! ACP-bridge providers) is a single name-list per call site.
//!
//! Unlike `execute_schedule_tool`, this dispatcher takes a bare
//! `session_id: &str` rather than a context struct — goals have no
//! "unattended run" branch inside the tool itself. The one unattended-run
//! restriction (`goal_set` must not hijack a session's goal from inside a
//! `/loop` iteration or a schedule's own fire) is enforced entirely at the
//! `mcp_tool_executor` iteration gate, the same call site that rejects
//! `loop_create` / `schedule_create`.

use crate::goals::manager::GoalManager;
use serde_json::{json, Value};

/// Dispatch one of the three `goal_*` tools. `args` is the tool call's JSON
/// arguments (already parsed — callers on the direct-API surface parse the
/// args string once before calling this).
pub async fn execute_goal_tool(
    name: &str,
    args: &Value,
    session_id: &str,
    mgr: &GoalManager,
) -> Result<Value, String> {
    match name {
        "goal_set" => goal_set(args, session_id, mgr).await,
        "goal_status" => mgr.status(session_id).await,
        "goal_clear" => goal_clear(session_id, mgr).await,
        _ => Err(format!("unknown goal tool: {name}")),
    }
}

async fn goal_set(args: &Value, session_id: &str, mgr: &GoalManager) -> Result<Value, String> {
    let condition = args
        .get("condition")
        .and_then(|v| v.as_str())
        .ok_or("condition (string) required")?
        .to_string();
    let evaluator_provider = args
        .get("evaluator_provider")
        .and_then(|v| v.as_str())
        .map(String::from);
    let evaluator_model = args
        .get("evaluator_model")
        .and_then(|v| v.as_str())
        .map(String::from);
    let max_turns = args.get("max_turns").and_then(|v| v.as_i64());

    mgr.set(
        session_id,
        &condition,
        evaluator_provider,
        evaluator_model,
        max_turns,
    )
    .await?;

    // `set` returns the freshly-created GoalRecord, but `status`'s JSON
    // shape is the LLM-facing contract (spec: goal_set returns "the status
    // JSON for the new goal") — reuse it rather than re-deriving the same
    // shape by hand.
    mgr.status(session_id).await
}

async fn goal_clear(session_id: &str, mgr: &GoalManager) -> Result<Value, String> {
    let cleared = mgr.clear(session_id).await?;
    Ok(json!({ "cleared": cleared }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;
    use nevoflux_storage::repositories::SessionRepository;
    use nevoflux_storage::{CreateSessionParams, Database};
    use std::sync::Arc;

    fn seed_session(db: &Database, id: &str) {
        SessionRepository::new(db)
            .create(CreateSessionParams::new().with_id(id))
            .unwrap();
    }

    fn config_anthropic() -> Arc<AgentConfig> {
        let mut cfg = AgentConfig::default();
        cfg.llm.provider = Some("anthropic".to_string());
        cfg.llm.anthropic.api_key = Some("sk-ant-test".to_string());
        cfg.llm.anthropic.model = Some("claude-haiku-4-5".to_string());
        Arc::new(cfg)
    }

    fn mgr_with_session(session_id: &str) -> (Database, Arc<GoalManager>) {
        let db = Database::open_in_memory().unwrap();
        seed_session(&db, session_id);
        let mgr = GoalManager::new(db.clone(), None, config_anthropic());
        (db, mgr)
    }

    #[tokio::test]
    async fn goal_set_happy_path_returns_full_status_json() {
        let (_db, mgr) = mgr_with_session("sess-1");

        let result = execute_goal_tool(
            "goal_set",
            &json!({ "condition": "the PR is merged", "max_turns": 15 }),
            "sess-1",
            &mgr,
        )
        .await
        .unwrap();

        assert_eq!(result["condition"], json!("the PR is merged"));
        assert_eq!(result["status"], json!("active"));
        assert_eq!(result["turns_used"], json!(0));
        assert_eq!(result["max_turns"], json!(15));
        assert_eq!(result["last_reason"], json!(null));
        assert_eq!(result["evaluator"]["provider"], json!("anthropic"));
        assert_eq!(result["evaluator"]["model"], json!("claude-haiku-4-5"));
        assert!(
            result.get("achieved_at").is_none(),
            "no achieved_at on a freshly-set active goal"
        );
    }

    #[tokio::test]
    async fn goal_set_defaults_max_turns_when_absent() {
        let (_db, mgr) = mgr_with_session("sess-1");

        let result = execute_goal_tool("goal_set", &json!({ "condition": "done" }), "sess-1", &mgr)
            .await
            .unwrap();

        assert_eq!(result["max_turns"], json!(20));
    }

    #[tokio::test]
    async fn goal_set_missing_condition_key_is_actionable_error() {
        let (_db, mgr) = mgr_with_session("sess-1");

        let err = execute_goal_tool("goal_set", &json!({}), "sess-1", &mgr)
            .await
            .unwrap_err();
        assert!(err.contains("condition"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn goal_set_empty_condition_surfaces_manager_validation_error() {
        let (_db, mgr) = mgr_with_session("sess-1");

        let err = execute_goal_tool("goal_set", &json!({ "condition": "   " }), "sess-1", &mgr)
            .await
            .unwrap_err();
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn goal_set_over_length_condition_surfaces_manager_validation_error() {
        let (_db, mgr) = mgr_with_session("sess-1");

        let too_long = "x".repeat(4001);
        let err = execute_goal_tool(
            "goal_set",
            &json!({ "condition": too_long }),
            "sess-1",
            &mgr,
        )
        .await
        .unwrap_err();
        assert!(err.contains("too long"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn goal_status_none_shape_when_no_goal() {
        let (_db, mgr) = mgr_with_session("sess-1");

        let result = execute_goal_tool("goal_status", &json!({}), "sess-1", &mgr)
            .await
            .unwrap();
        assert_eq!(result, json!({ "status": "none" }));
    }

    #[tokio::test]
    async fn goal_status_reflects_active_goal() {
        let (_db, mgr) = mgr_with_session("sess-1");
        execute_goal_tool("goal_set", &json!({ "condition": "done" }), "sess-1", &mgr)
            .await
            .unwrap();

        let result = execute_goal_tool("goal_status", &json!({}), "sess-1", &mgr)
            .await
            .unwrap();
        assert_eq!(result["status"], json!("active"));
        assert_eq!(result["condition"], json!("done"));
    }

    #[tokio::test]
    async fn goal_clear_both_branches() {
        let (_db, mgr) = mgr_with_session("sess-1");

        // No active goal yet — clear reports false.
        let result = execute_goal_tool("goal_clear", &json!({}), "sess-1", &mgr)
            .await
            .unwrap();
        assert_eq!(result, json!({ "cleared": false }));

        execute_goal_tool("goal_set", &json!({ "condition": "done" }), "sess-1", &mgr)
            .await
            .unwrap();

        // Now there is one — clear reports true.
        let result = execute_goal_tool("goal_clear", &json!({}), "sess-1", &mgr)
            .await
            .unwrap();
        assert_eq!(result, json!({ "cleared": true }));

        // Idempotent-ish: a second clear finds nothing active.
        let result = execute_goal_tool("goal_clear", &json!({}), "sess-1", &mgr)
            .await
            .unwrap();
        assert_eq!(result, json!({ "cleared": false }));
    }

    #[tokio::test]
    async fn unknown_tool_name_errors() {
        let (_db, mgr) = mgr_with_session("sess-1");

        let err = execute_goal_tool("goal_bogus", &json!({}), "sess-1", &mgr)
            .await
            .unwrap_err();
        assert!(err.contains("unknown goal tool"), "unexpected error: {err}");
    }
}
