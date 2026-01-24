//! Session repository for database operations.

use rusqlite::{params, OptionalExtension, Row};
use std::collections::HashMap;

use crate::connection::Database;
use crate::error::{Result, StorageError};
use crate::models::{
    current_timestamp, uuid_v4, CreateSessionParams, ListSessionsParams, Session,
    UpdateSessionParams,
};

/// Repository for session CRUD operations.
pub struct SessionRepository<'a> {
    db: &'a Database,
}

impl<'a> SessionRepository<'a> {
    /// Create a new session repository.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Create a new session.
    pub fn create(&self, params: CreateSessionParams) -> Result<Session> {
        let id = params.id.unwrap_or_else(uuid_v4);
        let now = current_timestamp();
        let mode = params.mode.unwrap_or_default();
        let pinned = params.pinned.unwrap_or(false);
        let metadata_json = params
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, title, mode, created_at, updated_at, pinned, archived, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
                params![
                    id,
                    params.title,
                    mode.as_str(),
                    now,
                    now,
                    pinned as i32,
                    metadata_json,
                ],
            )?;

            Ok(Session {
                id,
                title: params.title,
                mode,
                created_at: now,
                updated_at: now,
                pinned,
                archived: false,
                metadata: params.metadata,
            })
        })
    }

    /// Get a session by ID.
    pub fn get(&self, id: &str) -> Result<Option<Session>> {
        self.db.with_connection(|conn| {
            let result = conn
                .query_row(
                    "SELECT id, title, mode, created_at, updated_at, pinned, archived, metadata
                     FROM sessions WHERE id = ?1",
                    params![id],
                    row_to_session,
                )
                .optional()?;

            match result {
                Some(session_result) => Ok(Some(session_result?)),
                None => Ok(None),
            }
        })
    }

    /// Update an existing session.
    pub fn update(&self, id: &str, params: UpdateSessionParams) -> Result<Session> {
        if !params.has_changes() {
            // No changes, just return the current session
            return self.get(id)?.ok_or_else(|| StorageError::NotFound {
                entity: "session".to_string(),
                id: id.to_string(),
            });
        }

        let now = current_timestamp();

        self.db.with_connection(|conn| {
            // Build dynamic update SQL
            let mut updates = Vec::new();
            let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

            if let Some(ref title) = params.title {
                updates.push("title = ?");
                values.push(Box::new(title.clone()));
            }

            if let Some(mode) = params.mode {
                updates.push("mode = ?");
                values.push(Box::new(mode.as_str().to_string()));
            }

            if let Some(pinned) = params.pinned {
                updates.push("pinned = ?");
                values.push(Box::new(pinned as i32));
            }

            if let Some(archived) = params.archived {
                updates.push("archived = ?");
                values.push(Box::new(archived as i32));
            }

            if let Some(ref metadata) = params.metadata {
                updates.push("metadata = ?");
                let json = metadata.as_ref().map(serde_json::to_string).transpose()?;
                values.push(Box::new(json));
            }

            // Always update the timestamp
            updates.push("updated_at = ?");
            values.push(Box::new(now));

            // Add the ID for the WHERE clause
            values.push(Box::new(id.to_string()));

            let sql = format!("UPDATE sessions SET {} WHERE id = ?", updates.join(", "));

            let params_refs: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|v| v.as_ref()).collect();
            let rows_affected = conn.execute(&sql, params_refs.as_slice())?;

            if rows_affected == 0 {
                return Err(StorageError::NotFound {
                    entity: "session".to_string(),
                    id: id.to_string(),
                });
            }

            // Fetch and return the updated session
            conn.query_row(
                "SELECT id, title, mode, created_at, updated_at, pinned, archived, metadata
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )?
        })
    }

    /// Delete a session by ID.
    pub fn delete(&self, id: &str) -> Result<bool> {
        self.db.with_connection(|conn| {
            let rows_affected = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
            Ok(rows_affected > 0)
        })
    }

    /// List sessions with filtering and pagination.
    pub fn list(&self, params: ListSessionsParams) -> Result<Vec<Session>> {
        self.db.with_connection(|conn| {
            let mut conditions = Vec::new();
            let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

            // Filter archived by default
            if !params.include_archived.unwrap_or(false) {
                conditions.push("archived = 0");
            }

            if let Some(mode) = params.mode {
                conditions.push("mode = ?");
                values.push(Box::new(mode.as_str().to_string()));
            }

            if let Some(pinned) = params.pinned {
                conditions.push("pinned = ?");
                values.push(Box::new(pinned as i32));
            }

            if let Some(ref search) = params.search {
                conditions.push("title LIKE ?");
                values.push(Box::new(format!("%{}%", search)));
            }

            let where_clause = if conditions.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", conditions.join(" AND "))
            };

            let limit_clause = if let Some(limit) = params.limit {
                format!(" LIMIT {}", limit)
            } else {
                String::new()
            };

            let offset_clause = if let Some(offset) = params.offset {
                format!(" OFFSET {}", offset)
            } else {
                String::new()
            };

            let sql = format!(
                "SELECT id, title, mode, created_at, updated_at, pinned, archived, metadata
                 FROM sessions {} ORDER BY updated_at DESC{}{}",
                where_clause, limit_clause, offset_clause
            );

            let params_refs: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|v| v.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;

            let sessions = stmt
                .query_map(params_refs.as_slice(), row_to_session)?
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>>>()?;

            Ok(sessions)
        })
    }

    /// Count total sessions.
    pub fn count(&self, include_archived: bool) -> Result<u32> {
        self.db.with_connection(|conn| {
            let sql = if include_archived {
                "SELECT COUNT(*) FROM sessions"
            } else {
                "SELECT COUNT(*) FROM sessions WHERE archived = 0"
            };

            let count: i64 = conn.query_row(sql, [], |row| row.get(0))?;
            Ok(count as u32)
        })
    }

    /// Update the session's timestamp to now.
    pub fn touch(&self, id: &str) -> Result<()> {
        let now = current_timestamp();

        self.db.with_connection(|conn| {
            let rows_affected = conn.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;

            if rows_affected == 0 {
                return Err(StorageError::NotFound {
                    entity: "session".to_string(),
                    id: id.to_string(),
                });
            }

            Ok(())
        })
    }
}

/// Convert a database row to a Session.
fn row_to_session(row: &Row<'_>) -> rusqlite::Result<Result<Session>> {
    let id: String = row.get(0)?;
    let title: Option<String> = row.get(1)?;
    let mode_str: String = row.get(2)?;
    let created_at: i64 = row.get(3)?;
    let updated_at: i64 = row.get(4)?;
    let pinned: i32 = row.get(5)?;
    let archived: i32 = row.get(6)?;
    let metadata_json: Option<String> = row.get(7)?;

    let mode = mode_str.parse().unwrap_or_default();

    let metadata: Option<HashMap<String, serde_json::Value>> = match metadata_json {
        Some(json) => match serde_json::from_str(&json) {
            Ok(m) => Some(m),
            Err(e) => return Ok(Err(StorageError::Json(e))),
        },
        None => None,
    };

    Ok(Ok(Session {
        id,
        title,
        mode,
        created_at,
        updated_at,
        pinned: pinned != 0,
        archived: archived != 0,
        metadata,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SessionMode;

    fn setup_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn test_create_session_default() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let session = repo.create(CreateSessionParams::new()).unwrap();

        assert!(!session.id.is_empty());
        assert!(session.title.is_none());
        assert_eq!(session.mode, SessionMode::Chat);
        assert!(!session.pinned);
        assert!(!session.archived);
    }

    #[test]
    fn test_create_session_with_params() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let params = CreateSessionParams::new()
            .with_id("custom-id-123")
            .with_title("My Session")
            .with_mode(SessionMode::Agent)
            .with_pinned(true);

        let session = repo.create(params).unwrap();

        assert_eq!(session.id, "custom-id-123");
        assert_eq!(session.title, Some("My Session".to_string()));
        assert_eq!(session.mode, SessionMode::Agent);
        assert!(session.pinned);
    }

    #[test]
    fn test_create_session_with_metadata() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let mut metadata = HashMap::new();
        metadata.insert("key".to_string(), serde_json::json!("value"));
        metadata.insert("number".to_string(), serde_json::json!(42));

        let params = CreateSessionParams::new().with_metadata(metadata.clone());
        let session = repo.create(params).unwrap();

        assert_eq!(session.metadata, Some(metadata));
    }

    #[test]
    fn test_get_session() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let created = repo
            .create(
                CreateSessionParams::new()
                    .with_id("test-get")
                    .with_title("Test"),
            )
            .unwrap();

        let fetched = repo.get("test-get").unwrap();
        assert!(fetched.is_some());

        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.title, created.title);
    }

    #[test]
    fn test_get_session_not_found() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let result = repo.get("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_update_session_title() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("update-test"))
            .unwrap();

        let updated = repo
            .update(
                "update-test",
                UpdateSessionParams::new().with_title("New Title"),
            )
            .unwrap();

        assert_eq!(updated.title, Some("New Title".to_string()));
    }

    #[test]
    fn test_update_session_clear_title() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(
            CreateSessionParams::new()
                .with_id("clear-title-test")
                .with_title("Original"),
        )
        .unwrap();

        let updated = repo
            .update("clear-title-test", UpdateSessionParams::new().clear_title())
            .unwrap();

        assert!(updated.title.is_none());
    }

    #[test]
    fn test_update_session_mode() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("mode-test"))
            .unwrap();

        let updated = repo
            .update(
                "mode-test",
                UpdateSessionParams::new().with_mode(SessionMode::Agent),
            )
            .unwrap();

        assert_eq!(updated.mode, SessionMode::Agent);
    }

    #[test]
    fn test_update_session_pinned() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("pin-test"))
            .unwrap();

        let updated = repo
            .update("pin-test", UpdateSessionParams::new().with_pinned(true))
            .unwrap();

        assert!(updated.pinned);
    }

    #[test]
    fn test_update_session_archived() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("archive-test"))
            .unwrap();

        let updated = repo
            .update(
                "archive-test",
                UpdateSessionParams::new().with_archived(true),
            )
            .unwrap();

        assert!(updated.archived);
    }

    #[test]
    fn test_update_session_not_found() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let result = repo.update("nonexistent", UpdateSessionParams::new().with_title("Test"));

        assert!(matches!(result, Err(StorageError::NotFound { .. })));
    }

    #[test]
    fn test_update_no_changes() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let created = repo
            .create(CreateSessionParams::new().with_id("no-change-test"))
            .unwrap();

        let updated = repo
            .update("no-change-test", UpdateSessionParams::new())
            .unwrap();

        assert_eq!(updated.id, created.id);
    }

    #[test]
    fn test_delete_session() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("delete-test"))
            .unwrap();

        let deleted = repo.delete("delete-test").unwrap();
        assert!(deleted);

        let fetched = repo.get("delete-test").unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn test_delete_session_not_found() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let deleted = repo.delete("nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn test_list_sessions_default() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("list-1"))
            .unwrap();
        repo.create(CreateSessionParams::new().with_id("list-2"))
            .unwrap();

        let sessions = repo.list(ListSessionsParams::new()).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_list_sessions_excludes_archived() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("active"))
            .unwrap();
        repo.create(CreateSessionParams::new().with_id("archived"))
            .unwrap();
        repo.update("archived", UpdateSessionParams::new().with_archived(true))
            .unwrap();

        let sessions = repo.list(ListSessionsParams::new()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "active");

        let all_sessions = repo
            .list(ListSessionsParams::new().include_archived(true))
            .unwrap();
        assert_eq!(all_sessions.len(), 2);
    }

    #[test]
    fn test_list_sessions_filter_by_mode() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("chat-session"))
            .unwrap();
        repo.create(
            CreateSessionParams::new()
                .with_id("agent-session")
                .with_mode(SessionMode::Agent),
        )
        .unwrap();

        let chat_sessions = repo
            .list(ListSessionsParams::new().with_mode(SessionMode::Chat))
            .unwrap();
        assert_eq!(chat_sessions.len(), 1);
        assert_eq!(chat_sessions[0].id, "chat-session");

        let agent_sessions = repo
            .list(ListSessionsParams::new().with_mode(SessionMode::Agent))
            .unwrap();
        assert_eq!(agent_sessions.len(), 1);
        assert_eq!(agent_sessions[0].id, "agent-session");
    }

    #[test]
    fn test_list_sessions_filter_by_pinned() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("unpinned"))
            .unwrap();
        repo.create(
            CreateSessionParams::new()
                .with_id("pinned")
                .with_pinned(true),
        )
        .unwrap();

        let pinned_sessions = repo
            .list(ListSessionsParams::new().with_pinned(true))
            .unwrap();
        assert_eq!(pinned_sessions.len(), 1);
        assert_eq!(pinned_sessions[0].id, "pinned");
    }

    #[test]
    fn test_list_sessions_pagination() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        for i in 0..5 {
            repo.create(CreateSessionParams::new().with_id(format!("page-{}", i)))
                .unwrap();
            // Small delay to ensure different timestamps
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let first_page = repo.list(ListSessionsParams::new().with_limit(2)).unwrap();
        assert_eq!(first_page.len(), 2);

        let second_page = repo
            .list(ListSessionsParams::new().with_limit(2).with_offset(2))
            .unwrap();
        assert_eq!(second_page.len(), 2);

        let third_page = repo
            .list(ListSessionsParams::new().with_limit(2).with_offset(4))
            .unwrap();
        assert_eq!(third_page.len(), 1);
    }

    #[test]
    fn test_list_sessions_search() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(
            CreateSessionParams::new()
                .with_id("s1")
                .with_title("Project Alpha"),
        )
        .unwrap();
        repo.create(
            CreateSessionParams::new()
                .with_id("s2")
                .with_title("Project Beta"),
        )
        .unwrap();
        repo.create(
            CreateSessionParams::new()
                .with_id("s3")
                .with_title("Something Else"),
        )
        .unwrap();

        let results = repo
            .list(ListSessionsParams::new().with_search("Project"))
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_list_sessions_ordered_by_updated_at() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("old"))
            .unwrap();
        repo.create(CreateSessionParams::new().with_id("new"))
            .unwrap();

        // Since timestamps are in seconds, we need to wait at least 1 second
        // and touch the "new" session to give it a later timestamp
        std::thread::sleep(std::time::Duration::from_secs(1));
        repo.touch("new").unwrap();

        let sessions = repo.list(ListSessionsParams::new()).unwrap();
        assert_eq!(sessions.len(), 2);
        // Most recent first
        assert_eq!(sessions[0].id, "new");
        assert_eq!(sessions[1].id, "old");
    }

    #[test]
    fn test_count_sessions() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        assert_eq!(repo.count(false).unwrap(), 0);

        repo.create(CreateSessionParams::new().with_id("c1"))
            .unwrap();
        repo.create(CreateSessionParams::new().with_id("c2"))
            .unwrap();

        assert_eq!(repo.count(false).unwrap(), 2);
    }

    #[test]
    fn test_count_excludes_archived() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        repo.create(CreateSessionParams::new().with_id("active"))
            .unwrap();
        repo.create(CreateSessionParams::new().with_id("archived"))
            .unwrap();
        repo.update("archived", UpdateSessionParams::new().with_archived(true))
            .unwrap();

        assert_eq!(repo.count(false).unwrap(), 1);
        assert_eq!(repo.count(true).unwrap(), 2);
    }

    #[test]
    fn test_touch_session() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let created = repo
            .create(CreateSessionParams::new().with_id("touch-test"))
            .unwrap();
        let original_updated = created.updated_at;

        // Wait at least 1 second since timestamps are in seconds
        std::thread::sleep(std::time::Duration::from_secs(1));
        repo.touch("touch-test").unwrap();

        let fetched = repo.get("touch-test").unwrap().unwrap();
        assert!(fetched.updated_at > original_updated);
    }

    #[test]
    fn test_touch_session_not_found() {
        let db = setup_db();
        let repo = SessionRepository::new(&db);

        let result = repo.touch("nonexistent");
        assert!(matches!(result, Err(StorageError::NotFound { .. })));
    }
}
