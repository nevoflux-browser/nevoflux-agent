//! LLM-callable tool handlers for /loop (spec §10).
//!
//! Five tools share one dispatcher so registration in Phase 9
//! (builtin-wasm + mcp_tool_executor) is a single name-list per call site.

use crate::loops::manager::{CreateLoopArgs, LoopManager};
use crate::loops::tool_classes::is_forbidden_in_iteration;
use crate::loops::types::{GateKind, GateSpec, LoopId};
use crate::wasm::services::HostServices;
use nevoflux_storage::connection::Database;
use nevoflux_storage::models::current_timestamp;
use nevoflux_storage::repositories::{LoopProposalRepository, LoopRepository};
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

/// `services` is only required by `loop_evolve` (it needs `HostServices` to
/// run the meta-pass's LLM turn); every other tool ignores it. Callers that
/// never dispatch `loop_evolve` (most existing tests) may pass `None`.
pub async fn execute_loop_tool(
    name: &str,
    args: &Value,
    ctx: &ToolCallContext,
    mgr: &LoopManager,
    db: &Database,
    services: Option<&HostServices>,
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
        "loop_evolve" => loop_evolve(args, db, services).await,
        "loop_proposal_respond" => loop_proposal_respond(args, ctx, mgr, db).await,
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

/// `loop_evolve {loop_id}` — run the self-improvement meta-pass (W4) for a
/// loop and return a summary of the proposal it produced. Not gated to
/// main-session-only or restricted from iterations (mirrors `loop_list`) —
/// the meta-pass itself is what's locked down (see
/// `evolve::evolve_forbidden_prefixes`), not the ability to kick it off.
async fn loop_evolve(
    args: &Value,
    db: &Database,
    services: Option<&HostServices>,
) -> Result<Value, String> {
    let loop_id = args
        .get("loop_id")
        .and_then(|v| v.as_str())
        .ok_or("loop_id required")?;
    let services = services
        .ok_or("loop_evolve requires HostServices (LLM access) — not available in this context")?;
    let proposal = crate::loops::evolve::evolve_loop(db, services, loop_id).await?;
    Ok(json!({
        "id": proposal.id,
        "rationale": proposal.rationale,
        "has_prompt_text": proposal.proposed_prompt_text.is_some(),
        "has_gate_spec": proposal.proposed_gate_spec.is_some(),
    }))
}

/// `loop_proposal_respond {proposal_id, accept}` — accept or reject a
/// pending self-improvement proposal (W4). On accept, applies the proposed
/// `prompt_text`/`gate_spec` onto the loop via
/// `LoopRepository::apply_proposal_fields`. If the proposal is no longer
/// pending (already responded to, or an unknown id), returns a clear
/// `{applied: false, status: "no_pending_proposal"}` result instead of
/// erroring — mirrors `respond_proposal`'s `None`-on-noop contract.
async fn loop_proposal_respond(
    args: &Value,
    ctx: &ToolCallContext,
    mgr: &LoopManager,
    db: &Database,
) -> Result<Value, String> {
    let proposal_id = args
        .get("proposal_id")
        .and_then(|v| v.as_str())
        .ok_or("proposal_id required")?;
    let accept = args
        .get("accept")
        .and_then(|v| v.as_bool())
        .ok_or("accept (boolean) required")?;
    let now = current_timestamp();

    let proposal_repo = LoopProposalRepository::new(db);
    let Some(proposal) = proposal_repo
        .respond_proposal(proposal_id, accept, now)
        .map_err(|e| e.to_string())?
    else {
        return Ok(json!({ "applied": false, "status": "no_pending_proposal" }));
    };

    let applied = if accept {
        LoopRepository::new(db)
            .apply_proposal_fields(
                &proposal.loop_id,
                proposal.proposed_prompt_text.as_deref(),
                proposal.proposed_gate_spec.as_deref(),
                now,
            )
            .map_err(|e| e.to_string())?;
        true
    } else {
        false
    };

    mgr.events()
        .proposal_resolved(
            &ctx.session_id,
            &LoopId(proposal.loop_id.clone()),
            &proposal.id,
            accept,
        )
        .await;

    Ok(json!({ "applied": applied, "status": proposal.status }))
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

    /// Thin wrapper around `execute_loop_tool` that passes `services: None`
    /// — every test below exercises tools that don't need `HostServices`
    /// (`loop_evolve` is the only one that does, and it's covered directly
    /// via `evolve::evolve_loop`'s own tests, not through this dispatcher,
    /// since it needs a real LLM turn).
    async fn exec(
        name: &str,
        args: &Value,
        ctx: &ToolCallContext,
        mgr: &LoopManager,
        db: &Database,
    ) -> Result<Value, String> {
        execute_loop_tool(name, args, ctx, mgr, db, None).await
    }

    #[tokio::test]
    async fn loop_create_then_list_includes_it() {
        let (storage, mgr) = setup();
        let res = exec(
            "loop_create",
            &json!({ "trigger_expr": "time:5m", "prompt_text": "x" }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        let id = res.get("loop_id").unwrap().as_str().unwrap().to_string();

        let list = exec(
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
        let res = exec(
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
        let res = exec(
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
        let err = exec(
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
        let err = exec(
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
        let err = exec(
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
        let id = exec(
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
        let err = exec(
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
        let id = exec(
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

        exec(
            "loop_scratchpad_set",
            &json!({ "content": "k=v" }),
            &ctx(true, Some(&id)),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();

        let got = exec(
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
        let id = exec(
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

        let err = exec(
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
        let id = exec(
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

        let err = exec(
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
        let err = exec(
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

    // ---- loop_evolve / loop_proposal_respond (W4 task 3) -----------------

    /// Bypasses `evolve::evolve_loop` (which needs a real LLM turn) and
    /// inserts a pending `LoopProposal` row directly, mirroring
    /// `storage::repositories::loop_record::tests::sample_proposal` — these
    /// tests are exercising `loop_proposal_respond`'s dispatch, not the
    /// meta-pass itself.
    fn insert_pending_proposal(
        db: &Database,
        loop_id: &str,
        proposal_id: &str,
        prompt_text: Option<&str>,
    ) {
        let proposal = nevoflux_storage::repositories::LoopProposal {
            id: proposal_id.into(),
            loop_id: loop_id.into(),
            created_at: current_timestamp(),
            rationale: "test rationale".into(),
            proposed_prompt_text: prompt_text.map(String::from),
            proposed_gate_spec: None,
            status: "pending".into(),
        };
        LoopProposalRepository::new(db)
            .insert_proposal(&proposal)
            .unwrap();
    }

    async fn create_loop_with_prompt(storage: &Storage, mgr: &LoopManager, prompt: &str) -> String {
        exec(
            "loop_create",
            &json!({ "trigger_expr": "time:5m", "prompt_text": prompt }),
            &ctx(false, None),
            mgr,
            storage.database(),
        )
        .await
        .unwrap()
        .get("loop_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
    }

    #[tokio::test]
    async fn loop_proposal_respond_accept_applies_prompt_text() {
        let (storage, mgr) = setup();
        let id = create_loop_with_prompt(&storage, &mgr, "old").await;
        insert_pending_proposal(storage.database(), &id, "prop-1", Some("new"));

        let res = exec(
            "loop_proposal_respond",
            &json!({ "proposal_id": "prop-1", "accept": true }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        assert!(res.get("applied").unwrap().as_bool().unwrap());
        assert_eq!(res.get("status").unwrap().as_str().unwrap(), "accepted");

        let rec = LoopRepository::new(storage.database())
            .get(&id)
            .unwrap()
            .unwrap();
        assert_eq!(rec.prompt_text.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn loop_proposal_respond_reject_leaves_loop_unchanged() {
        let (storage, mgr) = setup();
        let id = create_loop_with_prompt(&storage, &mgr, "old").await;
        insert_pending_proposal(storage.database(), &id, "prop-2", Some("new"));

        let res = exec(
            "loop_proposal_respond",
            &json!({ "proposal_id": "prop-2", "accept": false }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        assert!(!res.get("applied").unwrap().as_bool().unwrap());
        assert_eq!(res.get("status").unwrap().as_str().unwrap(), "rejected");

        let rec = LoopRepository::new(storage.database())
            .get(&id)
            .unwrap()
            .unwrap();
        assert_eq!(
            rec.prompt_text.as_deref(),
            Some("old"),
            "a rejected proposal must not touch the loop's prompt_text"
        );
    }

    #[tokio::test]
    async fn loop_proposal_respond_already_responded_returns_no_pending_without_error() {
        let (storage, mgr) = setup();
        let id = create_loop_with_prompt(&storage, &mgr, "old").await;
        insert_pending_proposal(storage.database(), &id, "prop-3", Some("new"));

        exec(
            "loop_proposal_respond",
            &json!({ "proposal_id": "prop-3", "accept": false }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();

        // Second response to the now-resolved proposal must not error.
        let res = exec(
            "loop_proposal_respond",
            &json!({ "proposal_id": "prop-3", "accept": true }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        assert!(!res.get("applied").unwrap().as_bool().unwrap());
        assert_eq!(
            res.get("status").unwrap().as_str().unwrap(),
            "no_pending_proposal"
        );

        // And the earlier reject must still stand — a stale "accept" retry
        // must never flip it to accepted after the fact.
        let rec = LoopRepository::new(storage.database())
            .get(&id)
            .unwrap()
            .unwrap();
        assert_eq!(rec.prompt_text.as_deref(), Some("old"));
    }

    #[tokio::test]
    async fn loop_proposal_respond_unknown_id_returns_no_pending_without_error() {
        let (storage, mgr) = setup();
        let res = exec(
            "loop_proposal_respond",
            &json!({ "proposal_id": "does-not-exist", "accept": true }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap();
        assert!(!res.get("applied").unwrap().as_bool().unwrap());
        assert_eq!(
            res.get("status").unwrap().as_str().unwrap(),
            "no_pending_proposal"
        );
    }

    #[tokio::test]
    async fn loop_proposal_respond_missing_args_errors() {
        let (storage, mgr) = setup();
        let err = exec(
            "loop_proposal_respond",
            &json!({ "accept": true }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("proposal_id"), "got: {err}");

        let err = exec(
            "loop_proposal_respond",
            &json!({ "proposal_id": "prop-x" }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("accept"), "got: {err}");
    }

    #[tokio::test]
    async fn loop_evolve_missing_loop_id_errors() {
        let (storage, mgr) = setup();
        let err = exec(
            "loop_evolve",
            &json!({}),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("loop_id"), "got: {err}");
    }

    /// `exec` always passes `services: None` — proves `loop_evolve` fails
    /// with a clear, actionable error rather than panicking when no
    /// `HostServices` is available (the shape every existing unit test in
    /// this module is in, since none of them wire an LLM).
    #[tokio::test]
    async fn loop_evolve_without_services_errors_clearly() {
        let (storage, mgr) = setup();
        let id = create_loop_with_prompt(&storage, &mgr, "old").await;
        let err = exec(
            "loop_evolve",
            &json!({ "loop_id": id }),
            &ctx(false, None),
            &mgr,
            storage.database(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("HostServices"), "got: {err}");
    }
}
