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
}
