//! Per-call LLM and Jev usage rows (design v1.6 §9 M4, minimal).

use rusqlite::params;

use crate::connection::Database;
use crate::error::Result;

/// One LLM or Jev call. `role` is `main`, `subagent` or `jev`; cache fields
/// are `None` when the provider did not report them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmCallRow {
    /// Unix epoch milliseconds.
    pub ts: i64,
    pub session_id: String,
    pub role: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    /// The token counts were estimated, not reported by the provider.
    pub estimated: bool,
}

/// Append-only access to `llm_calls`.
pub struct LlmCallRepository<'a> {
    db: &'a Database,
}

impl<'a> LlmCallRepository<'a> {
    /// Create a new repository over the given database handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Append one row.
    pub fn append(&self, row: &LlmCallRow) -> Result<()> {
        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO llm_calls
                 (ts, session_id, role, model, input, output, cache_read, cache_write, estimated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.ts,
                    row.session_id,
                    row.role,
                    row.model,
                    row.input as i64,
                    row.output as i64,
                    row.cache_read.map(|v| v as i64),
                    row.cache_write.map(|v| v as i64),
                    row.estimated,
                ],
            )?;
            Ok(())
        })
    }

    /// Rows with `ts >= since_ms`, oldest first.
    pub fn list_since(&self, since_ms: i64) -> Result<Vec<LlmCallRow>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ts, session_id, role, model, input, output, cache_read, cache_write, estimated
                 FROM llm_calls WHERE ts >= ?1 ORDER BY id ASC",
            )?;
            let rows = stmt.query_map(params![since_ms], |r| {
                Ok(LlmCallRow {
                    ts: r.get(0)?,
                    session_id: r.get(1)?,
                    role: r.get(2)?,
                    model: r.get(3)?,
                    input: r.get::<_, i64>(4)?.max(0) as u64,
                    output: r.get::<_, i64>(5)?.max(0) as u64,
                    cache_read: r.get::<_, Option<i64>>(6)?.map(|v| v.max(0) as u64),
                    cache_write: r.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u64),
                    estimated: r.get(8)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts: i64, role: &str, input: u64) -> LlmCallRow {
        LlmCallRow {
            ts,
            session_id: "s1".into(),
            role: role.into(),
            model: "k3".into(),
            input,
            output: 7,
            cache_read: Some(input / 2),
            cache_write: None,
            estimated: false,
        }
    }

    #[test]
    fn append_then_list_since() {
        let db = Database::open_in_memory().unwrap();
        let repo = LlmCallRepository::new(&db);
        repo.append(&row(100, "main", 10)).unwrap();
        repo.append(&row(200, "jev", 30)).unwrap();
        repo.append(&row(300, "subagent", 20)).unwrap();
        let rows = repo.list_since(200).unwrap();
        assert_eq!(rows, vec![row(200, "jev", 30), row(300, "subagent", 20)]);
    }
}
