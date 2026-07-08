//! Schedule repository — CRUD for routines-style scheduled jobs.
//!
//! Mirrors `loop_record.rs`'s style: thin wrappers around
//! `Database::with_connection`/`with_connection_mut`, a free `row_to_*`
//! mapper per table, and a transactional trim-to-50 pattern for run history.

use rusqlite::{params, OptionalExtension, Row};

use crate::connection::Database;
use crate::error::{Result, StorageError};
use crate::models::schedule::{ScheduleRecord, ScheduleRun, ScheduleRunStatus, ScheduleStatus};

const SCHEDULE_COLUMNS: &str =
    "id, creator_session_id, name, cron_expr, at_ts, prompt_text, wrapped_skill,
     mode, browser_policy, on_unavailable, headless_profile, catch_up,
     goal_condition, goal_max_turns, max_tokens_per_run, evaluator_model,
     status, next_fire_at, last_run_status, last_run_at,
     consecutive_failures, run_count, created_at, updated_at";

/// Cap `final_text` at 4096 chars before insert — same cap `daemon::loops::events`
/// applies to loop iteration output, so a long run response doesn't bloat the
/// `schedule_runs` summary row (full transcripts live in messages, not here).
fn truncate_final_text(s: &str) -> String {
    if s.chars().count() > 4096 {
        s.chars().take(4096).collect()
    } else {
        s.to_string()
    }
}

pub struct ScheduleRepository<'a> {
    db: &'a Database,
}

impl<'a> ScheduleRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn create(&self, rec: &ScheduleRecord) -> Result<String> {
        self.db.with_connection(|conn| {
            conn.execute(
                &format!(
                    "INSERT INTO schedules ({SCHEDULE_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                             ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)"
                ),
                params![
                    rec.id,
                    rec.creator_session_id,
                    rec.name,
                    rec.cron_expr,
                    rec.at_ts,
                    rec.prompt_text,
                    rec.wrapped_skill,
                    rec.mode,
                    rec.browser_policy,
                    rec.on_unavailable,
                    rec.headless_profile,
                    rec.catch_up as i64,
                    rec.goal_condition,
                    rec.goal_max_turns,
                    rec.max_tokens_per_run,
                    rec.evaluator_model,
                    rec.status.as_str(),
                    rec.next_fire_at,
                    rec.last_run_status,
                    rec.last_run_at,
                    rec.consecutive_failures,
                    rec.run_count,
                    rec.created_at,
                    rec.updated_at,
                ],
            )?;
            Ok(rec.id.clone())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<ScheduleRecord>> {
        self.db.with_connection(|conn| {
            conn.query_row(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE id = ?1"),
                params![id],
                row_to_schedule,
            )
            .optional()
            .map_err(StorageError::from)
            .and_then(|opt| opt.transpose())
        })
    }

    pub fn list_all(&self) -> Result<Vec<ScheduleRecord>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SCHEDULE_COLUMNS} FROM schedules ORDER BY created_at"
            ))?;
            let rows = stmt.query_map([], row_to_schedule)?;
            rows.map(|r| r?).collect()
        })
    }

    pub fn list_active(&self) -> Result<Vec<ScheduleRecord>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE status = 'active' ORDER BY created_at"
            ))?;
            let rows = stmt.query_map([], row_to_schedule)?;
            rows.map(|r| r?).collect()
        })
    }

    /// Schedules due to fire at or before `now`. One-off schedules that have
    /// already fired move to `ScheduleStatus::Ran`, so the `status = 'active'`
    /// filter naturally excludes them without a separate check.
    pub fn list_due(&self, now: i64) -> Result<Vec<ScheduleRecord>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SCHEDULE_COLUMNS} FROM schedules
                 WHERE status = 'active' AND next_fire_at IS NOT NULL AND next_fire_at <= ?1
                 ORDER BY next_fire_at"
            ))?;
            let rows = stmt.query_map(params![now], row_to_schedule)?;
            rows.map(|r| r?).collect()
        })
    }

    pub fn update_status(&self, id: &str, status: ScheduleStatus, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE schedules SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status.as_str(), now, id],
            )?;
            Ok(())
        })
    }

    pub fn update_next_fire(&self, id: &str, next_fire_at: Option<i64>, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE schedules SET next_fire_at = ?1, updated_at = ?2 WHERE id = ?3",
                params![next_fire_at, now, id],
            )?;
            Ok(())
        })
    }

    /// Advance a schedule past a fire: set the next fire time, stamp
    /// `last_run_at`, and bump `run_count` — all in one UPDATE so readers
    /// never observe a schedule mid-fire.
    pub fn update_after_fire(&self, id: &str, next_fire_at: Option<i64>, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE schedules
                 SET next_fire_at = ?1, last_run_at = ?2, run_count = run_count + 1, updated_at = ?2
                 WHERE id = ?3",
                params![next_fire_at, now, id],
            )?;
            Ok(())
        })
    }

    pub fn set_last_run_status(&self, id: &str, status: &str, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE schedules SET last_run_status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, id],
            )?;
            Ok(())
        })
    }

    pub fn set_consecutive_failures(&self, id: &str, n: i64, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE schedules SET consecutive_failures = ?1, updated_at = ?2 WHERE id = ?3",
                params![n, now, id],
            )?;
            Ok(())
        })
    }

    /// Insert a new `running` row and trim history to the most-recent 50
    /// runs for this schedule, in a single transaction so retention can
    /// never race with another reader observing >50 rows (mirrors
    /// `LoopRepository::insert_iteration`). Returns the new run's row id.
    pub fn record_run_start(
        &self,
        schedule_id: &str,
        started_at: i64,
        fire_kind: &str,
    ) -> Result<i64> {
        self.db.with_connection_mut(|conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO schedule_runs (schedule_id, started_at, status, fire_kind)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    schedule_id,
                    started_at,
                    ScheduleRunStatus::Running.as_str(),
                    fire_kind
                ],
            )?;
            let id: i64 = tx.last_insert_rowid();
            tx.execute(
                "DELETE FROM schedule_runs
                 WHERE schedule_id = ?1
                   AND id NOT IN (
                      SELECT id FROM schedule_runs
                      WHERE schedule_id = ?1
                      ORDER BY started_at DESC
                      LIMIT 50
                   )",
                params![schedule_id],
            )?;
            tx.commit()?;
            Ok(id)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_run_end(
        &self,
        run_id: i64,
        ended_at: i64,
        status: ScheduleRunStatus,
        error: Option<&str>,
        final_text: Option<&str>,
        tokens_used: Option<i64>,
        goal_turns: Option<i64>,
    ) -> Result<()> {
        let final_text_capped = final_text.map(truncate_final_text);
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE schedule_runs
                 SET ended_at = ?1, status = ?2, error_message = ?3, final_text = ?4,
                     tokens_used = ?5, goal_turns = ?6
                 WHERE id = ?7",
                params![
                    ended_at,
                    status.as_str(),
                    error,
                    final_text_capped,
                    tokens_used,
                    goal_turns,
                    run_id,
                ],
            )?;
            Ok(())
        })
    }

    /// Record a fire that was missed (daemon was down at `fire_was_at`,
    /// discovered at `noted_at`). Returns the new `schedule_runs` row id.
    pub fn record_missed(&self, schedule_id: &str, fire_was_at: i64, noted_at: i64) -> Result<i64> {
        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO schedule_runs (schedule_id, started_at, ended_at, status, fire_kind)
                 VALUES (?1, ?2, ?3, ?4, 'scheduled')",
                params![
                    schedule_id,
                    fire_was_at,
                    noted_at,
                    ScheduleRunStatus::Missed.as_str()
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn list_runs(&self, schedule_id: &str, limit: i64) -> Result<Vec<ScheduleRun>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, schedule_id, started_at, ended_at, status, fire_kind,
                        error_message, final_text, tokens_used, goal_turns
                 FROM schedule_runs WHERE schedule_id = ?1
                 ORDER BY started_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![schedule_id, limit], row_to_run)?;
            rows.map(|r| r?).collect()
        })
    }
}

fn row_to_schedule(row: &Row<'_>) -> rusqlite::Result<Result<ScheduleRecord>> {
    let id: String = row.get(0)?;
    let creator_session_id: Option<String> = row.get(1)?;
    let name: String = row.get(2)?;
    let cron_expr: Option<String> = row.get(3)?;
    let at_ts: Option<i64> = row.get(4)?;
    let prompt_text: Option<String> = row.get(5)?;
    let wrapped_skill: Option<String> = row.get(6)?;
    let mode: String = row.get(7)?;
    let browser_policy: String = row.get(8)?;
    let on_unavailable: Option<String> = row.get(9)?;
    let headless_profile: Option<String> = row.get(10)?;
    let catch_up: i64 = row.get(11)?;
    let goal_condition: Option<String> = row.get(12)?;
    let goal_max_turns: Option<i64> = row.get(13)?;
    let max_tokens_per_run: Option<i64> = row.get(14)?;
    let evaluator_model: Option<String> = row.get(15)?;
    let status_str: String = row.get(16)?;
    let next_fire_at: Option<i64> = row.get(17)?;
    let last_run_status: Option<String> = row.get(18)?;
    let last_run_at: Option<i64> = row.get(19)?;
    let consecutive_failures: i64 = row.get(20)?;
    let run_count: i64 = row.get(21)?;
    let created_at: i64 = row.get(22)?;
    let updated_at: i64 = row.get(23)?;

    Ok((|| -> Result<ScheduleRecord> {
        let status = ScheduleStatus::from_db_str(&status_str).ok_or_else(|| {
            StorageError::Migration(format!("unknown schedule status in row: {status_str}"))
        })?;
        Ok(ScheduleRecord {
            id,
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
            catch_up: catch_up != 0,
            goal_condition,
            goal_max_turns,
            max_tokens_per_run,
            evaluator_model,
            status,
            next_fire_at,
            last_run_status,
            last_run_at,
            consecutive_failures,
            run_count,
            created_at,
            updated_at,
        })
    })())
}

fn row_to_run(row: &Row<'_>) -> rusqlite::Result<Result<ScheduleRun>> {
    let id: i64 = row.get(0)?;
    let schedule_id: String = row.get(1)?;
    let started_at: i64 = row.get(2)?;
    let ended_at: Option<i64> = row.get(3)?;
    let status_str: String = row.get(4)?;
    let fire_kind: String = row.get(5)?;
    let error_message: Option<String> = row.get(6)?;
    let final_text: Option<String> = row.get(7)?;
    let tokens_used: Option<i64> = row.get(8)?;
    let goal_turns: Option<i64> = row.get(9)?;

    Ok((|| -> Result<ScheduleRun> {
        let status = ScheduleRunStatus::from_db_str(&status_str).ok_or_else(|| {
            StorageError::Migration(format!("unknown schedule run status in row: {status_str}"))
        })?;
        Ok(ScheduleRun {
            id,
            schedule_id,
            started_at,
            ended_at,
            status,
            fire_kind,
            error_message,
            final_text,
            tokens_used,
            goal_turns,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::schedule::{ScheduleRecord, ScheduleRunStatus, ScheduleStatus};

    fn test_db() -> crate::Database {
        crate::Database::open_in_memory().expect("in-memory db")
    }

    fn sample(id: &str) -> ScheduleRecord {
        ScheduleRecord {
            id: id.to_string(),
            creator_session_id: None,
            name: "daily-report".into(),
            cron_expr: Some("0 9 * * *".into()),
            at_ts: None,
            prompt_text: Some("Summarize the news".into()),
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
            status: ScheduleStatus::Active,
            next_fire_at: Some(1_800_000_000),
            last_run_status: None,
            last_run_at: None,
            consecutive_failures: 0,
            run_count: 0,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn create_get_roundtrip() {
        let db = test_db();
        let repo = ScheduleRepository::new(&db);
        repo.create(&sample("sch00001")).unwrap();
        let got = repo.get("sch00001").unwrap().expect("exists");
        assert_eq!(got.name, "daily-report");
        assert_eq!(got.cron_expr.as_deref(), Some("0 9 * * *"));
        assert_eq!(got.status, ScheduleStatus::Active);
    }

    #[test]
    fn due_query_and_fire_update() {
        let db = test_db();
        let repo = ScheduleRepository::new(&db);
        repo.create(&sample("sch00001")).unwrap();
        let due = repo.list_due(1_800_000_001).unwrap();
        assert_eq!(due.len(), 1);
        repo.update_after_fire("sch00001", Some(1_800_003_600), 1_800_000_001)
            .unwrap();
        let got = repo.get("sch00001").unwrap().unwrap();
        assert_eq!(got.next_fire_at, Some(1_800_003_600));
        assert_eq!(got.run_count, 1);
        assert!(repo.list_due(1_800_000_001).unwrap().is_empty());
    }

    #[test]
    fn run_lifecycle_and_history() {
        let db = test_db();
        let repo = ScheduleRepository::new(&db);
        repo.create(&sample("sch00001")).unwrap();
        let run_id = repo
            .record_run_start("sch00001", 1_800_000_000, "scheduled")
            .unwrap();
        repo.record_run_end(
            run_id,
            1_800_000_060,
            ScheduleRunStatus::Ok,
            None,
            Some("done"),
            Some(1234),
            None,
        )
        .unwrap();
        let runs = repo.list_runs("sch00001", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, ScheduleRunStatus::Ok);
        assert_eq!(runs[0].tokens_used, Some(1234));
    }

    #[test]
    fn one_off_ran_status_excluded_from_due() {
        let db = test_db();
        let repo = ScheduleRepository::new(&db);
        let mut rec = sample("sch00002");
        rec.cron_expr = None;
        rec.at_ts = Some(1_800_000_000);
        repo.create(&rec).unwrap();
        repo.update_status("sch00002", ScheduleStatus::Ran, 1_800_000_001)
            .unwrap();
        assert!(repo.list_due(1_900_000_000).unwrap().is_empty());
    }
}
