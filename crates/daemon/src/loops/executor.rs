//! Per-iteration execution (spec §7).
//!
//! ## Phase 9c status (2026-05-08)
//!
//! Phase 9c lights up **real LLM-driven iteration execution** by reusing
//! the production agent host the chat-send path uses. Iteration flow:
//!
//! 1. Read the loop record, increment `iteration_count`, insert a
//!    `loop_iterations` row with `status=running`, emit
//!    `system:loop:iteration_start`.
//! 2. Resolve the loop's `mode`; build per-mode tool allowlist via
//!    iteration error and bump `consecutive_failures`.
//! 3. Build the §7.2 LOOP-CONTEXT block + per-class tool-name allowlist.
//! 4. **Production path** (when [`HostServices`] are wired via
//!    [`IterationExecutor::with_services`]): spawn a
//!    [`crate::agent_host::DaemonHostFunctions`] via the daemon's
//!    [`HostServices::agent_config`] + [`HostServices::runtime_handle`]
//!    snapshots, then call
//!    `nevoflux_builtin_wasm::Agent::new(host).run(&AgentInput { tools_config: Allow(...), ..})`.
//!    The host's pre-LLM tool filter (`Agent::filter_tools`) honors the
//!    allowlist, so no extra runtime guard is needed.
//! 5. **Stub path** (no services — unit tests only): record a no-op
//!    success without invoking an LLM. This preserves Phase-6 test
//!    semantics while production traffic gets real execution.
//! 6. On success → reset `consecutive_failures` to 0; on error → bump.
//!    The dispatcher in `manager.rs` auto-cancels at >= 3 strikes
//!    (spec §8.4).
//! 7. Emit `system:loop:iteration_end` with the final status + a small
//!    `tool_calls_summary` array.
//!
//! ## What is intentionally **NOT** wired here
//!
//! - **Sidebar streaming**. Iterations don't push their delta-chunks to
//!   the chat sidebar; the §7-mandated visibility surface is the sticky
//!   loop card + `system:loop:iteration_end` event, not the streaming
//!   `stream_chunk` channel. So we skip `with_sidebar_stream`.
//! - **Session memory extraction**. Loop iterations would otherwise
//!   pollute `session_memories` with one extraction per tick; we skip
//!   `with_session_extractor` to keep memory pristine.
//! - **Trace collection**. Iterations write their own
//!   `tool_calls_summary` to `loop_iterations.summary_json`; the
//!   per-chat trace collector is intentionally bypassed.
//! - **Canvas video service**. Loops can't render video (the
//!   `canvas_*` tools are in the `Write` class, which isn't in the
//!   default tool-class set); we don't bother wiring
//!   `with_canvas_video_service`. If a loop opts into the `Write`
//!   class, the call will fail with a clear "tool not configured"
//!   error — acceptable for MVP.
//!
//! Phase 12.2 (`time:dynamic` reschedule) reads
//! [`ExecResult::final_text`] in `manager.rs` and parses the
//! `loop-meta` block out of the assistant text.

use crate::loops::events::LoopEvents;
use crate::loops::manager::db_str_to_agent_mode;
use crate::loops::tool_classes::iteration_forbidden_tools;
use crate::loops::types::LoopId;
use crate::wasm::services::HostServices;
use nevoflux_storage::models::{current_timestamp, IterationStatus, LoopRecord};
use nevoflux_storage::repositories::{LoopRepository, MessageRepository};
use nevoflux_storage::Database;
use std::sync::Arc;

/// W5 §verify: how many of the session's most-recent `tool_result` messages
/// to read back for a loop iteration's programmatic check. Mirrors
/// `goals/manager.rs::TOOL_RESULTS_WINDOW` in spirit but sized a bit larger
/// since loop iterations can invoke more tools per turn than a chat goal
/// turn does.
const VERIFY_TOOL_RESULTS_WINDOW: u32 = 20;

#[derive(Debug)]
pub enum ExecResult {
    /// Iteration completed successfully.
    Ok,
    /// Iteration completed successfully and carried final assistant text.
    /// Phase 12.2's `time:dynamic` reschedule reads `loop-meta` from this
    /// text. `None` means the iteration ran but produced no text (e.g.
    /// when `HostServices` is not wired — unit tests only).
    OkWithText(Option<String>),
    /// Iteration failed; the string is a short human-readable reason.
    Error(String),
}

impl ExecResult {
    /// True for any successful completion (with or without text).
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok | Self::OkWithText(_))
    }

    /// Final assistant text, when available.
    pub fn final_text(&self) -> Option<&str> {
        match self {
            Self::OkWithText(Some(t)) => Some(t.as_str()),
            _ => None,
        }
    }
}

pub struct IterationExecutor {
    db: Database,
    events: Arc<LoopEvents>,
    /// `HostServices` snapshot for spawning a production
    /// [`crate::agent_host::DaemonHostFunctions`]. `None` in unit tests
    /// → stub success path. Set during server boot via
    /// [`Self::with_services`] (see `manager.rs::start_with_bus`).
    services: Option<HostServices>,
}

impl IterationExecutor {
    pub fn new(db: Database) -> Self {
        Self::new_with_events(db, Arc::new(LoopEvents::new(None)))
    }

    pub fn new_with_events(db: Database, events: Arc<LoopEvents>) -> Self {
        Self {
            db,
            events,
            services: None,
        }
    }

    /// Wire the `HostServices` snapshot the executor uses to invoke a
    /// real production agent. When unset, [`Self::execute`] short-circuits
    /// to the Phase-6 stub path that records the iteration as `ok`
    /// without invoking an LLM. Phase 9c.
    pub fn with_services(mut self, services: HostServices) -> Self {
        self.services = Some(services);
        self
    }

    /// Cheap clone of the underlying Database handle.
    /// Phase 7's LoopManager dispatcher needs this to construct
    /// short-lived `LoopRepository` instances at fire time.
    pub fn database(&self) -> Database {
        self.db.clone()
    }

    /// Run a single iteration. See module-level docs for the
    /// production-vs-stub split and intentionally-skipped wiring.
    ///
    /// `gate_output` is the deterministic gate's observed value for this
    /// fire (W3 §gate), when the loop has a gate configured and it decided
    /// to run — `None` for gate-less loops or fires with no gate output.
    /// Threaded into the `<LOOP-CONTEXT>` block via `build_user_message`.
    pub async fn execute(
        &self,
        loop_id: LoopId,
        fire_reason: String,
        gate_output: Option<String>,
    ) -> ExecResult {
        let repo = LoopRepository::new(&self.db);
        let now = current_timestamp();

        let rec = match repo.get(loop_id.as_ref()) {
            Ok(Some(r)) => r,
            Ok(None) => return ExecResult::Error(format!("loop {} vanished", loop_id)),
            Err(e) => return ExecResult::Error(e.to_string()),
        };
        let session_id = rec.session_id.clone();

        let seq = match repo.increment_iteration_count(loop_id.as_ref(), now) {
            Ok(s) => s,
            Err(e) => return ExecResult::Error(e.to_string()),
        };
        let iter_id =
            match repo.insert_iteration(loop_id.as_ref(), seq, now, IterationStatus::Running) {
                Ok(i) => i,
                Err(e) => return ExecResult::Error(e.to_string()),
            };

        self.events
            .iteration_start(&session_id, &loop_id, seq, now, &fire_reason)
            .await;

        // Resolve the loop's AgentMode from its DB record. The mode picks
        // the iteration's tool catalog via `Agent::get_tools_for_mode`. Tools
        // forbidden in iteration context (`ask_user`, `loop_create`) are
        // stripped from the resulting allowlist.
        let iter_mode = db_str_to_agent_mode(&rec.mode);
        let user_message = build_user_message(
            &rec,
            seq,
            &fire_reason,
            &self.db,
            self.services.as_ref(),
            gate_output.as_deref(),
        )
        .await;

        // Stub path: no services → record ok without invoking LLM.
        // Preserves Phase-6 test semantics. Production callers always
        // wire services via `LoopManager::start_with_bus`.
        let services = match self.services.as_ref() {
            Some(s) => s.clone(),
            None => {
                tracing::debug!(
                    loop_id = %loop_id,
                    seq = seq,
                    "IterationExecutor: no HostServices wired — skipping LLM (stub path)"
                );
                return self
                    .finalize_iteration_ok(
                        iter_id,
                        None,
                        serde_json::json!([]),
                        &session_id,
                        &loop_id,
                        seq,
                        &rec,
                    )
                    .await;
            }
        };

        // Production path: run one unattended agent turn via the shared
        // `agent_exec` kernel. The kernel owns host construction, the sidebar
        // proxy borrow, the `CURRENT_LOOP_MANAGER` back-fill, interrupt reset,
        // allowlist filtering, and agent-panic mapping — extracted verbatim so
        // schedules/goals can reuse the exact same wiring. Iterations skip
        // sidebar streaming, session extraction, and the per-chat trace
        // collector (we persist our own `tool_calls_summary`).
        let req = crate::agent_exec::AgentExecRequest {
            session_id: session_id.clone(),
            mode: iter_mode,
            user_message,
            forbidden_tools: iteration_forbidden_tools(),
            forbidden_prefixes: Vec::new(),
            unattended: true,
            iteration_loop_id: Some(loop_id.0.clone()),
            borrow_proxy: true,
            bound_browser: None,
            history: Vec::new(),
            token_budget: None,
        };
        match crate::agent_exec::run_agent_once(&services, req).await {
            Ok(outcome) => {
                self.finalize_iteration_ok(
                    iter_id,
                    Some(outcome.text),
                    outcome.trace,
                    &session_id,
                    &loop_id,
                    seq,
                    &rec,
                )
                .await
            }
            Err(e) => {
                self.finalize_iteration_error(iter_id, e, &session_id, &loop_id, seq, &rec)
                    .await
            }
        }
    }

    /// Finish row + emit event for a successful iteration. Resets
    /// `consecutive_failures` to 0.
    async fn finalize_iteration_ok(
        &self,
        iter_id: i64,
        final_text: Option<String>,
        trace_summary: serde_json::Value,
        session_id: &str,
        loop_id: &LoopId,
        seq: i64,
        rec: &LoopRecord,
    ) -> ExecResult {
        let end_now = current_timestamp();
        let repo = LoopRepository::new(&self.db);
        let _ = repo.set_consecutive_failures(loop_id.as_ref(), 0, end_now);
        // TODO(W4): wire loop token spend. `run_agent_once` doesn't thread a
        // token budget for iterations today (req.token_budget is always
        // `None` above), so there's no spend to read yet — see
        // `schedules/runner.rs::budget_spent` for the pattern to follow once
        // loops gain a per-iteration/per-loop token limit.
        let tokens_used: Option<i64> = None;

        // W5 §verify: run the loop's optional programmatic check (reuses the
        // `/goal` check engine) against this iteration's tool results.
        // `parse_check` expects a `{"check": <GoalCheck>}` envelope (that's
        // the shape `goal_set` args carry); `verify_check` on the loop row
        // is the bare `GoalCheck` JSON, so wrap it before parsing. Any parse
        // failure (malformed JSON, missing `matches`, bad regex) fails open:
        // `(None, None)` — a bad verify spec must never block the loop.
        //
        // Content source: NOT `trace_summary` — the production trace built
        // by `agent_exec.rs` only carries `{name, ok}` per tool call, no
        // result content, so a content-matching check would silently always
        // fail against it. Reuse `/goal`'s exact data source instead: real
        // tool-result content is persisted to the `messages` table
        // (`content_type='tool_result'`) by `DaemonHostFunctions::
        // record_tool_result` on the same `Agent::run` pipeline loops use.
        // This is also why we must NOT feed `final_text` in here — that's
        // the model's own paraphrase, not the independent tool read-back the
        // check engine is meant to verify against.
        let (verify_passed, verify_reason) = match rec.verify_check.as_deref() {
            Some(cj) => {
                let parsed = serde_json::from_str::<serde_json::Value>(cj)
                    .ok()
                    .and_then(|v| {
                        crate::goals::check::parse_check(&serde_json::json!({ "check": v }))
                            .ok()
                            .flatten()
                    });
                match parsed {
                    Some(check) => {
                        let tool_results = MessageRepository::new(&self.db)
                            .list_recent_tool_results(session_id, VERIFY_TOOL_RESULTS_WINDOW)
                            .unwrap_or_default();
                        let passed = crate::goals::check::eval_check(&check, &tool_results);
                        let reason = format!(
                            "check '{}' {}",
                            check.matches,
                            if passed { "passed" } else { "failed" }
                        );
                        (Some(passed), Some(reason))
                    }
                    None => (None, None),
                }
            }
            None => (None, None),
        };

        let _ = repo.finish_iteration(
            iter_id,
            end_now,
            IterationStatus::Ok,
            None,
            Some(&serde_json::to_string(&trace_summary).unwrap_or_default()),
            final_text.as_deref(),
            tokens_used,
            verify_passed,
            verify_reason.as_deref(),
        );
        self.events
            .iteration_end(
                session_id,
                loop_id,
                seq,
                end_now,
                "ok",
                trace_summary,
                final_text.as_deref(),
                verify_passed,
                verify_reason.as_deref(),
            )
            .await;
        ExecResult::OkWithText(final_text)
    }

    /// Finish row + emit event for a failed iteration. Bumps
    /// `consecutive_failures`.
    async fn finalize_iteration_error(
        &self,
        iter_id: i64,
        err: String,
        session_id: &str,
        loop_id: &LoopId,
        seq: i64,
        rec: &LoopRecord,
    ) -> ExecResult {
        let end_now = current_timestamp();
        let repo = LoopRepository::new(&self.db);
        let new_failures = rec.consecutive_failures + 1;
        let _ = repo.set_consecutive_failures(loop_id.as_ref(), new_failures, end_now);
        let _ = repo.finish_iteration(
            iter_id,
            end_now,
            IterationStatus::Error,
            Some(&err),
            None,
            None,
            None,
            None,
            None,
        );
        self.events
            .iteration_end(
                session_id,
                loop_id,
                seq,
                end_now,
                "error",
                serde_json::json!([]),
                None,
                None,
                None,
            )
            .await;
        ExecResult::Error(err)
    }
}

/// Build the §7.2 LOOP-CONTEXT-prefixed user message for an iteration.
/// Public-in-crate for unit testing and for Phase 9c's AgentInput construction.
///
/// When `rec.prompt_text` is `None` and `rec.wrapped_skill` is `Some(json)`,
/// resolves the wrapped-skill body from the [`HostServices`] skill registry
/// (Phase 21). The JSON shape is `{ "name": "<skill>", "args": <string-or-object> }`,
/// matching the slash-command sender on the extension side.
pub(crate) async fn build_user_message(
    rec: &LoopRecord,
    sequence: i64,
    fire_reason: &str,
    db: &Database,
    services: Option<&HostServices>,
    gate_output: Option<&str>,
) -> String {
    let scratchpad = if rec.scratchpad.is_empty() {
        "(empty)"
    } else {
        rec.scratchpad.as_str()
    };

    let body: String = if let Some(prompt) = &rec.prompt_text {
        prompt.clone()
    } else if let Some(skill_json) = &rec.wrapped_skill {
        materialize_wrapped_skill(skill_json, services).await
    } else {
        "(no prompt or wrapped_skill)".into()
    };

    // Feed the last N finished iterations' result summaries back into this
    // iteration's context so it can see prior outcomes and stop re-doing
    // work. Empty when the loop has no finished iterations yet (fresh loop)
    // or the query fails — the LOOP-CONTEXT block degrades gracefully.
    const RECENT_RUNS_N: usize = 5;
    const RECENT_LINE_MAX: usize = 200; // one-line summary cap per run, in chars (char-safe truncation)
    let recent_block = {
        let repo = LoopRepository::new(db);
        match repo.recent_iterations(rec.id.as_ref(), RECENT_RUNS_N) {
            Ok(rows) if !rows.is_empty() => {
                let mut s = String::from("recent_runs (newest first):\n");
                for r in &rows {
                    let summary = r.final_text.as_deref().unwrap_or("").replace('\n', " ");
                    let summary = if summary.chars().count() > RECENT_LINE_MAX {
                        let truncated: String = summary.chars().take(RECENT_LINE_MAX).collect();
                        format!("{}…", truncated)
                    } else {
                        summary
                    };
                    s.push_str(&format!(
                        "  #{} [{}] {}\n",
                        r.sequence_number, r.status, summary
                    ));
                }
                s
            }
            _ => String::new(),
        }
    };

    // W3 §gate: only present when the loop has a deterministic gate that
    // ran and produced an observed value for this fire (http/bash diff, or
    // the matched event payload). Absent for gate-less loops and for gate
    // decisions with no output (`GateKind::None`, event gates with no
    // extracted value).
    let gate_block = match gate_output {
        Some(v) if !v.is_empty() => format!("gate_output:\n{}\n", v),
        _ => String::new(),
    };

    format!(
        "<LOOP-CONTEXT>\n\
         loop_id={}\n\
         iteration={}\n\
         trigger={}\n\
         fire_reason={}\n\
         scratchpad_bytes={}\n\
         scratchpad:\n{}\n\
         {}{}</LOOP-CONTEXT>\n\
         \n\
         {}",
        rec.id,
        sequence,
        rec.trigger_expr,
        fire_reason,
        rec.scratchpad.len(),
        scratchpad,
        recent_block,
        gate_block,
        body,
    )
}

/// Resolve the wrapped-skill JSON blob (`{name, args}`) into the raw skill
/// body the iteration should run. Args, when present, are appended verbatim
/// after a blank line — the skill itself decides how to interpret them.
///
/// Returns a self-explanatory diagnostic string if anything goes wrong (no
/// skill registry, missing skill, parse failure). Loops should not fail
/// outright on a bad wrapped_skill — they should surface the problem in the
/// iteration log so the operator can fix the loop record.
pub(crate) async fn materialize_wrapped_skill(
    skill_json: &str,
    services: Option<&HostServices>,
) -> String {
    // Parse the {name, args} JSON shape stashed at loop creation.
    let parsed: serde_json::Value = match serde_json::from_str(skill_json) {
        Ok(v) => v,
        Err(e) => return format!("(wrapped_skill parse error: {e})"),
    };
    let name = parsed.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = parsed
        .get("args")
        .and_then(|v| {
            // Sender wraps args either as a string or an object — handle both.
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| Some(v.to_string()))
        })
        .unwrap_or_default();

    if name.is_empty() {
        return "(wrapped_skill missing 'name')".into();
    }

    let services = match services {
        Some(s) => s,
        None => return format!("(wrapped_skill {name}: no skill registry available)"),
    };

    let registry = services.skills.read().await;
    let content = match registry.get(name) {
        Some(s) => s.content.clone(),
        None => return format!("(wrapped_skill not found: {name})"),
    };
    drop(registry);

    if args.is_empty() || args == "\"\"" || args == "{}" {
        content
    } else {
        format!("{}\n\n{}", content, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_storage::models::{CreateSessionParams, LoopState};
    use nevoflux_storage::Storage;

    fn sample_loop(id: &str) -> LoopRecord {
        LoopRecord {
            id: id.into(),
            session_id: "s1".into(),
            trigger_expr: "time:5m".into(),
            prompt_text: Some("check PR".into()),
            wrapped_skill: None,
            mode: "chat".into(),
            scratchpad: "k=v".into(),
            state: LoopState::Running,
            consecutive_failures: 0,
            skipped_triggers: 0,
            iteration_count: 0,
            created_at: 0,
            updated_at: 0,
            gate_kind: "none".into(),
            gate_spec: None,
            gate_last_value: None,
            verify_check: None,
        }
    }

    #[tokio::test]
    async fn loop_context_block_includes_required_fields() {
        let storage = Storage::open_in_memory().unwrap();
        let rec = sample_loop("abcd1234");
        let s = build_user_message(&rec, 1, "time", storage.database(), None, None).await;
        assert!(s.contains("loop_id=abcd1234"));
        assert!(s.contains("iteration=1"));
        assert!(s.contains("trigger=time:5m"));
        assert!(s.contains("fire_reason=time"));
        assert!(s.contains("scratchpad_bytes=3"));
        assert!(s.contains("k=v"));
        assert!(s.contains("check PR"));
    }

    #[tokio::test]
    async fn loop_context_block_marks_empty_scratchpad() {
        let storage = Storage::open_in_memory().unwrap();
        let mut rec = sample_loop("a");
        rec.scratchpad.clear();
        let s = build_user_message(&rec, 1, "time", storage.database(), None, None).await;
        assert!(s.contains("scratchpad_bytes=0"));
        assert!(s.contains("(empty)"));
    }

    #[tokio::test]
    async fn loop_context_block_falls_back_for_wrapped_skill() {
        let storage = Storage::open_in_memory().unwrap();
        let mut rec = sample_loop("a");
        rec.prompt_text = None;
        rec.wrapped_skill = Some(r#"{"name":"video","args":{}}"#.into());
        let s = build_user_message(&rec, 1, "time", storage.database(), None, None).await;
        // services=None → "(wrapped_skill <name>: no skill registry available)"
        assert!(s.contains("video"));
        assert!(s.contains("no skill registry available"));
    }

    /// W3 §gate: when the dispatcher's gate evaluator produced an observed
    /// value for this fire (http/bash diff, or a matched event payload), it
    /// must land in the `<LOOP-CONTEXT>` block so the iteration's LLM can
    /// see what changed.
    #[tokio::test]
    async fn loop_context_includes_gate_output_when_present() {
        let storage = Storage::open_in_memory().unwrap();
        let rec = sample_loop("abcd1234");
        let s = build_user_message(
            &rec,
            1,
            "event:test:gate",
            storage.database(),
            None,
            Some("observed-42"),
        )
        .await;
        assert!(
            s.contains("gate_output:\nobserved-42"),
            "expected gate_output section in: {s}"
        );
        // Must still land inside the <LOOP-CONTEXT> block, before the closing tag.
        let ctx_end = s.find("</LOOP-CONTEXT>").expect("context block present");
        let gate_pos = s.find("gate_output:").expect("gate_output present");
        assert!(
            gate_pos < ctx_end,
            "gate_output must be inside LOOP-CONTEXT"
        );
    }

    /// Gate-less loops (`GateKind::None`, the default) and gate decisions
    /// with no output must not add a spurious `gate_output:` section.
    #[tokio::test]
    async fn loop_context_omits_gate_output_when_absent() {
        let storage = Storage::open_in_memory().unwrap();
        let rec = sample_loop("abcd1234");
        let s = build_user_message(&rec, 1, "time", storage.database(), None, None).await;
        assert!(!s.contains("gate_output:"));
    }

    #[tokio::test]
    async fn loop_context_includes_recent_runs() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let rec = sample_loop("L");
        storage.loops().create(&rec).unwrap();
        let id = storage
            .loops()
            .insert_iteration("L", 1, 100, IterationStatus::Running)
            .unwrap();
        storage
            .loops()
            .finish_iteration(
                id,
                110,
                IterationStatus::Ok,
                None,
                Some("[]"),
                Some("prior result"),
                None,
                None,
                None,
            )
            .unwrap();

        let msg = build_user_message(&rec, 2, "time", storage.database(), None, None).await;
        assert!(msg.contains("recent_runs"));
        assert!(msg.contains("prior result"));
    }

    /// Regression test for a byte-slice truncation panic: `final_text` is
    /// unconstrained LLM output and routinely contains multibyte UTF-8 (CJK,
    /// emoji). Slicing `&summary[..RECENT_LINE_MAX]` on a byte index that
    /// falls mid-codepoint panics. This builds a 207-char summary (199 ASCII
    /// + 8 CJK chars) so the 200-char cap lands inside the multibyte tail,
    /// then asserts `build_user_message` returns normally and truncates on a
    /// char boundary.
    #[tokio::test]
    async fn loop_context_truncates_multibyte_summary_without_panicking() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let rec = sample_loop("M");
        storage.loops().create(&rec).unwrap();
        let id = storage
            .loops()
            .insert_iteration("M", 1, 100, IterationStatus::Running)
            .unwrap();

        let final_text = format!("{}{}", "a".repeat(199), "中文摘要内容测试");
        assert_eq!(final_text.chars().count(), 207, "sanity: 199 ascii + 8 cjk");

        storage
            .loops()
            .finish_iteration(
                id,
                110,
                IterationStatus::Ok,
                None,
                Some("[]"),
                Some(&final_text),
                None,
                None,
                None,
            )
            .unwrap();

        // Must return normally, not panic on a mid-codepoint byte slice.
        let msg = build_user_message(&rec, 2, "time", storage.database(), None, None).await;

        let expected_truncated: String = final_text.chars().take(200).collect();
        assert!(
            msg.contains(&format!("{}…", expected_truncated)),
            "expected char-safe 200-char truncation with ellipsis; got: {msg}"
        );
        assert!(
            !msg.contains(&final_text),
            "full untruncated multibyte summary should not appear"
        );
    }

    #[tokio::test]
    async fn execute_advances_iteration_count_and_writes_row() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        storage.loops().create(&sample_loop("abc")).unwrap();

        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor
            .execute(LoopId("abc".into()), "time".into(), None)
            .await;

        assert!(result.is_ok(), "expected ok-variant, got {:?}", result);
        // Stub path (no services wired) — claims success without text.
        assert!(matches!(result, ExecResult::OkWithText(None)));

        let rec = storage.loops().get("abc").unwrap().unwrap();
        assert_eq!(rec.iteration_count, 1);
    }

    #[tokio::test]
    async fn execute_returns_error_for_missing_loop() {
        let storage = Storage::open_in_memory().unwrap();
        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor
            .execute(LoopId("nope".into()), "time".into(), None)
            .await;
        assert!(matches!(result, ExecResult::Error(_)));
    }

    #[tokio::test]
    async fn execute_resets_failure_counter_on_success() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let mut rec = sample_loop("rst");
        rec.consecutive_failures = 2;
        storage.loops().create(&rec).unwrap();

        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor
            .execute(LoopId("rst".into()), "time".into(), None)
            .await;
        assert!(result.is_ok());

        let after = storage.loops().get("rst").unwrap().unwrap();
        assert_eq!(
            after.consecutive_failures, 0,
            "successful iteration must reset consecutive_failures"
        );
    }

    // W5 §verify: `finalize_iteration_ok` runs the loop's optional
    // `verify_check` against the iteration's real tool-result content, read
    // back from the `messages` table (`content_type='tool_result'`) — the
    // same source `/goal`'s programmatic check reads via
    // `MessageRepository::list_recent_tool_results`. The `trace_summary`
    // passed in below intentionally carries NO `content` field (mirroring
    // the production `agent_exec.rs` trace shape, which is `{name, ok}`
    // only) to prove the verify path no longer depends on it. These tests
    // call the private finalizer directly (rather than going through
    // `execute()`'s stub path, which never produces tool results) so the
    // check-evaluation branch is exercised without needing a real LLM run.

    /// Seed a `tool_result` message directly into the `messages` table,
    /// mirroring what `DaemonHostFunctions::record_tool_result` persists in
    /// production. Same pattern as `goals::manager::tests::seed_tool_result`.
    fn seed_tool_result(db: &Database, session_id: &str, tool: &str, content: &str) {
        use nevoflux_storage::models::{ContentType, CreateMessageParams, MessageRole};
        let mut meta = std::collections::HashMap::new();
        meta.insert("tool_name".to_string(), serde_json::json!(tool));
        nevoflux_storage::repositories::MessageRepository::new(db)
            .create(
                CreateMessageParams::new(session_id, MessageRole::Assistant, content)
                    .with_content_type(ContentType::ToolResult)
                    .with_metadata(meta),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn finalize_iteration_ok_stores_verify_passed_true_on_match() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let mut rec = sample_loop("v1");
        rec.verify_check = Some(r#"{"matches":"OK"}"#.into());
        storage.loops().create(&rec).unwrap();
        let iter_id = storage
            .loops()
            .insert_iteration("v1", 1, 100, IterationStatus::Running)
            .unwrap();
        seed_tool_result(storage.database(), "s1", "bash", "exit 0: OK");

        let executor = IterationExecutor::new(storage.database().clone());
        // No `content` key — proves the check reads the messages table, not
        // the trace, for the actual matching content.
        let trace = serde_json::json!([{"name": "bash", "ok": true}]);
        let result = executor
            .finalize_iteration_ok(
                iter_id,
                Some("done".into()),
                trace,
                "s1",
                &LoopId("v1".into()),
                1,
                &rec,
            )
            .await;
        assert!(result.is_ok(), "expected ok-variant, got {:?}", result);

        let recent = storage.loops().recent_iterations("v1", 1).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].verify_passed, Some(true));
        assert_eq!(
            recent[0].verify_reason.as_deref(),
            Some("check 'OK' passed")
        );
    }

    #[tokio::test]
    async fn finalize_iteration_ok_stores_verify_passed_false_on_no_match() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let mut rec = sample_loop("v2");
        rec.verify_check = Some(r#"{"matches":"OK"}"#.into());
        storage.loops().create(&rec).unwrap();
        let iter_id = storage
            .loops()
            .insert_iteration("v2", 1, 100, IterationStatus::Running)
            .unwrap();
        seed_tool_result(storage.database(), "s1", "bash", "exit 1: failed");

        let executor = IterationExecutor::new(storage.database().clone());
        let trace = serde_json::json!([{"name": "bash", "ok": false}]);
        let result = executor
            .finalize_iteration_ok(
                iter_id,
                Some("done".into()),
                trace,
                "s1",
                &LoopId("v2".into()),
                1,
                &rec,
            )
            .await;
        assert!(result.is_ok(), "expected ok-variant, got {:?}", result);

        let recent = storage.loops().recent_iterations("v2", 1).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].verify_passed, Some(false));
        assert_eq!(
            recent[0].verify_reason.as_deref(),
            Some("check 'OK' failed")
        );
    }

    #[tokio::test]
    async fn finalize_iteration_ok_stores_no_verdict_when_no_verify_check() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let rec = sample_loop("v3"); // verify_check: None (default in sample_loop)
        storage.loops().create(&rec).unwrap();
        let iter_id = storage
            .loops()
            .insert_iteration("v3", 1, 100, IterationStatus::Running)
            .unwrap();
        seed_tool_result(storage.database(), "s1", "bash", "exit 0: OK");

        let executor = IterationExecutor::new(storage.database().clone());
        let trace = serde_json::json!([{"name": "bash", "ok": true}]);
        let result = executor
            .finalize_iteration_ok(
                iter_id,
                Some("done".into()),
                trace,
                "s1",
                &LoopId("v3".into()),
                1,
                &rec,
            )
            .await;
        assert!(result.is_ok(), "expected ok-variant, got {:?}", result);

        let recent = storage.loops().recent_iterations("v3", 1).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].verify_passed, None);
        assert_eq!(recent[0].verify_reason, None);
    }

    #[tokio::test]
    async fn finalize_iteration_ok_fails_open_on_unparseable_verify_check() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let mut rec = sample_loop("v4");
        rec.verify_check = Some("not valid json".into());
        storage.loops().create(&rec).unwrap();
        let iter_id = storage
            .loops()
            .insert_iteration("v4", 1, 100, IterationStatus::Running)
            .unwrap();
        seed_tool_result(storage.database(), "s1", "bash", "exit 0: OK");

        let executor = IterationExecutor::new(storage.database().clone());
        let trace = serde_json::json!([{"name": "bash", "ok": true}]);
        let result = executor
            .finalize_iteration_ok(
                iter_id,
                Some("done".into()),
                trace,
                "s1",
                &LoopId("v4".into()),
                1,
                &rec,
            )
            .await;
        assert!(
            result.is_ok(),
            "malformed verify_check must not fail the iteration, got {:?}",
            result
        );

        let recent = storage.loops().recent_iterations("v4", 1).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].verify_passed, None);
        assert_eq!(recent[0].verify_reason, None);
    }
}
