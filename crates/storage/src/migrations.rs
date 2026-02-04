//! Database migrations for the storage layer.

use rusqlite::Connection;

use crate::error::{Result, StorageError};

const MIGRATIONS: &[(&str, &str)] = &[
    ("001_initial", include_str!("migrations/001_initial.sql")),
    ("002_memory", include_str!("migrations/002_memory.sql")),
    ("003_traces", include_str!("migrations/003_traces.sql")),
];

/// Run all pending migrations on the given connection.
pub fn run_all(conn: &mut Connection) -> Result<()> {
    // Create migrations tracking table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS _migrations (
            name TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )",
        [],
    )?;

    // Run each migration if not already applied
    for (name, sql) in MIGRATIONS {
        let already_applied: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM _migrations WHERE name = ?)",
            [name],
            |row| row.get(0),
        )?;

        if !already_applied {
            conn.execute_batch(sql).map_err(|e| {
                StorageError::Migration(format!("Migration {} failed: {}", name, e))
            })?;

            conn.execute(
                "INSERT INTO _migrations (name, applied_at) VALUES (?, strftime('%s', 'now'))",
                [name],
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrations_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();

        // Run migrations twice
        run_all(&mut conn).unwrap();
        run_all(&mut conn).unwrap();

        // Should still work
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM _migrations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn test_trace_spans_table_created() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_all(&mut conn).unwrap();

        conn.execute(
            "INSERT INTO trace_spans (session_id, iteration, span_type, success)
             VALUES ('sess-1', 0, 'tool_exec', 0)",
            [],
        ).unwrap();

        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM trace_spans", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_trace_spans_cleanup_by_session() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_all(&mut conn).unwrap();

        conn.execute(
            "INSERT INTO trace_spans (session_id, iteration, span_type, success)
             VALUES ('sess-1', 0, 'llm_call', 1)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO trace_spans (session_id, iteration, span_type, success)
             VALUES ('sess-2', 0, 'tool_exec', 0)",
            [],
        ).unwrap();

        conn.execute("DELETE FROM trace_spans WHERE session_id = 'sess-1'", []).unwrap();

        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM trace_spans", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
