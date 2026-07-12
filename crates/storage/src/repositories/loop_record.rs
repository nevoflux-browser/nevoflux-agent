//! Loop repository — CRUD for the /loop skill (spec §6.1).

use rusqlite::{params, OptionalExtension, Row};

use crate::connection::Database;
use crate::error::{Result, StorageError};
use crate::models::{IterationStatus, LoopRecord, LoopState};
use crate::repositories::truncate_final_text;

/// A compact past-iteration record for the loop's "recent runs" log.
#[derive(Debug, Clone)]
pub struct RecentIteration {
    pub sequence_number: i64,
    pub ended_at: Option<i64>,
    pub status: String,
    pub final_text: Option<String>,
}

pub struct LoopRepository<'a> {
    db: &'a Database,
}

impl<'a> LoopRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn create(&self, rec: &LoopRecord) -> Result<String> {
        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO loops
                    (id, session_id, trigger_expr, prompt_text, wrapped_skill,
                     mode, scratchpad, state, consecutive_failures,
                     skipped_triggers, iteration_count, created_at, updated_at,
                     gate_kind, gate_spec, gate_last_value)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    rec.id,
                    rec.session_id,
                    rec.trigger_expr,
                    rec.prompt_text,
                    rec.wrapped_skill,
                    rec.mode,
                    rec.scratchpad,
                    rec.state.as_str(),
                    rec.consecutive_failures,
                    rec.skipped_triggers,
                    rec.iteration_count,
                    rec.created_at,
                    rec.updated_at,
                    rec.gate_kind,
                    rec.gate_spec,
                    rec.gate_last_value,
                ],
            )?;
            Ok(rec.id.clone())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<LoopRecord>> {
        self.db.with_connection(|conn| {
            conn.query_row(
                "SELECT id, session_id, trigger_expr, prompt_text, wrapped_skill,
                        mode, scratchpad, state,
                        consecutive_failures, skipped_triggers, iteration_count,
                        created_at, updated_at,
                        gate_kind, gate_spec, gate_last_value
                 FROM loops WHERE id = ?1",
                params![id],
                row_to_loop,
            )
            .optional()
            .map_err(StorageError::from)
            .and_then(|opt| opt.transpose())
        })
    }

    pub fn update_state(&self, id: &str, state: LoopState, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE loops SET state = ?1, updated_at = ?2 WHERE id = ?3",
                params![state.as_str(), now, id],
            )?;
            Ok(())
        })
    }

    /// Atomically transition a loop into a terminal state (`Cancelled` /
    /// `Failed`), but only if it is not already terminal. Returns whether a
    /// row actually flipped. This is the concurrency primitive for every
    /// caller that pairs a `pending_work` decrement with a terminal
    /// transition (cancel, dispatcher auto-fail): two racing writers (a
    /// double cancel, or cancel racing the 3-strike auto-fail) are
    /// serialized by SQLite and exactly one observes `true`, so the
    /// decrement can never be paired with a stale read-then-write snapshot.
    /// Mirrors `ScheduleRepository::transition_status`.
    pub fn transition_to_terminal(&self, id: &str, new_state: LoopState, now: i64) -> Result<bool> {
        debug_assert!(
            matches!(new_state, LoopState::Cancelled | LoopState::Failed),
            "transition_to_terminal must target a terminal state"
        );
        self.db.with_connection(|conn| {
            let n = conn.execute(
                "UPDATE loops SET state = ?1, updated_at = ?2
                 WHERE id = ?3 AND state NOT IN ('cancelled', 'failed')",
                params![new_state.as_str(), now, id],
            )?;
            Ok(n > 0)
        })
    }

    pub fn update_scratchpad(&self, id: &str, content: &str, now: i64) -> Result<()> {
        // Note: 4096-byte cap is enforced by the SQL CHECK; we let it raise.
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE loops SET scratchpad = ?1, updated_at = ?2 WHERE id = ?3",
                params![content, now, id],
            )?;
            Ok(())
        })
    }

    pub fn increment_skipped(&self, id: &str, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE loops SET skipped_triggers = skipped_triggers + 1, updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            Ok(())
        })
    }

    pub fn increment_iteration_count(&self, id: &str, now: i64) -> Result<i64> {
        // rusqlite's `RETURNING` support depends on the build; do it as
        // UPDATE-then-SELECT inside one connection borrow.
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE loops SET iteration_count = iteration_count + 1, updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            let n: i64 = conn.query_row(
                "SELECT iteration_count FROM loops WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(n)
        })
    }

    pub fn set_consecutive_failures(&self, id: &str, n: i64, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE loops SET consecutive_failures = ?1, updated_at = ?2 WHERE id = ?3",
                params![n, now, id],
            )?;
            Ok(())
        })
    }

    /// Insert a new iteration and trim history to the most-recent 50 rows
    /// for this loop. Returns the new iteration row id.
    ///
    /// The insert + trim run in a single transaction so retention can never
    /// race with another reader observing >50 rows.
    pub fn insert_iteration(
        &self,
        loop_id: &str,
        sequence_number: i64,
        started_at: i64,
        status: IterationStatus,
    ) -> Result<i64> {
        self.db.with_connection_mut(|conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO loop_iterations (loop_id, sequence_number, started_at, status)
                 VALUES (?1, ?2, ?3, ?4)",
                params![loop_id, sequence_number, started_at, status.as_str()],
            )?;
            let id: i64 = tx.last_insert_rowid();
            tx.execute(
                "DELETE FROM loop_iterations
                 WHERE loop_id = ?1
                   AND id NOT IN (
                      SELECT id FROM loop_iterations
                      WHERE loop_id = ?1
                      ORDER BY sequence_number DESC
                      LIMIT 50
                   )",
                params![loop_id],
            )?;
            tx.commit()?;
            Ok(id)
        })
    }

    pub fn finish_iteration(
        &self,
        iteration_id: i64,
        ended_at: i64,
        status: IterationStatus,
        error: Option<&str>,
        tool_calls_json: Option<&str>,
        final_text: Option<&str>,
        tokens_used: Option<i64>,
    ) -> Result<()> {
        // Cap final_text at 4096 chars, same as `schedule_runs.final_text`
        // (ScheduleRepository::record_run_end) and the event payload cap in
        // `daemon::loops::events::iteration_end`.
        let final_text_capped = final_text.map(truncate_final_text);
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE loop_iterations
                 SET ended_at = ?1, status = ?2, error_message = ?3, tool_calls_json = ?4,
                     final_text = ?5, tokens_used = ?6
                 WHERE id = ?7",
                params![
                    ended_at,
                    status.as_str(),
                    error,
                    tool_calls_json,
                    final_text_capped,
                    tokens_used,
                    iteration_id
                ],
            )?;
            Ok(())
        })
    }

    /// The most-recent `limit` finished iterations, newest first, for feeding
    /// back into the next iteration's LOOP-CONTEXT.
    pub fn recent_iterations(&self, loop_id: &str, limit: usize) -> Result<Vec<RecentIteration>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT sequence_number, ended_at, status, final_text
                   FROM loop_iterations
                  WHERE loop_id = ?1 AND status != 'running'
               ORDER BY sequence_number DESC
                  LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![loop_id, limit as i64], |r| {
                Ok(RecentIteration {
                    sequence_number: r.get(0)?,
                    ended_at: r.get(1)?,
                    status: r.get(2)?,
                    final_text: r.get(3)?,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Into::into)
        })
    }

    pub fn list_by_session(&self, session_id: &str) -> Result<Vec<LoopRecord>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, trigger_expr, prompt_text, wrapped_skill,
                        mode, scratchpad, state,
                        consecutive_failures, skipped_triggers, iteration_count,
                        created_at, updated_at,
                        gate_kind, gate_spec, gate_last_value
                 FROM loops WHERE session_id = ?1 ORDER BY created_at",
            )?;
            let rows = stmt.query_map(params![session_id], row_to_loop)?;
            rows.map(|r| r?).collect()
        })
    }

    pub fn list_running_or_pending(&self) -> Result<Vec<LoopRecord>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, trigger_expr, prompt_text, wrapped_skill,
                        mode, scratchpad, state,
                        consecutive_failures, skipped_triggers, iteration_count,
                        created_at, updated_at,
                        gate_kind, gate_spec, gate_last_value
                 FROM loops WHERE state IN ('pending', 'running') ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], row_to_loop)?;
            rows.map(|r| r?).collect()
        })
    }

    /// Persist the last observed gate value (deterministic-gate diff cursor,
    /// W3 spec). No-op semantics if `loop_id` doesn't exist: 0 rows affected,
    /// `Ok(())` — mirrors `update_scratchpad`/`set_consecutive_failures`.
    pub fn set_gate_last_value(&self, loop_id: &str, value: &str) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE loops SET gate_last_value = ?1, updated_at = ?2 WHERE id = ?3",
                params![value, now, loop_id],
            )?;
            Ok(())
        })
    }
}

fn row_to_loop(row: &Row<'_>) -> rusqlite::Result<Result<LoopRecord>> {
    let id: String = row.get(0)?;
    let session_id: String = row.get(1)?;
    let trigger_expr: String = row.get(2)?;
    let prompt_text: Option<String> = row.get(3)?;
    let wrapped_skill: Option<String> = row.get(4)?;
    let mode: String = row.get(5)?;
    let scratchpad: String = row.get(6)?;
    let state_str: String = row.get(7)?;
    let consecutive_failures: i64 = row.get(8)?;
    let skipped_triggers: i64 = row.get(9)?;
    let iteration_count: i64 = row.get(10)?;
    let created_at: i64 = row.get(11)?;
    let updated_at: i64 = row.get(12)?;
    let gate_kind: String = row.get(13)?;
    let gate_spec: Option<String> = row.get(14)?;
    let gate_last_value: Option<String> = row.get(15)?;

    Ok((|| -> Result<LoopRecord> {
        let state = LoopState::from_db_str(&state_str).ok_or_else(|| {
            StorageError::Migration(format!("unknown loop state in row: {state_str}"))
        })?;
        Ok(LoopRecord {
            id,
            session_id,
            trigger_expr,
            prompt_text,
            wrapped_skill,
            mode,
            scratchpad,
            state,
            consecutive_failures,
            skipped_triggers,
            iteration_count,
            created_at,
            updated_at,
            gate_kind,
            gate_spec,
            gate_last_value,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::CreateSessionParams;
    use crate::Storage;

    fn fresh() -> Storage {
        let s = Storage::open_in_memory().unwrap();
        s.sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        s
    }

    fn sample_loop(id: &str) -> LoopRecord {
        LoopRecord {
            id: id.into(),
            session_id: "s1".into(),
            trigger_expr: "time:5m".into(),
            prompt_text: Some("p".into()),
            wrapped_skill: None,
            mode: "chat".into(),
            scratchpad: String::new(),
            state: LoopState::Pending,
            consecutive_failures: 0,
            skipped_triggers: 0,
            iteration_count: 0,
            created_at: 100,
            updated_at: 100,
            gate_kind: "none".into(),
            gate_spec: None,
            gate_last_value: None,
        }
    }

    #[test]
    fn create_and_fetch_round_trip() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        let id = repo.create(&sample_loop("abcd1234")).unwrap();
        assert_eq!(id, "abcd1234");

        let row = repo.get("abcd1234").unwrap().unwrap();
        assert_eq!(row.trigger_expr, "time:5m");
        assert_eq!(row.state, LoopState::Pending);
        assert_eq!(row.mode, "chat");
    }

    #[test]
    fn get_missing_returns_none() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        assert!(repo.get("nope").unwrap().is_none());
    }

    #[test]
    fn update_state_persists() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        repo.update_state("abc", LoopState::Running, 200).unwrap();
        assert_eq!(repo.get("abc").unwrap().unwrap().state, LoopState::Running);
    }

    #[test]
    fn update_scratchpad_under_4kb_succeeds() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        repo.update_scratchpad("abc", "k=v", 200).unwrap();
        assert_eq!(repo.get("abc").unwrap().unwrap().scratchpad, "k=v");
    }

    #[test]
    fn update_scratchpad_over_4kb_rejected_by_check() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        let big = "x".repeat(4097);
        assert!(repo.update_scratchpad("abc", &big, 200).is_err());
    }

    #[test]
    fn increment_skipped_triggers() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        repo.increment_skipped("abc", 200).unwrap();
        repo.increment_skipped("abc", 201).unwrap();
        assert_eq!(repo.get("abc").unwrap().unwrap().skipped_triggers, 2);
    }

    #[test]
    fn increment_iteration_count_returns_new_value() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        let n = repo.increment_iteration_count("abc", 200).unwrap();
        assert_eq!(n, 1);
        let n = repo.increment_iteration_count("abc", 201).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn set_consecutive_failures_persists() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        repo.set_consecutive_failures("abc", 3, 200).unwrap();
        assert_eq!(repo.get("abc").unwrap().unwrap().consecutive_failures, 3);
    }

    #[test]
    fn insert_iteration_returns_id() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        let id = repo
            .insert_iteration("abc", 1, 100, IterationStatus::Running)
            .unwrap();
        assert!(id > 0);
    }

    #[test]
    fn insert_iteration_trims_to_50() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        for n in 1..=55 {
            repo.insert_iteration("abc", n, 100 + n, IterationStatus::Ok)
                .unwrap();
        }
        let kept = s
            .database()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM loop_iterations WHERE loop_id = ?1",
                    params!["abc"],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(crate::error::StorageError::from)
            })
            .unwrap();
        assert_eq!(kept, 50);
        let oldest = s
            .database()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT MIN(sequence_number) FROM loop_iterations WHERE loop_id = ?1",
                    params!["abc"],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(crate::error::StorageError::from)
            })
            .unwrap();
        assert_eq!(oldest, 6);
    }

    #[test]
    fn finish_iteration_writes_status_and_trace() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        let id = repo
            .insert_iteration("abc", 1, 100, IterationStatus::Running)
            .unwrap();
        repo.finish_iteration(id, 110, IterationStatus::Ok, None, Some("[]"), None, None)
            .unwrap();
        // Spot check via raw SQL since we don't have list_iterations yet:
        let row: (i64, String, Option<String>) = s
            .database()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT ended_at, status, tool_calls_json FROM loop_iterations WHERE id = ?1",
                    params![id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .map_err(crate::error::StorageError::from)
            })
            .unwrap();
        assert_eq!(row.0, 110);
        assert_eq!(row.1, "ok");
        assert_eq!(row.2.as_deref(), Some("[]"));
    }

    #[test]
    fn finish_iteration_persists_final_text_and_tokens() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        let id = repo
            .insert_iteration("abc", 1, 100, IterationStatus::Running)
            .unwrap();
        repo.finish_iteration(
            id,
            110,
            IterationStatus::Ok,
            None,
            Some("[]"),
            Some("digested 3 new items"),
            Some(1234),
        )
        .unwrap();
        let row: (Option<String>, Option<i64>) = s
            .database()
            .with_connection(|c| {
                c.query_row(
                    "SELECT final_text, tokens_used FROM loop_iterations WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(crate::error::StorageError::from)
            })
            .unwrap();
        assert_eq!(row.0.as_deref(), Some("digested 3 new items"));
        assert_eq!(row.1, Some(1234));
    }

    #[test]
    fn finish_iteration_truncates_final_text_over_4096_chars() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        let id = repo
            .insert_iteration("abc", 1, 100, IterationStatus::Running)
            .unwrap();
        let long_text = "x".repeat(5000);
        repo.finish_iteration(
            id,
            110,
            IterationStatus::Ok,
            None,
            None,
            Some(&long_text),
            None,
        )
        .unwrap();
        let stored: Option<String> = s
            .database()
            .with_connection(|c| {
                c.query_row(
                    "SELECT final_text FROM loop_iterations WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .map_err(crate::error::StorageError::from)
            })
            .unwrap();
        let stored = stored.expect("final_text should be stored");
        assert!(
            stored.chars().count() <= 4096,
            "stored final_text length {} exceeds cap",
            stored.chars().count()
        );
        assert_eq!(stored.chars().count(), 4096);
    }

    #[test]
    fn list_by_session_filters_correctly() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("a")).unwrap();

        s.sessions()
            .create(CreateSessionParams::new().with_id("other").with_title("x"))
            .unwrap();
        let mut other = sample_loop("b");
        other.session_id = "other".into();
        repo.create(&other).unwrap();

        let in_s1 = repo.list_by_session("s1").unwrap();
        assert_eq!(in_s1.len(), 1);
        assert_eq!(in_s1[0].id, "a");
    }

    #[test]
    fn transition_to_terminal_flips_non_terminal_row() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();

        let flipped = repo
            .transition_to_terminal("abc", LoopState::Cancelled, 200)
            .unwrap();
        assert!(flipped, "pending -> cancelled must flip");
        assert_eq!(
            repo.get("abc").unwrap().unwrap().state,
            LoopState::Cancelled
        );
    }

    #[test]
    fn transition_to_terminal_is_noop_when_already_terminal() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();

        let first = repo
            .transition_to_terminal("abc", LoopState::Cancelled, 200)
            .unwrap();
        assert!(first);

        // Racing second caller (e.g. concurrent cancel, or cancel racing an
        // auto-fail) must observe `false` and must NOT clobber the row.
        let second = repo
            .transition_to_terminal("abc", LoopState::Failed, 201)
            .unwrap();
        assert!(!second, "already-terminal row must not flip again");
        assert_eq!(
            repo.get("abc").unwrap().unwrap().state,
            LoopState::Cancelled,
            "state must stay at the state the winning transition set"
        );
    }

    #[test]
    fn list_running_or_pending_returns_both_states() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        let mut a = sample_loop("a");
        a.state = LoopState::Pending;
        repo.create(&a).unwrap();
        let mut b = sample_loop("b");
        b.state = LoopState::Running;
        repo.create(&b).unwrap();
        let mut c = sample_loop("c");
        c.state = LoopState::Idle;
        repo.create(&c).unwrap();

        let active = repo.list_running_or_pending().unwrap();
        let mut ids: Vec<&str> = active.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn gate_columns_round_trip_and_set_gate_last_value() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        let mut rec = sample_loop("gate1");
        rec.gate_kind = "http".into();
        rec.gate_spec = Some(r#"{"url":"https://x","extract":"$.v"}"#.into());
        repo.create(&rec).unwrap();

        let row = repo.get("gate1").unwrap().unwrap();
        assert_eq!(row.gate_kind, "http");
        assert_eq!(
            row.gate_spec.as_deref(),
            Some(r#"{"url":"https://x","extract":"$.v"}"#)
        );
        assert_eq!(row.gate_last_value, None);

        repo.set_gate_last_value("gate1", "42").unwrap();
        let row = repo.get("gate1").unwrap().unwrap();
        assert_eq!(row.gate_last_value.as_deref(), Some("42"));
    }

    #[test]
    fn recent_iterations_returns_newest_first_capped() {
        let s = fresh();
        let repo = LoopRepository::new(s.database());
        repo.create(&sample_loop("abc")).unwrap();
        for seq in 1..=3 {
            let id = repo
                .insert_iteration("abc", seq, seq * 100, IterationStatus::Running)
                .unwrap();
            repo.finish_iteration(
                id,
                seq * 100 + 5,
                IterationStatus::Ok,
                None,
                Some("[]"),
                Some(&format!("run {seq}")),
                None,
            )
            .unwrap();
        }
        let recent = repo.recent_iterations("abc", 2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].sequence_number, 3); // newest first
        assert_eq!(recent[0].final_text.as_deref(), Some("run 3"));
        assert_eq!(recent[1].sequence_number, 2);
    }
}
