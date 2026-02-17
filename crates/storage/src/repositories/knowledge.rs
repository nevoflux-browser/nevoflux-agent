//! Knowledge repository for database operations.

use rusqlite::{params, OptionalExtension, Row};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::connection::Database;
use crate::error::Result;
use crate::models::knowledge::{CreateKnowledgeParams, Knowledge};

/// Repository for knowledge CRUD operations.
pub struct KnowledgeRepository<'a> {
    db: &'a Database,
}

impl<'a> KnowledgeRepository<'a> {
    /// Create a new knowledge repository.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Create a new knowledge entry.
    pub fn create(&self, params: CreateKnowledgeParams) -> Result<Knowledge> {
        let id = generate_knowledge_id();
        let now = rfc3339_now();
        let priority = params.priority.unwrap_or_else(|| "medium".to_string());
        let privacy_level = params.privacy_level.unwrap_or_else(|| "internal".to_string());
        let source_type = params.source_type.unwrap_or_else(|| "system".to_string());

        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO knowledge (
                    id, category, subcategory, domain, summary, details,
                    resolution, confidence, hit_count, success_count, fail_count,
                    priority, status, source_ids, related_ids, tags,
                    privacy_level, promotion_target, promoted_section,
                    source_type, created_at, updated_at, last_hit_at, promoted_at
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6,
                    ?7, 0.5, 1, 0, 0,
                    ?8, 'pending', ?9, NULL, ?10,
                    ?11, ?12, NULL,
                    ?13, ?14, ?15, NULL, NULL
                )",
                params![
                    id,
                    params.category,
                    params.subcategory,
                    params.domain,
                    params.summary,
                    params.details,
                    params.resolution,
                    priority,
                    params.source_ids,
                    params.tags,
                    privacy_level,
                    params.promotion_target,
                    source_type,
                    now,
                    now,
                ],
            )?;

            // Read back to get the computed effectiveness column
            Ok(conn.query_row(
                "SELECT id, category, subcategory, domain, summary, details,
                        resolution, confidence, hit_count, success_count, fail_count,
                        effectiveness, priority, status, source_ids, related_ids, tags,
                        privacy_level, promotion_target, promoted_section,
                        source_type, created_at, updated_at, last_hit_at, promoted_at
                 FROM knowledge WHERE id = ?1",
                params![id],
                row_to_knowledge,
            )?)
        })
    }

    /// Get a knowledge entry by ID.
    pub fn get(&self, id: &str) -> Result<Option<Knowledge>> {
        self.db.with_connection(|conn| {
            let result = conn
                .query_row(
                    "SELECT id, category, subcategory, domain, summary, details,
                            resolution, confidence, hit_count, success_count, fail_count,
                            effectiveness, priority, status, source_ids, related_ids, tags,
                            privacy_level, promotion_target, promoted_section,
                            source_type, created_at, updated_at, last_hit_at, promoted_at
                     FROM knowledge WHERE id = ?1",
                    params![id],
                    row_to_knowledge,
                )
                .optional()?;

            match result {
                Some(k) => Ok(Some(k)),
                None => Ok(None),
            }
        })
    }

    /// Update the status of a knowledge entry.
    pub fn update_status(&self, id: &str, status: &str) -> Result<()> {
        let now = rfc3339_now();

        self.db.with_connection(|conn| {
            let rows_affected = conn.execute(
                "UPDATE knowledge SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, id],
            )?;

            if rows_affected == 0 {
                return Err(crate::error::StorageError::NotFound {
                    entity: "knowledge".to_string(),
                    id: id.to_string(),
                });
            }

            Ok(())
        })
    }

    /// Query knowledge entries by domain, limited to a maximum number of results.
    pub fn query_by_domain(&self, domain: &str, limit: u32) -> Result<Vec<Knowledge>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, category, subcategory, domain, summary, details,
                        resolution, confidence, hit_count, success_count, fail_count,
                        effectiveness, priority, status, source_ids, related_ids, tags,
                        privacy_level, promotion_target, promoted_section,
                        source_type, created_at, updated_at, last_hit_at, promoted_at
                 FROM knowledge WHERE domain = ?1
                 ORDER BY confidence DESC, updated_at DESC
                 LIMIT ?2",
            )?;

            let rows = stmt
                .query_map(params![domain, limit], row_to_knowledge)?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            Ok(rows)
        })
    }

    /// Delete a knowledge entry by ID.
    pub fn delete(&self, id: &str) -> Result<bool> {
        self.db.with_connection(|conn| {
            let rows_affected =
                conn.execute("DELETE FROM knowledge WHERE id = ?1", params![id])?;
            Ok(rows_affected > 0)
        })
    }
}

/// Convert a database row to a Knowledge struct.
fn row_to_knowledge(row: &Row<'_>) -> rusqlite::Result<Knowledge> {
    Ok(Knowledge {
        id: row.get(0)?,
        category: row.get(1)?,
        subcategory: row.get(2)?,
        domain: row.get(3)?,
        summary: row.get(4)?,
        details: row.get(5)?,
        resolution: row.get(6)?,
        confidence: row.get(7)?,
        hit_count: row.get(8)?,
        success_count: row.get(9)?,
        fail_count: row.get(10)?,
        effectiveness: row.get(11)?,
        priority: row.get(12)?,
        status: row.get(13)?,
        source_ids: row.get(14)?,
        related_ids: row.get(15)?,
        tags: row.get(16)?,
        privacy_level: row.get(17)?,
        promotion_target: row.get(18)?,
        promoted_section: row.get(19)?,
        source_type: row.get(20)?,
        created_at: row.get(21)?,
        updated_at: row.get(22)?,
        last_hit_at: row.get(23)?,
        promoted_at: row.get(24)?,
    })
}

/// Generate a knowledge ID in the format K-{YYYYMMDD}-{6hex}.
fn generate_knowledge_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();

    // Convert Unix timestamp to date components
    // Days since epoch
    let days = secs / 86400;
    // Algorithm to convert days since epoch to Y-M-D
    let (year, month, day) = days_to_date(days);

    // Generate 6 hex chars from nanoseconds + wrapping multiply for uniqueness
    let nanos = now.as_nanos();
    let random_part = (nanos as u64).wrapping_mul(6364136223846793005);

    format!("K-{:04}{:02}{:02}-{:06x}", year, month, day, random_part & 0xFFFFFF)
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_date(days_since_epoch: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm from Howard Hinnant
    let z = days_since_epoch + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Get the current time as an RFC3339 string.
fn rfc3339_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let (year, month, day) = days_to_date(days);
    let rem = secs % 86400;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, minute, second
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;

    #[test]
    fn test_generate_knowledge_id_format() {
        let id = generate_knowledge_id();
        assert!(id.starts_with("K-"));
        // K-YYYYMMDD-6hex = 2 + 8 + 1 + 6 = 17 chars
        assert_eq!(id.len(), 17, "ID should be 17 chars: {}", id);
    }

    #[test]
    fn test_rfc3339_now_format() {
        let ts = rfc3339_now();
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
        assert_eq!(ts.len(), 20);
    }

    #[test]
    fn knowledge_crud_lifecycle() {
        let storage = Storage::open_in_memory().unwrap();

        // Create
        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "site_interaction".into(),
                subcategory: Some("selector_result".into()),
                domain: Some("github.com".into()),
                summary: "github.com uses data-testid selectors".into(),
                details: "Verified across 10 pages".into(),
                ..Default::default()
            })
            .unwrap();

        assert!(created.id.starts_with("K-"));
        assert_eq!(created.status, "pending");
        assert_eq!(created.confidence, 0.5);

        // Read
        let found = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert_eq!(found.summary, created.summary);

        // Update status
        storage
            .knowledge()
            .update_status(&created.id, "validated")
            .unwrap();
        let updated = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert_eq!(updated.status, "validated");

        // Query by domain
        let results = storage
            .knowledge()
            .query_by_domain("github.com", 5)
            .unwrap();
        assert_eq!(results.len(), 1);

        // Delete
        storage.knowledge().delete(&created.id).unwrap();
        assert!(storage.knowledge().get(&created.id).unwrap().is_none());
    }

    #[test]
    fn test_get_nonexistent() {
        let storage = Storage::open_in_memory().unwrap();
        let result = storage.knowledge().get("K-00000000-000000").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_update_status_not_found() {
        let storage = Storage::open_in_memory().unwrap();
        let result = storage
            .knowledge()
            .update_status("K-00000000-000000", "validated");
        assert!(result.is_err());
    }

    #[test]
    fn test_query_by_domain_empty() {
        let storage = Storage::open_in_memory().unwrap();
        let results = storage
            .knowledge()
            .query_by_domain("nonexistent.com", 10)
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_delete_returns_false_for_nonexistent() {
        let storage = Storage::open_in_memory().unwrap();
        let deleted = storage
            .knowledge()
            .delete("K-00000000-000000")
            .unwrap();
        assert!(!deleted);
    }

    #[test]
    fn test_effectiveness_computation() {
        let storage = Storage::open_in_memory().unwrap();

        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "test".into(),
                summary: "test effectiveness".into(),
                details: "details".into(),
                ..Default::default()
            })
            .unwrap();

        // Default: 0 success + 0 fail => effectiveness = 0.5
        assert_eq!(created.effectiveness, 0.5);

        // Manually update counts to test the GENERATED ALWAYS column
        storage.database().with_connection(|conn| {
            conn.execute(
                "UPDATE knowledge SET success_count = 8, fail_count = 2 WHERE id = ?1",
                params![created.id],
            )?;
            Ok(())
        }).unwrap();

        let updated = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert!((updated.effectiveness - 0.8).abs() < 0.001);
    }
}
