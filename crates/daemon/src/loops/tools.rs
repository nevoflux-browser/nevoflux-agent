//! LLM-callable tool handlers for /loop (spec §10).
//!
//! Five tools share one dispatcher so registration in Phase 9
//! (builtin-wasm + mcp_tool_executor) is a single name-list per call site.

use crate::loops::manager::{CreateLoopArgs, LoopManager};
use crate::loops::tool_classes::is_forbidden_in_iteration;
use crate::loops::types::{GateKind, GateSpec, LoopId};
use nevoflux_storage::connection::Database;
use nevoflux_storage::models::current_timestamp;
use nevoflux_storage::repositories::LoopRepository;
use serde_json::{json, Value};

/// Execution context for a tool call.
///
/// `is_iteration: false` ⇒ main-session call (sidebar-driven LLM).
/// `is_iteration: true`  ⇒ call from inside a loop iteration's AgentRunner.
/// `own_loop_id` is set only when `is_iteration` so dispatch can enforce
/// "iterations may only cancel their own loop_id" and similar rules.
#[derive(Debug, Clone)]
pub struct ToolCallContext {
    pub session_id: String,
    pub is_iteration: bool,
    pub own_loop_id: Option<LoopId>,
}

pub async fn execute_loop_tool(
    name: &str,
    args: &Value,
    ctx: &ToolCallContext,
    mgr: &LoopManager,
    db: &Database,
) -> Result<Value, String> {
    if ctx.is_iteration && is_forbidden_in_iteration(name) {
        return Err(format!("{name} is forbidden inside loop iterations"));
    }
    match name {
        "loop_create" => loop_create(args, ctx, mgr).await,
        "loop_list" => loop_list(args, ctx, db),
        "loop_cancel" => loop_cancel(args, ctx, mgr).await,
        "loop_scratchpad_get" => scratchpad_get(args, ctx, db),
        "loop_scratchpad_set" => scratchpad_set(args, ctx, mgr, db).await,
        _ => Err(format!("unknown loop tool: {name}")),
    }
}

/// Parse the optional `gate` tool arg into a `GateSpec`.
///
/// Input shape: `{kind: "http"|"bash"|"event", url?, extract?, command?,
/// path?, equals?}`. `kind` selects the `GateKind`; every other field is
/// forwarded verbatim as `GateSpec::spec_json` — the evaluator (`loops::gate`)
/// is what actually interprets those fields per-kind, so this parser stays
/// agnostic to which ones a given kind needs. Trigger-compat validation
/// happens later, in `LoopManager::create_loop`, once the trigger has also
/// been parsed.
fn parse_gate_arg(args: &Value) -> Result<Option<GateSpec>, String> {
    let Some(gate) = args.get("gate") else {
        return Ok(None);
    };
    if gate.is_null() {
        return Ok(None);
    }
    let kind_str = gate
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or("gate.kind is required (one of \"http\", \"bash\", \"event\")")?;
    let kind = GateKind::from_db_str(kind_str)
        .filter(|k| !matches!(k, GateKind::None))
        .ok_or_else(|| format!("unknown gate.kind: {kind_str} (expected http, bash, or event)"))?;
    let mut spec_json = gate.clone();
    if let Some(obj) = spec_json.as_object_mut() {
        obj.remove("kind");
    }
    Ok(Some(GateSpec { kind, spec_json }))
}

/// Parse the optional `verify` tool arg into `CreateLoopArgs.verify_check`.
///
/// Input shape: `{tool?, matches, negate?}` — the same `GoalCheck` shape
/// `/goal`'s `check` arg takes (W5 spec §verify). Validated eagerly via
/// `goals::check::parse_check` (wrapped in a `{"check": ...}` envelope,
/// the shape it expects) so a missing `matches` or invalid regex fails at
/// create time with a clear error, mirroring how `parse_gate_arg` validates
/// `gate.kind` above. Only the RAW object is persisted — not the
/// parsed-then-re-serialized form — because `IterationExecutor::
/// finalize_iteration` re-wraps and re-parses `verify_check` the same way
/// on every iteration (see `executor.rs`), so the raw string is the single
/// source of truth for the on-disk shape.
fn parse_verify_arg(args: &Value) -> Result<Option<String>, String> {
    let Some(verify) = args.get("verify") else {
        return Ok(None);
    };
    if verify.is_null() {
        return Ok(None);
    }
    crate::goals::check::parse_check(&json!({ "check": verify }))?;
    Ok(Some(verify.to_string()))
}

async fn loop_create(
    args: &Value,
    ctx: &ToolCallContext,
    mgr: &LoopManager,
) -> Result<Value, String> {
    if ctx.is_iteration {
        return Err("loop_create cannot be called from inside an iteration".into());
    }
    let trigger_expr_text = args
        .get("trigger_expr")
        .and_then(|v| v.as_str())
        .ok_or("trigger_expr required")?
        .to_string();
    let prompt_text = args
        .get("prompt_text")
        .and_then(|v| v.as_str())
        .map(String::from);
    // Accept either:
    //  - a string (the schema's declared shape — strict-mode-friendly,
    //    model is told to JSON.stringify the {name, args} blob), or
    //  - an object (older direct-API callers may still pass it raw).
    let wrapped_skill = args.get("wrapped_skill").map(|v| {
        v.as_str()
            .map(String::from)
            .unwrap_or_else(|| v.to_string())
    });
    // Optional `mode` arg: one of "chat" | "browser" | "agent". Defaults to
    // Chat (matches `server.rs::parse_agent_mode` semantics).
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .map(crate::loops::manager::db_str_to_agent_mode)
        .unwrap_or(nevoflux_builtin_wasm::AgentMode::Chat);
    let gate = parse_gate_arg(args)?;
    let verify_check = parse_verify_arg(args)?;

    let id = mgr
        .create_loop(CreateLoopArgs {
            session_id: ctx.session_id.clone(),
            trigger_expr_text,
            prompt_text,
            wrapped_skill,
            mode,
            gate,
            verify_check,
        })
        .await?;
    Ok(json!({ "loop_id": id.0 }))
}

fn loop_list(_args: &Value, ctx: &ToolCallContext, db: &Database) -> Result<Value, String> {
    let rows = LoopRepository::new(db)
        .list_by_session(&ctx.session_id)
        .map_err(|e| e.to_string())?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let preview: String = r.scratchpad.chars().take(120).collect();
            json!({
                "loop_id": r.id,
                "state": r.state.as_str(),
                "trigger_expr": r.trigger_expr,
                "iteration_count": r.iteration_count,
                "scratchpad_preview": preview,
            })
        })
        .collect();
    Ok(json!(out))
}

async fn loop_cancel(
    args: &Value,
    ctx: &ToolCallContext,
    mgr: &LoopManager,
) -> Result<Value, String> {
    let target = args
        .get("loop_id")
        .and_then(|v| v.as_str())
        .ok_or("loop_id required")?;
    if ctx.is_iteration {
        match &ctx.own_loop_id {
            Some(own) if own.as_ref() == target => {}
            _ => return Err("iteration may only cancel its own loop_id".into()),
        }
    }
    mgr.cancel_loop(&LoopId(target.to_string()), false).await?;
    Ok(json!({ "cancelled": true }))
}

fn scratchpad_get(args: &Value, ctx: &ToolCallContext, db: &Database) -> Result<Value, String> {
    let target = args
        .get("loop_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| ctx.own_loop_id.as_ref().map(|i| i.0.clone()))
        .ok_or("loop_id required (no current iteration context)")?;
    let rec = LoopRepository::new(db)
        .get(&target)
        .map_err(|e| e.to_string())?
        .ok_or("loop not found")?;
    Ok(json!({ "content": rec.scratchpad, "bytes": rec.scratchpad.len() }))
}

async fn scratchpad_set(
    args: &Value,
    ctx: &ToolCallContext,
    mgr: &LoopManager,
    db: &Database,
) -> Result<Value, String> {
    if !ctx.is_iteration {
        return Err("loop_scratchpad_set is only callable from inside an iteration".into());
    }
    let own = ctx
        .own_loop_id
        .as_ref()
        .ok_or("no own_loop_id in iteration context")?;
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or("content (string) required")?;
    if content.len() > 4096 {
        return Err(format!(
            "content exceeds 4096 bytes (got {})",
            content.len()
        ));
    }
    LoopRepository::new(db)
        .update_scratchpad(&own.0, content, current_timestamp())
        .map_err(|e| e.to_string())?;
    mgr.events()
        .scratchpad_changed(&ctx.session_id, own, content)
        .await;
    Ok(json!({ "bytes_written": content.len() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::manager::LoopManager;
    use nevoflux_storage::models::CreateSessionParams;
    use nevoflux_storage::Storage;

    fn setup() -> (Storage, LoopManager) {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let mgr = LoopManager::start(storage.database().clone());
        (storage, mgr)
    }

    fn ctx(is_iter: bool, own: Option<&str>) -> ToolCallContext {
        ToolCallContext {
            session_id: "s1".into(),
            is_iteration: is_iter,
            own_loop_id: own.map(|s| LoopId(s.into())),
        }
    }

    #[tokio::test]
    async fn loop_create_then_list_includes_it() {
        let (storage, mgr) = setup();
        let res = execute_loop_tool(
            "loop_create",
            &json!({ "trigger_expr": "time:5m", "prompt_text": "x" }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        let id = res.get("loop_id").unwrap().as_str().unwrap().to_string();

        let list = execute_loop_tool(
            "loop_list",
            &json!({}),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("loop_id").unwrap().as_str().unwrap(), id);
        assert_eq!(arr[0].get("state").unwrap().as_str().unwrap(), "pending");
        assert_eq!(
            arr[0].get("trigger_expr").unwrap().as_str().unwrap(),
            "time:5m"
        );
    }

    /// W3 task 4: the `gate` tool arg parses through `parse_gate_arg` and
    /// round-trips through `LoopRepository::get` via the JSON-arg dispatch
    /// path (as opposed to `manager::tests`, which construct `GateSpec`
    /// directly in Rust and never exercise `parse_gate_arg`'s JSON shape).
    #[tokio::test]
    async fn loop_create_with_http_gate_arg_persists_gate() {
        let (storage, mgr) = setup();
        let res = execute_loop_tool(
            "loop_create",
            &json!({
                "trigger_expr": "time:5m",
                "prompt_text": "x",
                "gate": { "kind": "http", "url": "https://x", "extract": "$.v" }
            }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        let id = res.get("loop_id").unwrap().as_str().unwrap().to_string();

        let rec = storage.loops().get(&id).unwrap().unwrap();
        assert_eq!(rec.gate_kind, "http");
        let spec: serde_json::Value =
            serde_json::from_str(rec.gate_spec.as_deref().unwrap()).unwrap();
        assert_eq!(spec["url"], "https://x");
        assert_eq!(spec["extract"], "$.v");
        // `kind` must not leak into spec_json — the evaluator only expects
        // kind-specific fields there.
        assert!(spec.get("kind").is_none());
    }

    /// W5 task 2: the `verify` tool arg parses through `parse_verify_arg`
    /// and round-trips through `LoopRepository::get` via the JSON-arg
    /// dispatch path — mirrors `loop_create_with_http_gate_arg_persists_gate`.
    #[tokio::test]
    async fn loop_create_with_verify_arg_persists_verify_check() {
        let (storage, mgr) = setup();
        let res = execute_loop_tool(
            "loop_create",
            &json!({
                "trigger_expr": "time:5m",
                "prompt_text": "x",
                "verify": { "matches": "OK" }
            }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        let id = res.get("loop_id").unwrap().as_str().unwrap().to_string();

        let rec = storage.loops().get(&id).unwrap().unwrap();
        let verify: serde_json::Value =
            serde_json::from_str(rec.verify_check.as_deref().unwrap()).unwrap();
        assert_eq!(verify["matches"], "OK");
    }

    /// W5 task 2: a `verify` arg missing `matches` must be rejected at
    /// create time (before persisting), not silently accepted and failed
    /// open at iteration time.
    #[tokio::test]
    async fn loop_create_with_verify_arg_missing_matches_rejected() {
        let (storage, mgr) = setup();
        let err = execute_loop_tool(
            "loop_create",
            &json!({
                "trigger_expr": "time:5m",
                "prompt_text": "x",
                "verify": { "tool": "canvas_eval" }
            }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("matches"), "got: {err}");
    }

    /// Mirrors `manager::tests::create_loop_rejects_event_gate_on_time_trigger`
    /// but through the JSON tool-arg path, confirming the error surfaces
    /// all the way up through `execute_loop_tool`.
    #[tokio::test]
    async fn loop_create_with_event_gate_on_time_trigger_arg_rejected() {
        let (storage, mgr) = setup();
        let err = execute_loop_tool(
            "loop_create",
            &json!({
                "trigger_expr": "time:5m",
                "prompt_text": "x",
                "gate": { "kind": "event" }
            }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("event"),
            "expected event/trigger mismatch error, got: {err}"
        );
    }

    #[tokio::test]
    async fn loop_create_blocked_in_iteration() {
        let (storage, mgr) = setup();
        let err = execute_loop_tool(
            "loop_create",
            &json!({ "trigger_expr": "time:5m", "prompt_text": "x" }),
            &ctx(true, Some("aaa")),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("forbidden") || err.contains("inside an iteration"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn scratchpad_set_rejects_oversize() {
        let (storage, mgr) = setup();
        let id = execute_loop_tool(
            "loop_create",
            &json!({ "trigger_expr": "time:5m", "prompt_text": "x" }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap()
        .get("loop_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

        let big = "x".repeat(4097);
        let err = execute_loop_tool(
            "loop_scratchpad_set",
            &json!({ "content": big }),
            &ctx(true, Some(&id)),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("4096"), "got: {err}");
    }

    #[tokio::test]
    async fn scratchpad_set_persists_under_limit() {
        let (storage, mgr) = setup();
        let id = execute_loop_tool(
            "loop_create",
            &json!({ "trigger_expr": "time:5m", "prompt_text": "x" }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap()
        .get("loop_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

        execute_loop_tool(
            "loop_scratchpad_set",
            &json!({ "content": "k=v" }),
            &ctx(true, Some(&id)),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();

        let got = execute_loop_tool(
            "loop_scratchpad_get",
            &json!({ "loop_id": id }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        assert_eq!(got.get("content").unwrap().as_str().unwrap(), "k=v");
        assert_eq!(got.get("bytes").unwrap().as_i64().unwrap(), 3);
    }

    #[tokio::test]
    async fn cancel_other_loop_from_iteration_rejected() {
        let (storage, mgr) = setup();
        let id = execute_loop_tool(
            "loop_create",
            &json!({ "trigger_expr": "time:5m", "prompt_text": "x" }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap()
        .get("loop_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

        let err = execute_loop_tool(
            "loop_cancel",
            &json!({ "loop_id": &id }),
            &ctx(true, Some("self")),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("only cancel its own"), "got: {err}");
    }

    #[tokio::test]
    async fn scratchpad_set_outside_iteration_rejected() {
        let (storage, mgr) = setup();
        let id = execute_loop_tool(
            "loop_create",
            &json!({ "trigger_expr": "time:5m", "prompt_text": "x" }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap()
        .get("loop_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

        let err = execute_loop_tool(
            "loop_scratchpad_set",
            &json!({ "content": "k=v" }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("iteration"), "got: {err}");
        let _ = id;
    }

    #[tokio::test]
    async fn unknown_tool_name_errors() {
        let (storage, mgr) = setup();
        let err = execute_loop_tool(
            "loop_invented",
            &json!({}),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("unknown loop tool"));
    }
}
