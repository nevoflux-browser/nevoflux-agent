//! Database migrations for the storage layer.

use rusqlite::Connection;

use crate::error::{Result, StorageError};

const MIGRATIONS: &[(&str, &str)] = &[
    ("001_initial", include_str!("migrations/001_initial.sql")),
    ("002_memory", include_str!("migrations/002_memory.sql")),
    ("003_traces", include_str!("migrations/003_traces.sql")),
    (
        "004_artifacts",
        include_str!("migrations/004_artifacts.sql"),
    ),
    ("005_learning", include_str!("migrations/005_learning.sql")),
    (
        "006_knowledge_embedding",
        include_str!("migrations/006_knowledge_embedding.sql"),
    ),
    (
        "007_knowledge_hot",
        include_str!("migrations/007_knowledge_hot.sql"),
    ),
    (
        "008_message_index",
        include_str!("migrations/008_message_index.sql"),
    ),
    (
        "009_fts_trigram",
        include_str!("migrations/009_fts_trigram.sql"),
    ),
    (
        "010_event_bus",
        include_str!("migrations/010_event_bus.sql"),
    ),
    (
        "011_canvas_tool_audit",
        include_str!("migrations/011_canvas_tool_audit.sql"),
    ),
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
        assert_eq!(count, 11);
    }

    #[test]
    fn test_trace_spans_table_created() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_all(&mut conn).unwrap();

        conn.execute(
            "INSERT INTO trace_spans (session_id, iteration, span_type, success)
             VALUES ('sess-1', 0, 'tool_exec', 0)",
            [],
        )
        .unwrap();

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
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trace_spans (session_id, iteration, span_type, success)
             VALUES ('sess-2', 0, 'tool_exec', 0)",
            [],
        )
        .unwrap();

        conn.execute("DELETE FROM trace_spans WHERE session_id = 'sess-1'", [])
            .unwrap();

        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM trace_spans", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn migration_005_creates_learning_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_all(&mut conn).unwrap();

        // Verify tables exist
        for table in &[
            "knowledge",
            "site_adaptations",
            "tool_stats",
            "learning_metrics",
        ] {
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{}'",
                        table
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "Table {} should exist", table);
        }

        // Verify view exists
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='knowledge_health'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "View knowledge_health should exist");
    }

    #[test]
    fn migration_010_creates_event_bus_persistent_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_all(&mut conn).unwrap();

        // Verify table exists
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='event_bus_persistent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "Table event_bus_persistent should exist");

        // Verify indexes exist
        for idx in &[
            "idx_ebp_topic",
            "idx_ebp_expires_at",
            "idx_ebp_created_at",
        ] {
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='{}'",
                        idx
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "Index {} should exist", idx);
        }

        // Verify we can INSERT and SELECT
        conn.execute(
            "INSERT INTO event_bus_persistent (id, topic, payload, publisher_kind, publisher_id, created_at, expires_at)
             VALUES ('evt-001', 'task:status', '{\"status\":\"done\"}', 'agent', 'agent-1', strftime('%s','now'), strftime('%s','now') + 3600)",
            [],
        )
        .unwrap();

        let payload: String = conn
            .query_row(
                "SELECT payload FROM event_bus_persistent WHERE id = 'evt-001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(payload.contains("done"));
    }

    #[test]
    fn migration_011_creates_canvas_tool_invocations_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_all(&mut conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='canvas_tool_invocations'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "Table canvas_tool_invocations should exist");

        // Verify INSERT works
        conn.execute(
            "INSERT INTO canvas_tool_invocations (id, session_id, tool_name, backend_kind, args_json, status, started_at)
             VALUES ('inv-001', 'sess-001', 'ffmpeg.trim', 'command', '[]', 'started', strftime('%s','now'))",
            [],
        )
        .unwrap();

        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM canvas_tool_invocations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(row_count, 1);
    }

    #[test]
    fn migration_011_indexes() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_all(&mut conn).unwrap();

        for idx in &[
            "idx_canvas_invocations_session",
            "idx_canvas_invocations_tool",
        ] {
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='{}'",
                        idx
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "Index {} should exist", idx);
        }
    }
}
