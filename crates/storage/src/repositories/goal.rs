//! Goal repository — CRUD for session-scoped goal conditions.
//!
//! Mirrors `schedule.rs`'s style: thin wrappers around
//! `Database::with_connection`/`with_connection_mut`, a free `row_to_goal`
//! mapper, and a transactional "replace prior active goal" pattern on
//! `create` so the `goals_session_active_uidx` partial unique index (one
//! active goal per session) is never violated.

use rusqlite::{params, OptionalExtension, Row};

use crate::connection::Database;
use crate::error::{Result, StorageError};
use crate::models::goal::{GoalRecord, GoalStatus};

const GOAL_COLUMNS: &str = "id, session_id, condition, evaluator_provider, evaluator_model,
     max_turns, turns_used, status, last_reason, created_at, updated_at, achieved_at";

pub struct GoalRepository<'a> {
    db: &'a Database,
}

impl<'a> GoalRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Insert a new goal, replacing any prior active goal for the same
    /// session (set to `Cleared`) inside one transaction — this keeps the
    /// `goals_session_active_uidx` partial unique index satisfied even
    /// though the caller is not required to clear the old goal first.
    pub fn create(&self, rec: &GoalRecord) -> Result<String> {
        self.db.with_connection_mut(|conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE goals SET status = ?1, updated_at = ?2
                 WHERE session_id = ?3 AND status = 'active'",
                params![GoalStatus::Cleared.as_str(), rec.updated_at, rec.session_id],
            )?;
            tx.execute(
                &format!(
                    "INSERT INTO goals ({GOAL_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
                ),
                params![
                    rec.id,
                    rec.session_id,
                    rec.condition,
                    rec.evaluator_provider,
                    rec.evaluator_model,
                    rec.max_turns,
                    rec.turns_used,
                    rec.status.as_str(),
                    rec.last_reason,
                    rec.created_at,
                    rec.updated_at,
                    rec.achieved_at,
                ],
            )?;
            tx.commit()?;
            Ok(rec.id.clone())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<GoalRecord>> {
        self.db.with_connection(|conn| {
            conn.query_row(
                &format!("SELECT {GOAL_COLUMNS} FROM goals WHERE id = ?1"),
                params![id],
                row_to_goal,
            )
            .optional()
            .map_err(StorageError::from)
            .and_then(|opt| opt.transpose())
        })
    }

    /// The current active goal for a session, if any. The partial unique
    /// index guarantees at most one row can match.
    pub fn get_active(&self, session_id: &str) -> Result<Option<GoalRecord>> {
        self.db.with_connection(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {GOAL_COLUMNS} FROM goals WHERE session_id = ?1 AND status = 'active'"
                ),
                params![session_id],
                row_to_goal,
            )
            .optional()
            .map_err(StorageError::from)
            .and_then(|opt| opt.transpose())
        })
    }

    /// The most recently created goal for a session, regardless of status —
    /// used to report `goal_status` after a goal has already resolved
    /// (achieved/expired/cleared) and is no longer "active".
    pub fn latest(&self, session_id: &str) -> Result<Option<GoalRecord>> {
        self.db.with_connection(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {GOAL_COLUMNS} FROM goals WHERE session_id = ?1
                     ORDER BY created_at DESC LIMIT 1"
                ),
                params![session_id],
                row_to_goal,
            )
            .optional()
            .map_err(StorageError::from)
            .and_then(|opt| opt.transpose())
        })
    }

    /// Bump `turns_used` by one, stamp `last_reason`, and return the new
    /// turn count. UPDATE-then-SELECT in one connection borrow (mirrors
    /// `LoopRepository::increment_iteration_count`) since `RETURNING`
    /// support depends on the rusqlite build.
    pub fn increment_turns(&self, id: &str, reason: &str, now: i64) -> Result<i64> {
        self.db.with_connection(|conn| {
            conn.execute(
                "UPDATE goals SET turns_used = turns_used + 1, last_reason = ?1, updated_at = ?2
                 WHERE id = ?3",
                params![reason, now, id],
            )?;
            let n: i64 = conn.query_row(
                "SELECT turns_used FROM goals WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(n)
        })
    }

    /// Transition a goal's status, stamping `achieved_at` when moving to
    /// `Achieved`.
    pub fn set_status(&self, id: &str, status: GoalStatus, now: i64) -> Result<()> {
        self.db.with_connection(|conn| {
            let achieved_at = if status == GoalStatus::Achieved {
                Some(now)
            } else {
                None
            };
            conn.execute(
                "UPDATE goals SET status = ?1, updated_at = ?2,
                     achieved_at = COALESCE(?3, achieved_at)
                 WHERE id = ?4",
                params![status.as_str(), now, achieved_at, id],
            )?;
            Ok(())
        })
    }
}

fn row_to_goal(row: &Row<'_>) -> rusqlite::Result<Result<GoalRecord>> {
    let id: String = row.get(0)?;
    let session_id: String = row.get(1)?;
    let condition: String = row.get(2)?;
    let evaluator_provider: Option<String> = row.get(3)?;
    let evaluator_model: Option<String> = row.get(4)?;
    let max_turns: i64 = row.get(5)?;
    let turns_used: i64 = row.get(6)?;
    let status_str: String = row.get(7)?;
    let last_reason: Option<String> = row.get(8)?;
    let created_at: i64 = row.get(9)?;
    let updated_at: i64 = row.get(10)?;
    let achieved_at: Option<i64> = row.get(11)?;

    Ok((|| -> Result<GoalRecord> {
        let status = GoalStatus::from_db_str(&status_str).ok_or_else(|| {
            StorageError::Migration(format!("unknown goal status in row: {status_str}"))
        })?;
        Ok(GoalRecord {
            id,
            session_id,
            condition,
            evaluator_provider,
            evaluator_model,
            max_turns,
            turns_used,
            status,
            last_reason,
            created_at,
            updated_at,
            achieved_at,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::goal::{GoalRecord, GoalStatus};

    fn test_db() -> crate::Database {
        crate::Database::open_in_memory().expect("in-memory db")
    }

    /// Seed a session row so the `goals.session_id` FK is satisfiable.
    fn seed_session(db: &crate::Database, session_id: &str) {
        db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, created_at, updated_at) VALUES (?1, 1_700_000_000, 1_700_000_000)",
                params![session_id],
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn sample(id: &str, session_id: &str) -> GoalRecord {
        GoalRecord {
            id: id.to_string(),
            session_id: session_id.to_string(),
            condition: "the report has been posted to #general".to_string(),
            evaluator_provider: Some("anthropic".to_string()),
            evaluator_model: Some("claude-haiku-4-5".to_string()),
            max_turns: 20,
            turns_used: 0,
            status: GoalStatus::Active,
            last_reason: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            achieved_at: None,
        }
    }

    #[test]
    fn create_get_roundtrip() {
        let db = test_db();
        seed_session(&db, "sess-1");
        let repo = GoalRepository::new(&db);
        repo.create(&sample("goal00001", "sess-1")).unwrap();

        let got = repo.get("goal00001").unwrap().expect("exists");
        assert_eq!(got.session_id, "sess-1");
        assert_eq!(got.condition, "the report has been posted to #general");
        assert_eq!(got.evaluator_provider.as_deref(), Some("anthropic"));
        assert_eq!(got.max_turns, 20);
        assert_eq!(got.turns_used, 0);
        assert_eq!(got.status, GoalStatus::Active);
        assert!(got.achieved_at.is_none());
    }

    #[test]
    fn create_replaces_prior_active_goal_for_session() {
        let db = test_db();
        seed_session(&db, "sess-1");
        let repo = GoalRepository::new(&db);

        let mut first = sample("goal00001", "sess-1");
        first.created_at = 1_700_000_000;
        first.updated_at = 1_700_000_000;
        repo.create(&first).unwrap();

        let mut second = sample("goal00002", "sess-1");
        second.condition = "the PR has been merged".to_string();
        second.created_at = 1_700_000_100;
        second.updated_at = 1_700_000_100;
        repo.create(&second).unwrap();

        // First goal was cleared, second is now the sole active goal.
        assert_eq!(
            repo.get("goal00001").unwrap().unwrap().status,
            GoalStatus::Cleared
        );
        let active = repo.get_active("sess-1").unwrap().expect("one active goal");
        assert_eq!(active.id, "goal00002");

        // Only one row for the session may carry status='active' — this
        // would fail with a UNIQUE constraint violation if `create` hadn't
        // cleared the prior goal first.
        let active_count: i64 = db
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM goals WHERE session_id = 'sess-1' AND status = 'active'",
                    [],
                    |row| row.get(0),
                )
                .map_err(StorageError::from)
            })
            .unwrap();
        assert_eq!(active_count, 1);
    }

    #[test]
    fn increment_turns_updates_count_and_reason() {
        let db = test_db();
        seed_session(&db, "sess-1");
        let repo = GoalRepository::new(&db);
        repo.create(&sample("goal00001", "sess-1")).unwrap();

        let n = repo
            .increment_turns("goal00001", "condition not yet met", 1_700_000_050)
            .unwrap();
        assert_eq!(n, 1);
        let n = repo
            .increment_turns("goal00001", "still waiting on the merge", 1_700_000_100)
            .unwrap();
        assert_eq!(n, 2);

        let got = repo.get("goal00001").unwrap().unwrap();
        assert_eq!(got.turns_used, 2);
        assert_eq!(
            got.last_reason.as_deref(),
            Some("still waiting on the merge")
        );
        assert_eq!(got.updated_at, 1_700_000_100);
    }

    #[test]
    fn set_status_achieved_stamps_achieved_at() {
        let db = test_db();
        seed_session(&db, "sess-1");
        let repo = GoalRepository::new(&db);
        repo.create(&sample("goal00001", "sess-1")).unwrap();

        repo.set_status("goal00001", GoalStatus::Achieved, 1_700_000_200)
            .unwrap();
        let got = repo.get("goal00001").unwrap().unwrap();
        assert_eq!(got.status, GoalStatus::Achieved);
        assert_eq!(got.achieved_at, Some(1_700_000_200));
        assert_eq!(got.updated_at, 1_700_000_200);
    }

    #[test]
    fn set_status_non_achieved_leaves_achieved_at_untouched() {
        let db = test_db();
        seed_session(&db, "sess-1");
        let repo = GoalRepository::new(&db);
        repo.create(&sample("goal00001", "sess-1")).unwrap();

        repo.set_status("goal00001", GoalStatus::Expired, 1_700_000_200)
            .unwrap();
        let got = repo.get("goal00001").unwrap().unwrap();
        assert_eq!(got.status, GoalStatus::Expired);
        assert!(got.achieved_at.is_none());
    }

    #[test]
    fn latest_returns_most_recently_created_goal() {
        let db = test_db();
        seed_session(&db, "sess-1");
        let repo = GoalRepository::new(&db);

        let mut first = sample("goal00001", "sess-1");
        first.created_at = 1_700_000_000;
        first.updated_at = 1_700_000_000;
        repo.create(&first).unwrap();

        let mut second = sample("goal00002", "sess-1");
        second.created_at = 1_700_000_100;
        second.updated_at = 1_700_000_100;
        repo.create(&second).unwrap();

        let latest = repo.latest("sess-1").unwrap().expect("latest exists");
        assert_eq!(latest.id, "goal00002");
    }

    #[test]
    fn get_active_returns_none_when_no_active_goal() {
        let db = test_db();
        seed_session(&db, "sess-1");
        let repo = GoalRepository::new(&db);
        assert!(repo.get_active("sess-1").unwrap().is_none());

        repo.create(&sample("goal00001", "sess-1")).unwrap();
        repo.set_status("goal00001", GoalStatus::Cleared, 1_700_000_100)
            .unwrap();
        assert!(repo.get_active("sess-1").unwrap().is_none());
    }
}
