//! CanvasPersistService — persistence management for My Canvas artifacts.
//!
//! Provides listing of artifacts that have been marked `is_persistent = 1`
//! in the local SQLite database. Additional CRUD methods (save, rename, delete)
//! will be added by subsequent tasks.

use std::sync::Arc;

use nevoflux_protocol::canvas_persist::{
    CanvasPersistListRequest, CanvasPersistListResponse, CanvasPersistSortKey, CanvasPersistSource,
    CanvasPersistSourceFilter, CanvasPersistSummary,
};
use nevoflux_storage::Storage;
use rusqlite::types::ToSql;

use crate::error::Result;

/// Service for managing persistent canvas artifacts ("My Canvas").
pub struct CanvasPersistService {
    storage: Arc<Storage>,
}

impl CanvasPersistService {
    /// Create a new `CanvasPersistService` backed by the given storage handle.
    pub fn new(storage: Arc<Storage>) -> Self {
        Self { storage }
    }

    /// List persistent canvas artifacts matching the given filters.
    ///
    /// Always filters to `is_persistent = 1`. Applies optional `search`,
    /// `type_filter`, and `source_filter` predicates. Returns paginated items
    /// and a total count using the same WHERE clause (without LIMIT/OFFSET).
    pub fn list(&self, req: CanvasPersistListRequest) -> Result<CanvasPersistListResponse> {
        // Clamp limit: default 50, max 500, minimum 1.
        let limit = req.limit.unwrap_or(50).clamp(1, 500) as i64;
        let offset = req.offset.unwrap_or(0) as i64;

        // Build the WHERE clause dynamically. All user-supplied values are
        // bound as parameters — never interpolated directly into SQL.
        let mut where_clauses: Vec<&'static str> = vec!["is_persistent = 1"];

        // Build filter bind params. We use String values that outlive the
        // SQL execution. Source filter adds a static predicate with no param.
        let search_pattern: Option<String> = req.search.as_deref().map(|s| format!("%{}%", s));
        let type_filter: Option<String> = req.type_filter.clone();

        if search_pattern.is_some() {
            where_clauses.push("title LIKE ?");
        }
        if type_filter.is_some() {
            where_clauses.push("content_type = ?");
        }
        match &req.source_filter {
            Some(CanvasPersistSourceFilter::Created) => {
                where_clauses.push("imported_from_share_id IS NULL");
            }
            Some(CanvasPersistSourceFilter::Imported) => {
                where_clauses.push("imported_from_share_id IS NOT NULL");
            }
            None => {}
        }

        let where_sql = where_clauses.join(" AND ");

        let order_sql = match req.sort {
            CanvasPersistSortKey::PersistedAt => "ORDER BY persisted_at DESC",
            CanvasPersistSortKey::UpdatedAt => "ORDER BY updated_at DESC",
        };

        // Build the WHERE-only param list (shared by COUNT and SELECT).
        // Source filter predicates are static SQL — they add no bind params.
        let mut where_params: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(ref s) = search_pattern {
            where_params.push(Box::new(s.clone()));
        }
        if let Some(ref t) = type_filter {
            where_params.push(Box::new(t.clone()));
        }

        let count_sql = format!("SELECT COUNT(*) FROM artifacts WHERE {}", where_sql);
        let select_sql = format!(
            "SELECT id, title, content_type, imported_from_share_id, \
                    persisted_at, updated_at, session_id \
             FROM artifacts WHERE {} {} LIMIT ? OFFSET ?",
            where_sql, order_sql
        );

        let (total, items) = self.storage.database().with_connection(|conn| {
            // --- COUNT query (same WHERE, no LIMIT/OFFSET) ---
            let total: u32 = {
                let mut stmt = conn.prepare(&count_sql)?;
                // Bind WHERE params.
                for (i, p) in where_params.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 1, p.as_ref())?;
                }
                // Collect into a local variable so `stmt` can be dropped.
                let count_val = stmt
                    .raw_query()
                    .next()?
                    .and_then(|row| row.get::<_, i64>(0).ok())
                    .unwrap_or(0) as u32;
                count_val
            };

            // --- SELECT query (same WHERE + ORDER BY + LIMIT/OFFSET) ---
            let items: Vec<CanvasPersistSummary> = {
                let mut stmt = conn.prepare(&select_sql)?;
                // Bind WHERE params first.
                let where_param_count = where_params.len();
                for (i, p) in where_params.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 1, p.as_ref())?;
                }
                // Bind LIMIT and OFFSET after WHERE params.
                stmt.raw_bind_parameter(where_param_count + 1, limit)?;
                stmt.raw_bind_parameter(where_param_count + 2, offset)?;

                let mut rows = stmt.raw_query();
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    let id: String = row.get(0)?;
                    let title: String = row.get(1)?;
                    let content_type: String = row.get(2)?;
                    let imported_from_share_id: Option<String> = row.get(3)?;
                    let persisted_at_raw: Option<i64> = row.get(4)?;
                    let updated_at: i64 = row.get(5)?;
                    let session_id: Option<String> = row.get(6)?;

                    let source = match imported_from_share_id {
                        Some(s) => CanvasPersistSource::Imported { share_id: s },
                        None => CanvasPersistSource::Created,
                    };
                    // Fall back to updated_at when persisted_at is NULL so
                    // callers always get a non-null timestamp.
                    let persisted_at = persisted_at_raw.unwrap_or(updated_at);

                    out.push(CanvasPersistSummary {
                        id,
                        title,
                        content_type,
                        source,
                        persisted_at,
                        updated_at,
                        session_id,
                    });
                }
                out
            };

            Ok((total, items))
        })?;

        Ok(CanvasPersistListResponse { items, total })
    }
}
