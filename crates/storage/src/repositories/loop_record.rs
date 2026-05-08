//! Loop repository — CRUD for the /loop skill (spec §6.1).

use rusqlite::{params, OptionalExtension, Row};

use crate::connection::Database;
use crate::error::{Result, StorageError};
use crate::models::{LoopRecord, LoopState};

pub struct LoopRepository<'a> {
    db: &'a Database,
}

impl<'a> LoopRepository<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn create(&self, rec: &LoopRecord) -> Result<String> {
        let classes_json = serde_json::to_string(&rec.allowed_tool_classes)?;
        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO loops
                    (id, session_id, trigger_expr, prompt_text, wrapped_skill,
                     allowed_tool_classes, scratchpad, state, consecutive_failures,
                     skipped_triggers, iteration_count, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    rec.id,
                    rec.session_id,
                    rec.trigger_expr,
                    rec.prompt_text,
                    rec.wrapped_skill,
                    classes_json,
                    rec.scratchpad,
                    rec.state.as_str(),
                    rec.consecutive_failures,
                    rec.skipped_triggers,
                    rec.iteration_count,
                    rec.created_at,
                    rec.updated_at,
                ],
            )?;
            Ok(rec.id.clone())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<LoopRecord>> {
        self.db.with_connection(|conn| {
            conn.query_row(
                "SELECT id, session_id, trigger_expr, prompt_text, wrapped_skill,
                        allowed_tool_classes, scratchpad, state,
                        consecutive_failures, skipped_triggers, iteration_count,
                        created_at, updated_at
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
}

fn row_to_loop(row: &Row<'_>) -> rusqlite::Result<Result<LoopRecord>> {
    let id: String = row.get(0)?;
    let session_id: String = row.get(1)?;
    let trigger_expr: String = row.get(2)?;
    let prompt_text: Option<String> = row.get(3)?;
    let wrapped_skill: Option<String> = row.get(4)?;
    let classes_json: String = row.get(5)?;
    let scratchpad: String = row.get(6)?;
    let state_str: String = row.get(7)?;
    let consecutive_failures: i64 = row.get(8)?;
    let skipped_triggers: i64 = row.get(9)?;
    let iteration_count: i64 = row.get(10)?;
    let created_at: i64 = row.get(11)?;
    let updated_at: i64 = row.get(12)?;

    Ok((|| -> Result<LoopRecord> {
        let allowed_tool_classes: Vec<String> = serde_json::from_str(&classes_json)?;
        let state = LoopState::from_str(&state_str).ok_or_else(|| {
            StorageError::Migration(format!("unknown loop state in row: {state_str}"))
        })?;
        Ok(LoopRecord {
            id,
            session_id,
            trigger_expr,
            prompt_text,
            wrapped_skill,
            allowed_tool_classes,
            scratchpad,
            state,
            consecutive_failures,
            skipped_triggers,
            iteration_count,
            created_at,
            updated_at,
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
            allowed_tool_classes: vec!["read".into()],
            scratchpad: String::new(),
            state: LoopState::Pending,
            consecutive_failures: 0,
            skipped_triggers: 0,
            iteration_count: 0,
            created_at: 100,
            updated_at: 100,
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
        assert_eq!(row.allowed_tool_classes, vec!["read".to_string()]);
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
}
