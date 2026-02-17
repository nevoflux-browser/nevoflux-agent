//! Learning pipeline that drains entries from the in-memory buffer, persists
//! them to SQLite via `KnowledgeRepository`, and validates pending entries
//! that meet configurable thresholds.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use nevoflux_storage::{CreateKnowledgeParams, Storage};

use super::buffer::MemoryBuffer;
use super::types::LearningEntry;
use crate::error::Result;

/// Configurable thresholds that a pending knowledge entry must meet
/// before it can be promoted to "validated" status.
///
/// An entry qualifies for validation when ALL thresholds are satisfied:
/// - `hit_count >= min_occurrences`
/// - `confidence >= min_confidence`
/// - age (in hours since `created_at`) >= `min_alive_hours`
#[derive(Debug, Clone)]
pub struct ValidationThresholds {
    /// Minimum number of times the entry must have been observed.
    pub min_occurrences: u32,
    /// Minimum confidence score (0.0 to 1.0).
    pub min_confidence: f64,
    /// Minimum age in hours since the entry was created.
    pub min_alive_hours: u64,
}

impl Default for ValidationThresholds {
    fn default() -> Self {
        Self {
            min_occurrences: 3,
            min_confidence: 0.6,
            min_alive_hours: 24,
        }
    }
}

/// Pipeline that flushes `LearningEntry` items from a `MemoryBuffer` into the
/// SQLite `knowledge` table, and validates pending entries that meet thresholds.
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

    /// Validate pending knowledge entries that meet the given thresholds.
    ///
    /// Queries all pending entries from the `knowledge` table, checks each
    /// against the configured thresholds, and promotes qualifying entries
    /// from "pending" to "validated" status.
    ///
    /// Returns the number of entries that were validated.
    pub fn validate(&self, thresholds: &ValidationThresholds) -> Result<usize> {
        let pending = self.storage.knowledge().query_pending(1000)?;
        let now = Utc::now();
        let mut validated_count = 0;

        for entry in &pending {
            // Check hit_count threshold
            if (entry.hit_count as u32) < thresholds.min_occurrences {
                continue;
            }

            // Check confidence threshold
            if entry.confidence < thresholds.min_confidence {
                continue;
            }

            // Check age threshold: parse created_at as RFC3339 and compute hours elapsed
            if thresholds.min_alive_hours > 0 {
                if let Ok(created) = entry.created_at.parse::<DateTime<Utc>>() {
                    let age_hours = (now - created).num_hours();
                    if age_hours < 0 || (age_hours as u64) < thresholds.min_alive_hours {
                        continue;
                    }
                } else {
                    // If we cannot parse the timestamp, skip this entry
                    continue;
                }
            }

            // Entry meets all thresholds — promote to validated
            self.storage.knowledge().update_status(&entry.id, "validated")?;
            validated_count += 1;
        }

        Ok(validated_count)
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
    use nevoflux_storage::CreateKnowledgeParams;
    use rusqlite::params;
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

    // --- Validation pipeline tests ---

    #[test]
    fn validation_thresholds_default_values() {
        let thresholds = ValidationThresholds::default();
        assert_eq!(thresholds.min_occurrences, 3);
        assert!((thresholds.min_confidence - 0.6).abs() < f64::EPSILON);
        assert_eq!(thresholds.min_alive_hours, 24);
    }

    #[test]
    fn validate_promotes_qualifying_entries() {
        let (pipeline, storage) = setup();

        // Create a knowledge entry directly in SQLite
        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "site_interaction".into(),
                summary: "test entry".into(),
                details: "test details".into(),
                domain: Some("example.com".into()),
                ..Default::default()
            })
            .unwrap();

        // Default confidence is 0.5 and hit_count is 1.
        // Update hit_count and confidence via raw SQL so the entry qualifies.
        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET hit_count = 5, confidence = 0.8 WHERE id = ?1",
                    params![created.id],
                )?;
                Ok(())
            })
            .unwrap();

        // Use relaxed age threshold (0 hours) so the freshly-created entry qualifies
        let thresholds = ValidationThresholds {
            min_occurrences: 3,
            min_confidence: 0.6,
            min_alive_hours: 0,
        };

        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 1);

        // Verify status was updated to "validated"
        let entry = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert_eq!(entry.status, "validated");
    }

    #[test]
    fn validate_skips_entries_below_threshold() {
        let (pipeline, storage) = setup();

        // Create entry with default hit_count=1, confidence=0.5
        storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "site_interaction".into(),
                summary: "low confidence".into(),
                details: "details".into(),
                ..Default::default()
            })
            .unwrap();

        // Strict thresholds that the default entry cannot meet
        let thresholds = ValidationThresholds {
            min_occurrences: 5,
            min_confidence: 0.9,
            min_alive_hours: 0,
        };

        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn validate_skips_entries_below_hit_count() {
        let (pipeline, storage) = setup();

        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "tool_optimization".into(),
                summary: "high confidence but low hits".into(),
                details: "details".into(),
                ..Default::default()
            })
            .unwrap();

        // Set high confidence but leave hit_count at 1
        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET confidence = 0.95 WHERE id = ?1",
                    params![created.id],
                )?;
                Ok(())
            })
            .unwrap();

        let thresholds = ValidationThresholds {
            min_occurrences: 3,
            min_confidence: 0.6,
            min_alive_hours: 0,
        };

        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 0);

        // Entry should still be pending
        let entry = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert_eq!(entry.status, "pending");
    }

    #[test]
    fn validate_skips_entries_below_confidence() {
        let (pipeline, storage) = setup();

        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "tool_optimization".into(),
                summary: "high hits but low confidence".into(),
                details: "details".into(),
                ..Default::default()
            })
            .unwrap();

        // Set high hit_count but leave confidence at 0.5
        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET hit_count = 10 WHERE id = ?1",
                    params![created.id],
                )?;
                Ok(())
            })
            .unwrap();

        let thresholds = ValidationThresholds {
            min_occurrences: 3,
            min_confidence: 0.9,
            min_alive_hours: 0,
        };

        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 0);

        let entry = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert_eq!(entry.status, "pending");
    }

    #[test]
    fn validate_skips_entries_too_young() {
        let (pipeline, storage) = setup();

        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "site_interaction".into(),
                summary: "freshly created".into(),
                details: "details".into(),
                ..Default::default()
            })
            .unwrap();

        // Set high hit_count and confidence
        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET hit_count = 10, confidence = 0.9 WHERE id = ?1",
                    params![created.id],
                )?;
                Ok(())
            })
            .unwrap();

        // Require 24 hours of age — the entry was just created
        let thresholds = ValidationThresholds {
            min_occurrences: 1,
            min_confidence: 0.5,
            min_alive_hours: 24,
        };

        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 0);

        let entry = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert_eq!(entry.status, "pending");
    }

    #[test]
    fn validate_promotes_old_entry_with_backdated_created_at() {
        let (pipeline, storage) = setup();

        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "site_interaction".into(),
                summary: "old entry".into(),
                details: "details".into(),
                ..Default::default()
            })
            .unwrap();

        // Backdate created_at to 48 hours ago and set qualifying stats
        let old_time = (Utc::now() - chrono::Duration::hours(48))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();

        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET hit_count = 5, confidence = 0.8, created_at = ?1 WHERE id = ?2",
                    params![old_time, created.id],
                )?;
                Ok(())
            })
            .unwrap();

        let thresholds = ValidationThresholds {
            min_occurrences: 3,
            min_confidence: 0.6,
            min_alive_hours: 24,
        };

        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 1);

        let entry = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert_eq!(entry.status, "validated");
    }

    #[test]
    fn validate_only_affects_pending_entries() {
        let (pipeline, storage) = setup();

        // Create two entries, both with qualifying stats
        let entry1 = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "site_interaction".into(),
                summary: "pending entry".into(),
                details: "details".into(),
                ..Default::default()
            })
            .unwrap();

        let entry2 = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "site_interaction".into(),
                summary: "already validated".into(),
                details: "details".into(),
                ..Default::default()
            })
            .unwrap();

        // Give both high stats
        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET hit_count = 10, confidence = 0.9 WHERE id = ?1",
                    params![entry1.id],
                )?;
                conn.execute(
                    "UPDATE knowledge SET hit_count = 10, confidence = 0.9, status = 'validated' WHERE id = ?1",
                    params![entry2.id],
                )?;
                Ok(())
            })
            .unwrap();

        let thresholds = ValidationThresholds {
            min_occurrences: 3,
            min_confidence: 0.6,
            min_alive_hours: 0,
        };

        // Only the pending entry should be validated
        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn validate_with_multiple_qualifying_entries() {
        let (pipeline, storage) = setup();

        // Create three qualifying entries
        for i in 0..3 {
            let created = storage
                .knowledge()
                .create(CreateKnowledgeParams {
                    category: "site_interaction".into(),
                    summary: format!("entry {}", i),
                    details: "details".into(),
                    ..Default::default()
                })
                .unwrap();

            storage
                .database()
                .with_connection(|conn| {
                    conn.execute(
                        "UPDATE knowledge SET hit_count = 5, confidence = 0.8 WHERE id = ?1",
                        params![created.id],
                    )?;
                    Ok(())
                })
                .unwrap();
        }

        let thresholds = ValidationThresholds {
            min_occurrences: 3,
            min_confidence: 0.6,
            min_alive_hours: 0,
        };

        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn validate_no_pending_entries_returns_zero() {
        let (pipeline, _storage) = setup();

        let thresholds = ValidationThresholds::default();
        let count = pipeline.validate(&thresholds).unwrap();
        assert_eq!(count, 0);
    }
}
