//! Flush pipeline that drains entries from the in-memory buffer and persists
//! them to SQLite via `KnowledgeRepository`.

use std::sync::Arc;

use nevoflux_storage::{CreateKnowledgeParams, Storage};

use super::buffer::MemoryBuffer;
use super::types::LearningEntry;
use crate::error::Result;

/// Pipeline that flushes `LearningEntry` items from a `MemoryBuffer` into the
/// SQLite `knowledge` table.
pub struct LearningPipeline {
    buffer: Arc<MemoryBuffer>,
    storage: Arc<Storage>,
}

impl LearningPipeline {
    /// Create a new pipeline that reads from `buffer` and writes to `storage`.
    pub fn new(buffer: Arc<MemoryBuffer>, storage: Arc<Storage>) -> Self {
        Self { buffer, storage }
    }

    /// Drain all entries from the buffer and insert them into the SQLite
    /// knowledge table.
    ///
    /// Returns the number of entries that were flushed.
    pub fn flush(&self) -> Result<usize> {
        let entries = self.buffer.drain_all();
        let count = entries.len();
        for entry in &entries {
            let params = Self::entry_to_knowledge_params(entry);
            self.storage.knowledge().create(params)?;
        }
        self.buffer.mark_flushed();
        Ok(count)
    }

    /// Convert a `LearningEntry` to `CreateKnowledgeParams` for SQLite
    /// insertion.
    fn entry_to_knowledge_params(entry: &LearningEntry) -> CreateKnowledgeParams {
        CreateKnowledgeParams {
            category: format!("{:?}", entry.category).to_lowercase(),
            subcategory: entry.subcategory.clone(),
            domain: entry.context.domain.clone(),
            summary: entry.summary.clone(),
            details: entry.details.clone().unwrap_or_default(),
            source_type: Some("system".into()),
            privacy_level: Some(format!("{:?}", entry.privacy_level).to_lowercase()),
            ..Default::default()
        }
    }

    /// Get a reference to the underlying buffer (for inserting entries).
    pub fn buffer(&self) -> &MemoryBuffer {
        &self.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning::types::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn setup() -> (LearningPipeline, Arc<Storage>) {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let buffer = Arc::new(MemoryBuffer::new(20, Duration::from_secs(30)));
        let pipeline = LearningPipeline::new(buffer, storage.clone());
        (pipeline, storage)
    }

    #[test]
    fn flush_moves_entries_to_sqlite() {
        let (pipeline, storage) = setup();

        // Insert entries into buffer
        pipeline.buffer().insert(
            LearningEntry::new(
                LearningCategory::SiteInteraction,
                "click_failed",
                "Button click failed",
            )
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                ..Default::default()
            }),
        );

        pipeline.buffer().insert(LearningEntry::new(
            LearningCategory::ToolOptimization,
            "timeout",
            "Tool timed out",
        ));

        assert_eq!(pipeline.buffer().len(), 2);

        // Flush
        let count = pipeline.flush().unwrap();
        assert_eq!(count, 2);
        assert_eq!(pipeline.buffer().len(), 0);

        // Verify in SQLite
        let results = storage
            .knowledge()
            .query_by_domain("example.com", 10)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].summary, "Button click failed");
    }

    #[test]
    fn flush_empty_buffer_returns_zero() {
        let (pipeline, _) = setup();
        let count = pipeline.flush().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn entry_to_knowledge_params_maps_fields_correctly() {
        let entry = LearningEntry::new(
            LearningCategory::SiteInteraction,
            "selector_changed",
            "Selector .btn was replaced",
        )
        .with_context(LearningContext {
            domain: Some("test.com".into()),
            ..Default::default()
        })
        .with_subcategory("css_selectors")
        .with_details("The .btn class was renamed to .button")
        .with_privacy(PrivacyLevel::Public);

        let params = LearningPipeline::entry_to_knowledge_params(&entry);

        assert_eq!(params.category, "siteinteraction");
        assert_eq!(params.subcategory, Some("css_selectors".into()));
        assert_eq!(params.domain, Some("test.com".into()));
        assert_eq!(params.summary, "Selector .btn was replaced");
        assert_eq!(params.details, "The .btn class was renamed to .button");
        assert_eq!(params.source_type, Some("system".into()));
        assert_eq!(params.privacy_level, Some("public".into()));
    }

    #[test]
    fn flush_preserves_domain_none_entries() {
        let (pipeline, storage) = setup();

        // Insert an entry without a domain
        pipeline.buffer().insert(LearningEntry::new(
            LearningCategory::UserPreference,
            "language",
            "User prefers English",
        ));

        let count = pipeline.flush().unwrap();
        assert_eq!(count, 1);

        // Entry without domain should not appear in domain-specific query
        let results = storage
            .knowledge()
            .query_by_domain("example.com", 10)
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn multiple_flushes_accumulate_in_sqlite() {
        let (pipeline, storage) = setup();

        // First flush
        pipeline.buffer().insert(
            LearningEntry::new(
                LearningCategory::SiteInteraction,
                "click_failed",
                "First failure",
            )
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                ..Default::default()
            }),
        );
        pipeline.flush().unwrap();

        // Second flush
        pipeline.buffer().insert(
            LearningEntry::new(
                LearningCategory::SiteInteraction,
                "scroll_failed",
                "Second failure",
            )
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                ..Default::default()
            }),
        );
        pipeline.flush().unwrap();

        // Both entries should be in SQLite
        let results = storage
            .knowledge()
            .query_by_domain("example.com", 10)
            .unwrap();
        assert_eq!(results.len(), 2);
    }
}
