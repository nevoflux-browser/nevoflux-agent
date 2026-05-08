//! Per-iteration execution (spec §7).
//!
//! ## Phase 9b status (2026-05-08)
//!
//! Phase 9b lands the **tool-class filter plumbing** end-to-end:
//!
//! - `AgentRunner::with_tools_allowlist(Vec<String>)` (new) — gates the
//!   runner's tool dispatch through `ToolRegistry::execute_with_guard`.
//! - `tool_classes::filter_tool_names_by_classes` (new) — translates a
//!   `HashSet<ToolClass>` from the loop record's `allowed_tool_classes`
//!   into the concrete tool-name allowlist the runner needs.
//! - `IterationExecutor::execute` — validates `allowed_tool_classes`,
//!   computes the allowlist, tracks `consecutive_failures` with proper
//!   reset-on-success semantics, and returns the iteration's final
//!   assistant text via the new `ExecResult::OkWithText` variant so
//!   Phase 12.2's `time:dynamic` reschedule has a hook.
//!
//! What is **NOT** wired here yet:
//!
//! Production agent execution in this codebase runs through
//! `nevoflux_builtin_wasm::Agent::new(host).run(input)` (see
//! `server.rs::handle_chat_send`), not the daemon-side `AgentRunner` in
//! `crates/daemon/src/agent/runner.rs` (which is currently
//! test-infrastructure with a simulated `call_agent` body). Wiring real
//! LLM-driven iteration execution here requires either:
//!
//! 1. Snapshotting the full `HostServices` + an `LlmHostFunctions` adapter
//!    at the iteration site (the daemon currently builds these per
//!    chat session in `server.rs`, drawing on `proxy_id`, `client_identity`,
//!    `session_extractor`, and the active sidebar stream — none of which
//!    have natural values for an out-of-band loop tick).
//! 2. OR materializing `agent_wasm` bytes into `HostServices` and using
//!    the daemon-side `AgentRunner` (which still needs a real WASM
//!    `agent_process` export — today the runner only simulates the call).
//!
//! Both threads are out of scope for Phase 9b. The path of least
//! resistance is the daemon-AgentRunner + real `agent_process` route,
//! and the allowlist plumbing landed here is exactly what that route
//! will need.
//!
//! Until that ships, iterations record correctly (status=ok,
//! sequence advances, events emit) but do not actually invoke an LLM
//! and `OkWithText` always carries `None`.
//!
//! Phase 10 emits `system:loop:iteration_start` / `iteration_end`
//! around the work — already wired via [`LoopEvents`].

use crate::loops::events::LoopEvents;
use crate::loops::tool_classes::{
    filter_tool_names_by_classes, parse_class_list,
};
use crate::loops::types::LoopId;
use nevoflux_storage::models::{current_timestamp, IterationStatus, LoopRecord};
use nevoflux_storage::repositories::LoopRepository;
use nevoflux_storage::Database;
use std::sync::Arc;

#[derive(Debug)]
pub enum ExecResult {
    /// Iteration completed successfully.
    Ok,
    /// Iteration completed successfully and carried final assistant text.
    /// Phase 12.2's `time:dynamic` reschedule reads `loop-meta` from this
    /// text. `None` means the iteration ran but produced no text (e.g.
    /// when AgentRunner invocation is still stubbed — see module docs).
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
}

impl IterationExecutor {
    pub fn new(db: Database) -> Self {
        Self::new_with_events(db, Arc::new(LoopEvents::new(None)))
    }

    pub fn new_with_events(db: Database, events: Arc<LoopEvents>) -> Self {
        Self { db, events }
    }

    /// Cheap clone of the underlying Database handle.
    /// Phase 7's LoopManager dispatcher needs this to construct
    /// short-lived `LoopRepository` instances at fire time.
    pub fn database(&self) -> Database {
        self.db.clone()
    }

    /// Run a single iteration.
    ///
    /// Phase 9b semantics (see module-level docs):
    /// 1. Read the loop record.
    /// 2. Increment `iteration_count`, insert a `loop_iterations` row,
    ///    emit `system:loop:iteration_start`.
    /// 3. Validate `allowed_tool_classes`; on parse failure record an
    ///    iteration error and bump `consecutive_failures`.
    /// 4. Build the §7.2 LOOP-CONTEXT block + tool-name allowlist.
    /// 5. **STUB:** invoke AgentRunner with the allowlist. Today this
    ///    short-circuits to a no-op success because the production agent
    ///    path is in `builtin-wasm` and not yet bridged in. The plumbing
    ///    needed to bridge it (allowlist on AgentRunner) is in place.
    /// 6. On success → reset `consecutive_failures` to 0 and finish the
    ///    row with `status=ok`. On error → bump `consecutive_failures`,
    ///    finish with `status=error`. The dispatcher in `manager.rs`
    ///    auto-cancels at >= 3 strikes (spec §8.4).
    /// 7. Emit `system:loop:iteration_end`.
    pub async fn execute(&self, loop_id: LoopId, fire_reason: String) -> ExecResult {
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
        let iter_id = match repo.insert_iteration(
            loop_id.as_ref(),
            seq,
            now,
            IterationStatus::Running,
        ) {
            Ok(i) => i,
            Err(e) => return ExecResult::Error(e.to_string()),
        };

        self.events
            .iteration_start(&session_id, &loop_id, seq, now, &fire_reason)
            .await;

        // Build §7.2 LOOP-CONTEXT user-message. (The actual AgentInput
        // construction is deferred — see module docs.)
        let _user_message = build_user_message(&rec, seq, &fire_reason);

        // Validate allowed_tool_classes and compute the tool-name
        // allowlist that a future AgentRunner invocation will use.
        let outcome: Result<Option<String>, String> =
            match parse_class_list(&rec.allowed_tool_classes) {
                Ok(allowed_classes) => {
                    // Build the would-be allowlist from the *daemon's*
                    // ToolRegistry (the one AgentRunner consults).
                    // We materialize it eagerly so the path is exercised
                    // even though the runner invocation itself is stubbed
                    // — this catches drift in `class_for` / forbidden-set
                    // before Phase 9b's follow-up wires the real call.
                    let registry = crate::agent::tools::ToolRegistry::new();
                    let all_names: Vec<String> = registry
                        .tool_names()
                        .into_iter()
                        .map(|s| s.to_string())
                        .collect();
                    let _allowlist =
                        filter_tool_names_by_classes(&all_names, &allowed_classes);

                    // STUB: invoke the runner here once the production
                    // agent path is bridged. Until then, claim success
                    // without text.
                    Ok(None)
                }
                Err(e) => Err(format!("bad allowed_tool_classes: {e}")),
            };

        let end_now = current_timestamp();
        let (status, error_msg, final_text) = match &outcome {
            Ok(text) => (IterationStatus::Ok, None, text.clone()),
            Err(e) => (IterationStatus::Error, Some(e.clone()), None),
        };

        // Update consecutive_failures: reset on success, bump on error.
        // The dispatcher's 3-strike auto-cancel hook reads this field.
        let new_failures = if matches!(status, IterationStatus::Ok) {
            0
        } else {
            rec.consecutive_failures + 1
        };
        let _ = repo.set_consecutive_failures(loop_id.as_ref(), new_failures, end_now);

        let _ = repo.finish_iteration(iter_id, end_now, status, error_msg.as_deref(), None);

        let status_str = match status {
            IterationStatus::Ok => "ok",
            _ => "error",
        };
        self.events
            .iteration_end(
                &session_id,
                &loop_id,
                seq,
                end_now,
                status_str,
                serde_json::json!([]),
            )
            .await;

        match outcome {
            Ok(text) => ExecResult::OkWithText(text),
            Err(e) => ExecResult::Error(e),
        }
    }
}

/// Build the §7.2 LOOP-CONTEXT-prefixed user message for an iteration.
/// Public-in-crate for unit testing and for Phase 9's AgentInput construction.
pub(crate) fn build_user_message(rec: &LoopRecord, sequence: i64, fire_reason: &str) -> String {
    let scratchpad = if rec.scratchpad.is_empty() {
        "(empty)"
    } else {
        rec.scratchpad.as_str()
    };
    let body = rec
        .prompt_text
        .as_deref()
        .unwrap_or("(wrapped skill — Phase 21)");
    format!(
        "<LOOP-CONTEXT>\n\
         loop_id={}\n\
         iteration={}\n\
         trigger={}\n\
         fire_reason={}\n\
         scratchpad_bytes={}\n\
         scratchpad:\n{}\n\
         </LOOP-CONTEXT>\n\
         \n\
         {}",
        rec.id,
        sequence,
        rec.trigger_expr,
        fire_reason,
        rec.scratchpad.len(),
        scratchpad,
        body,
    )
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
            allowed_tool_classes: vec!["read".into()],
            scratchpad: "k=v".into(),
            state: LoopState::Running,
            consecutive_failures: 0,
            skipped_triggers: 0,
            iteration_count: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn loop_context_block_includes_required_fields() {
        let rec = sample_loop("abcd1234");
        let s = build_user_message(&rec, 1, "time");
        assert!(s.contains("loop_id=abcd1234"));
        assert!(s.contains("iteration=1"));
        assert!(s.contains("trigger=time:5m"));
        assert!(s.contains("fire_reason=time"));
        assert!(s.contains("scratchpad_bytes=3"));
        assert!(s.contains("k=v"));
        assert!(s.contains("check PR"));
    }

    #[test]
    fn loop_context_block_marks_empty_scratchpad() {
        let mut rec = sample_loop("a");
        rec.scratchpad.clear();
        let s = build_user_message(&rec, 1, "time");
        assert!(s.contains("scratchpad_bytes=0"));
        assert!(s.contains("(empty)"));
    }

    #[test]
    fn loop_context_block_falls_back_for_wrapped_skill() {
        let mut rec = sample_loop("a");
        rec.prompt_text = None;
        rec.wrapped_skill = Some(r#"{"name":"video","args":{}}"#.into());
        let s = build_user_message(&rec, 1, "time");
        assert!(s.contains("wrapped skill — Phase 21"));
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
        let result = executor.execute(LoopId("abc".into()), "time".into()).await;

        assert!(result.is_ok(), "expected ok-variant, got {:?}", result);
        // No final text yet — production agent path not bridged.
        assert!(matches!(result, ExecResult::OkWithText(None)));

        let rec = storage.loops().get("abc").unwrap().unwrap();
        assert_eq!(rec.iteration_count, 1);
    }

    #[tokio::test]
    async fn execute_returns_error_for_missing_loop() {
        let storage = Storage::open_in_memory().unwrap();
        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor.execute(LoopId("nope".into()), "time".into()).await;
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
        let result = executor.execute(LoopId("rst".into()), "time".into()).await;
        assert!(result.is_ok());

        let after = storage.loops().get("rst").unwrap().unwrap();
        assert_eq!(
            after.consecutive_failures, 0,
            "successful iteration must reset consecutive_failures"
        );
    }

    #[tokio::test]
    async fn execute_bumps_failure_counter_on_class_parse_error() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        let mut rec = sample_loop("bad");
        rec.allowed_tool_classes = vec!["bogus-class".into()];
        rec.consecutive_failures = 1;
        storage.loops().create(&rec).unwrap();

        let executor = IterationExecutor::new(storage.database().clone());
        let result = executor.execute(LoopId("bad".into()), "time".into()).await;
        match result {
            ExecResult::Error(e) => assert!(e.contains("bogus-class"), "unexpected msg: {e}"),
            other => panic!("expected Error, got {:?}", other),
        }

        let after = storage.loops().get("bad").unwrap().unwrap();
        assert_eq!(
            after.consecutive_failures, 2,
            "error iteration must bump consecutive_failures"
        );

        // The iteration row was inserted with status=running, then
        // updated to error via finish_iteration. Spot-check via direct
        // SQL since LoopRepository doesn't expose a list-iterations API.
        let (status, err_msg): (String, Option<String>) = storage
            .database()
            .with_connection(|conn| {
                let row = conn.query_row(
                    "SELECT status, error_message FROM loop_iterations WHERE loop_id = ?1",
                    rusqlite::params!["bad"],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )?;
                Ok(row)
            })
            .unwrap();
        assert_eq!(status, "error");
        assert!(err_msg.unwrap_or_default().contains("bogus-class"));
    }
}
