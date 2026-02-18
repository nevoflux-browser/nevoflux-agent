//! Repository for artifact persistence.

use rusqlite::params;

use crate::connection::Database;
use crate::error::Result;
use crate::models::artifact::{ArtifactRecord, CreateArtifactParams};

/// Repository for artifact CRUD operations.
pub struct ArtifactRepository<'a> {
    db: &'a Database,
}

impl<'a> ArtifactRepository<'a> {
    /// Create a new artifact repository.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Create or replace an artifact.
    pub fn create(&self, params: CreateArtifactParams) -> Result<ArtifactRecord> {
        let files_json = params
            .files
            .as_ref()
            .map(|f| serde_json::to_string(f).unwrap_or_default());

        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO artifacts (id, session_id, title, description, content_type, content, files, entry)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    params.id,
                    params.session_id,
                    params.title,
                    params.description,
                    params.content_type,
                    params.content,
                    files_json,
                    params.entry,
                ],
            )?;

            let created_at: i64 = conn.query_row(
                "SELECT created_at FROM artifacts WHERE id = ?1",
                params![params.id],
                |row| row.get(0),
            )?;

            Ok(ArtifactRecord {
                id: params.id,
                session_id: params.session_id,
                title: params.title,
                description: params.description,
                content_type: params.content_type,
                content: params.content,
                files: params.files,
                entry: params.entry,
                created_at,
            })
        })
    }

    /// Get a full artifact by ID.
    pub fn get(&self, id: &str) -> Result<Option<ArtifactRecord>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, title, description, content_type, content, files, entry, created_at
                 FROM artifacts WHERE id = ?1",
            )?;

            let mut rows = stmt.query_map(params![id], row_to_artifact)?;

            match rows.next() {
                Some(row) => Ok(Some(row?)),
                None => Ok(None),
            }
        })
    }

    /// List artifacts for a session (summaries only — empty content, no files).
    pub fn list_by_session(&self, session_id: &str) -> Result<Vec<ArtifactRecord>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, title, description, content_type, created_at
                 FROM artifacts WHERE session_id = ?1 ORDER BY created_at ASC",
            )?;

            let rows = stmt
                .query_map(params![session_id], |row| {
                    Ok(ArtifactRecord {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        title: row.get(2)?,
                        description: row.get(3)?,
                        content_type: row.get(4)?,
                        content: String::new(),
                        files: None,
                        entry: None,
                        created_at: row.get(5)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            Ok(rows)
        })
    }

    /// Delete all artifacts for a session.
    pub fn delete_by_session(&self, session_id: &str) -> Result<u32> {
        self.db.with_connection(|conn| {
            let rows_affected = conn.execute(
                "DELETE FROM artifacts WHERE session_id = ?1",
                params![session_id],
            )?;
            Ok(rows_affected as u32)
        })
    }
}

/// Convert a full database row to an ArtifactRecord.
fn row_to_artifact(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRecord> {
    let id: String = row.get(0)?;
    let session_id: String = row.get(1)?;
    let title: String = row.get(2)?;
    let description: Option<String> = row.get(3)?;
    let content_type: String = row.get(4)?;
    let content: String = row.get(5)?;
    let files_json: Option<String> = row.get(6)?;
    let entry: Option<String> = row.get(7)?;
    let created_at: i64 = row.get(8)?;

    let files = files_json.and_then(|j| serde_json::from_str(&j).ok());

    Ok(ArtifactRecord {
        id,
        session_id,
        title,
        description,
        content_type,
        content,
        files,
        entry,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;
    use std::collections::HashMap;

    #[test]
    fn test_create_and_get_artifact() {
        let storage = Storage::open_in_memory().unwrap();
        let repo = ArtifactRepository::new(storage.database());

        // Create a session first (artifacts reference sessions)
        storage
            .sessions()
            .create(crate::CreateSessionParams::new().with_id("sess-1"))
            .unwrap();

        let params = CreateArtifactParams::new("art-1", "sess-1", "My Page", "text/html")
            .with_description("A simple page")
            .with_content("<h1>Hello</h1>");

        let record = repo.create(params).unwrap();
        assert_eq!(record.id, "art-1");
        assert_eq!(record.title, "My Page");
        assert_eq!(record.description, Some("A simple page".to_string()));
        assert_eq!(record.content, "<h1>Hello</h1>");

        // Get it back
        let fetched = repo.get("art-1").unwrap().unwrap();
        assert_eq!(fetched.id, "art-1");
        assert_eq!(fetched.content, "<h1>Hello</h1>");
        assert_eq!(fetched.description, Some("A simple page".to_string()));
    }

    #[test]
    fn test_get_nonexistent_artifact() {
        let storage = Storage::open_in_memory().unwrap();
        let repo = ArtifactRepository::new(storage.database());

        let result = repo.get("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_create_artifact_with_files() {
        let storage = Storage::open_in_memory().unwrap();
        let repo = ArtifactRepository::new(storage.database());

        storage
            .sessions()
            .create(crate::CreateSessionParams::new().with_id("sess-1"))
            .unwrap();

        let mut files = HashMap::new();
        files.insert("src/App.jsx".to_string(), "export default App;".to_string());
        files.insert(
            "src/index.jsx".to_string(),
            "import App from './App';".to_string(),
        );

        let params = CreateArtifactParams::new("art-2", "sess-1", "React App", "project")
            .with_files(files.clone())
            .with_entry("src/index.jsx");

        let record = repo.create(params).unwrap();
        assert_eq!(record.files.as_ref().unwrap().len(), 2);
        assert_eq!(record.entry, Some("src/index.jsx".to_string()));

        // Verify files survive round-trip
        let fetched = repo.get("art-2").unwrap().unwrap();
        let fetched_files = fetched.files.unwrap();
        assert_eq!(fetched_files.len(), 2);
        assert_eq!(fetched_files["src/App.jsx"], "export default App;");
    }

    #[test]
    fn test_list_by_session() {
        let storage = Storage::open_in_memory().unwrap();
        let repo = ArtifactRepository::new(storage.database());

        storage
            .sessions()
            .create(crate::CreateSessionParams::new().with_id("sess-1"))
            .unwrap();
        storage
            .sessions()
            .create(crate::CreateSessionParams::new().with_id("sess-2"))
            .unwrap();

        repo.create(
            CreateArtifactParams::new("art-1", "sess-1", "Page 1", "text/html")
                .with_content("<h1>1</h1>"),
        )
        .unwrap();
        repo.create(
            CreateArtifactParams::new("art-2", "sess-1", "Page 2", "text/html")
                .with_content("<h1>2</h1>"),
        )
        .unwrap();
        repo.create(
            CreateArtifactParams::new("art-3", "sess-2", "Other", "text/html")
                .with_content("<h1>3</h1>"),
        )
        .unwrap();

        let list = repo.list_by_session("sess-1").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "art-1");
        assert_eq!(list[1].id, "art-2");
        // Summaries: content should be empty, files should be None
        assert!(list[0].content.is_empty());
        assert!(list[0].files.is_none());

        let list2 = repo.list_by_session("sess-2").unwrap();
        assert_eq!(list2.len(), 1);
    }

    #[test]
    fn test_delete_by_session() {
        let storage = Storage::open_in_memory().unwrap();
        let repo = ArtifactRepository::new(storage.database());

        storage
            .sessions()
            .create(crate::CreateSessionParams::new().with_id("sess-1"))
            .unwrap();

        repo.create(CreateArtifactParams::new(
            "art-1",
            "sess-1",
            "Page 1",
            "text/html",
        ))
        .unwrap();
        repo.create(CreateArtifactParams::new(
            "art-2",
            "sess-1",
            "Page 2",
            "text/html",
        ))
        .unwrap();

        let deleted = repo.delete_by_session("sess-1").unwrap();
        assert_eq!(deleted, 2);

        let list = repo.list_by_session("sess-1").unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_upsert_artifact() {
        let storage = Storage::open_in_memory().unwrap();
        let repo = ArtifactRepository::new(storage.database());

        storage
            .sessions()
            .create(crate::CreateSessionParams::new().with_id("sess-1"))
            .unwrap();

        repo.create(
            CreateArtifactParams::new("art-1", "sess-1", "V1", "text/html")
                .with_content("<h1>V1</h1>"),
        )
        .unwrap();

        // Upsert with new content
        repo.create(
            CreateArtifactParams::new("art-1", "sess-1", "V2", "text/html")
                .with_content("<h1>V2</h1>"),
        )
        .unwrap();

        let fetched = repo.get("art-1").unwrap().unwrap();
        assert_eq!(fetched.title, "V2");
        assert_eq!(fetched.content, "<h1>V2</h1>");

        // Should still be only one artifact
        let list = repo.list_by_session("sess-1").unwrap();
        assert_eq!(list.len(), 1);
    }
}
