//! Per-iteration execution (spec §7).
//!
//! Phase 6 — skeleton only. Builds the §7.2 LOOP-CONTEXT layout, inserts
//! a `loop_iterations` row, and marks it `ok` without invoking AgentRunner.
//! Phase 9 will replace the stub body with a real
//! `AgentRunner::run_for_loop_iteration` call that respects the loop's
//! tool-class allow-list and forbidden-set.
//!
//! **NOTE (2026-05-08):** Phase 9 ships with the stub still in place because
//! AgentRunner has no tool-filter mechanism today (`crates/daemon/src/agent/runner.rs`
//! constructs an immutable `tools: ToolRegistry` once). Adding per-iteration
//! filtering is a separate refactor. Until that lands, iterations *record*
//! correctly but do not actually execute the LLM prompt. The triple-registry
//! tool surfaces are wired (Phases 9.1-9.3) so the LLM can see and call
//! loop.* tools from the main session; iteration-side execution is inert
//! and deferred to a future phase.

use crate::loops::events::LoopEvents;
use crate::loops::types::LoopId;
use nevoflux_storage::models::{IterationStatus, LoopRecord};
use nevoflux_storage::repositories::LoopRepository;
use nevoflux_storage::Database;
use std::sync::Arc;

#[derive(Debug)]
pub enum ExecResult {
    Ok,
    Error(String),
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
    /// Phase 6 stub: reads the loop record, advances `iteration_count`,
    /// inserts a `loop_iterations` row, and marks it `ok`. No AgentRunner
    /// invocation, no tool dispatch yet — that lands in a future phase.
    /// Phase 10: emits `system:loop:iteration_start` and `iteration_end`
    /// EventBus events around the work.
    pub async fn execute(&self, loop_id: LoopId, fire_reason: String) -> ExecResult {
        let repo = LoopRepository::new(&self.db);
        let now = chrono::Utc::now().timestamp();

        let rec = match repo.get(loop_id.as_ref()) {
            Ok(Some(r)) => r,
            Ok(None) => return ExecResult::Error(format!("loop {} vanished", loop_id)),
            Err(e) => return ExecResult::Error(e.to_string()),
        };

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

        let session_id = rec.session_id.clone();
        self.events
            .iteration_start(&session_id, &loop_id, seq, now, &fire_reason)
            .await;

        // Build the §7.2 LOOP-CONTEXT block. Returned as a plain string for
        // now; a future phase turns this into the user_message of an AgentInput.
        let _input = build_user_message(&rec, seq, &fire_reason);

        // STUB: pretend the iteration succeeded.
        let end_now = chrono::Utc::now().timestamp();
        let _ = repo.finish_iteration(iter_id, end_now, IterationStatus::Ok, None, None);

        self.events
            .iteration_end(
                &session_id,
                &loop_id,
                seq,
                end_now,
                "ok",
                serde_json::json!([]),
            )
            .await;

        ExecResult::Ok
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

        assert!(matches!(result, ExecResult::Ok));

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
}
