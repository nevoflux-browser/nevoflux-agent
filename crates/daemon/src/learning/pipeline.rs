//! Learning pipeline that drains entries from the in-memory buffer, persists
//! them to SQLite via `KnowledgeRepository`, validates pending entries
//! that meet configurable thresholds, and promotes validated entries into
//! the soul Markdown documents.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use nevoflux_storage::{CreateKnowledgeParams, Storage};

use super::buffer::MemoryBuffer;
use super::routing;
use super::soul::manager::{SoulChange, SoulManager};
use super::soul::protection::{self, ChangePermission};
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

/// Configurable thresholds for the promotion step.
///
/// Only validated entries whose confidence meets `min_confidence` are eligible
/// for promotion. The batch size controls how many entries are processed per
/// `promote()` call.
#[derive(Debug, Clone)]
pub struct PromotionThresholds {
    /// Minimum confidence score for promotion (0.0 to 1.0).
    pub min_confidence: f64,
    /// Maximum number of entries to promote in a single batch.
    pub batch_size: usize,
}

impl Default for PromotionThresholds {
    fn default() -> Self {
        Self {
            min_confidence: 0.6,
            batch_size: 50,
        }
    }
}

/// Result of a promotion run, reporting how many entries were promoted vs skipped.
#[derive(Debug, Clone, Default)]
pub struct PromotionResult {
    /// Number of entries successfully promoted to soul documents.
    pub promoted: usize,
    /// Number of entries skipped because their protection level requires
    /// user confirmation (RequireConfirm / RequireDoubleConfirm / Forbidden).
    pub skipped_protection: usize,
    /// Number of entries skipped because they did not meet the promotion
    /// thresholds (e.g., confidence too low).
    pub skipped_threshold: usize,
    /// Number of entries that failed during the apply_change step.
    pub failed: usize,
}

/// Pipeline that flushes `LearningEntry` items from a `MemoryBuffer` into the
/// SQLite `knowledge` table, validates pending entries that meet thresholds,
/// and promotes validated entries into soul documents.
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

    /// Promote validated knowledge entries into soul Markdown documents.
    ///
    /// For each validated entry:
    /// 1. Determines the target document and section via `route_knowledge()`.
    ///    If the entry already has `promotion_target` set, it is preferred.
    /// 2. Checks the protection level via `check_permission()`.
    /// 3. Only auto-promotes entries with `AutoWithNotify` permission; entries
    ///    with stricter protections are skipped.
    /// 4. Builds a `SoulChange` and calls `SoulManager::apply_change()`.
    /// 5. Marks the entry as "promoted" in SQLite with a `promoted_at` timestamp.
    ///
    /// Returns a `PromotionResult` with counts of promoted/skipped entries.
    pub async fn promote(
        &self,
        thresholds: &PromotionThresholds,
        soul_manager: &mut SoulManager,
    ) -> Result<PromotionResult> {
        let validated = self
            .storage
            .knowledge()
            .query_validated(thresholds.batch_size)?;

        let mut result = PromotionResult::default();

        for entry in &validated {
            // Check confidence threshold
            if entry.confidence < thresholds.min_confidence {
                result.skipped_threshold += 1;
                continue;
            }

            // Determine target document and section
            let route = routing::route_knowledge(entry);

            // Check protection level — only auto-promote AutoWithNotify
            let permission = protection::check_permission(&route.target_file, &route.section);
            if permission != ChangePermission::AutoWithNotify {
                result.skipped_protection += 1;
                continue;
            }

            // Build the SoulChange
            let content = if let Some(ref resolution) = entry.resolution {
                format!(
                    "- **{}** ({}): {} — {}",
                    entry.summary,
                    entry.domain.as_deref().unwrap_or("universal"),
                    entry.details,
                    resolution
                )
            } else {
                format!(
                    "- **{}** ({}): {}",
                    entry.summary,
                    entry.domain.as_deref().unwrap_or("universal"),
                    entry.details
                )
            };

            let change = SoulChange {
                target_file: route.target_file.clone(),
                section: route.section.clone(),
                change_type: "add".to_string(),
                new_content: content,
                reason: format!("Auto-promoted from knowledge entry {}", entry.id),
                source_type: "system".to_string(),
                confidence: entry.confidence,
            };

            // Apply the change to the soul document
            match soul_manager.apply_change(change).await {
                Ok(()) => {
                    // Mark as promoted in SQLite
                    self.storage
                        .knowledge()
                        .mark_promoted(&entry.id, &route.section)?;
                    result.promoted += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        entry_id = %entry.id,
                        error = %e,
                        "failed to promote knowledge entry"
                    );
                    result.failed += 1;
                }
            }
        }

        Ok(result)
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

    // --- Promotion pipeline tests ---

    #[test]
    fn promotion_thresholds_default_values() {
        let thresholds = PromotionThresholds::default();
        assert!((thresholds.min_confidence - 0.6).abs() < f64::EPSILON);
        assert_eq!(thresholds.batch_size, 50);
    }

    #[test]
    fn promotion_result_default_is_zero() {
        let result = PromotionResult::default();
        assert_eq!(result.promoted, 0);
        assert_eq!(result.skipped_protection, 0);
        assert_eq!(result.skipped_threshold, 0);
        assert_eq!(result.failed, 0);
    }

    /// Helper to create a validated entry with high confidence in SQLite.
    fn create_validated_entry(
        storage: &Storage,
        category: &str,
        subcategory: Option<&str>,
        domain: Option<&str>,
        summary: &str,
        confidence: f64,
    ) -> nevoflux_storage::Knowledge {
        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: category.into(),
                subcategory: subcategory.map(|s| s.to_string()),
                domain: domain.map(|d| d.to_string()),
                summary: summary.into(),
                details: "test details".into(),
                ..Default::default()
            })
            .unwrap();

        // Set confidence and status to validated
        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET confidence = ?1, status = 'validated' WHERE id = ?2",
                    params![confidence, created.id],
                )?;
                Ok(())
            })
            .unwrap();

        storage.knowledge().get(&created.id).unwrap().unwrap()
    }

    #[tokio::test]
    async fn promote_writes_to_tools_md() {
        let (pipeline, storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // Create a validated site_interaction entry → TOOLS.md / Site Adaptation Graph
        let entry = create_validated_entry(
            &storage,
            "site_interaction",
            Some("selector"),
            Some("github.com"),
            "GitHub uses data-testid selectors",
            0.85,
        );

        let thresholds = PromotionThresholds {
            min_confidence: 0.6,
            batch_size: 10,
        };

        let result = pipeline.promote(&thresholds, &mut manager).await.unwrap();
        assert_eq!(result.promoted, 1);
        assert_eq!(result.skipped_protection, 0);
        assert_eq!(result.skipped_threshold, 0);
        assert_eq!(result.failed, 0);

        // Verify the entry is now "promoted" in SQLite
        let updated = storage.knowledge().get(&entry.id).unwrap().unwrap();
        assert_eq!(updated.status, "promoted");
        assert!(updated.promoted_at.is_some());
        assert_eq!(
            updated.promoted_section,
            Some("Site Adaptation Graph".to_string())
        );

        // Verify content was written to TOOLS.md
        let content = tokio::fs::read_to_string(soul_dir.join("TOOLS.md"))
            .await
            .unwrap();
        assert!(content.contains("GitHub uses data-testid selectors"));
        assert!(content.contains("github.com"));
    }

    #[tokio::test]
    async fn promote_writes_user_preference_to_user_md() {
        let (pipeline, storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // user_preference / language → USER.md / Communication Overrides
        create_validated_entry(
            &storage,
            "user_preference",
            Some("language"),
            None,
            "User prefers concise replies",
            0.9,
        );

        let thresholds = PromotionThresholds::default();
        let result = pipeline.promote(&thresholds, &mut manager).await.unwrap();
        assert_eq!(result.promoted, 1);

        let content = tokio::fs::read_to_string(soul_dir.join("USER.md"))
            .await
            .unwrap();
        assert!(content.contains("User prefers concise replies"));
    }

    #[tokio::test]
    async fn promote_skips_low_confidence_entries() {
        let (pipeline, storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // Entry with low confidence (0.3 < 0.6 threshold)
        create_validated_entry(
            &storage,
            "site_interaction",
            None,
            None,
            "Low confidence entry",
            0.3,
        );

        let thresholds = PromotionThresholds {
            min_confidence: 0.6,
            batch_size: 10,
        };

        let result = pipeline.promote(&thresholds, &mut manager).await.unwrap();
        assert_eq!(result.promoted, 0);
        assert_eq!(result.skipped_threshold, 1);
    }

    #[tokio::test]
    async fn promote_skips_protected_sections() {
        let (pipeline, storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // Create a validated entry that routes to a protected file
        // We'll set promotion_target to IDENTITY.md (RequireDoubleConfirm)
        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "site_interaction".into(),
                summary: "should be skipped".into(),
                details: "details".into(),
                promotion_target: Some("IDENTITY".into()),
                ..Default::default()
            })
            .unwrap();

        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET confidence = 0.9, status = 'validated' WHERE id = ?1",
                    params![created.id],
                )?;
                Ok(())
            })
            .unwrap();

        let thresholds = PromotionThresholds::default();
        let result = pipeline.promote(&thresholds, &mut manager).await.unwrap();
        assert_eq!(result.promoted, 0);
        assert_eq!(result.skipped_protection, 1);

        // Entry should still be validated, not promoted
        let entry = storage.knowledge().get(&created.id).unwrap().unwrap();
        assert_eq!(entry.status, "validated");
    }

    #[tokio::test]
    async fn promote_no_validated_entries_returns_empty_result() {
        let (pipeline, _storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        let thresholds = PromotionThresholds::default();
        let result = pipeline.promote(&thresholds, &mut manager).await.unwrap();
        assert_eq!(result.promoted, 0);
        assert_eq!(result.skipped_protection, 0);
        assert_eq!(result.skipped_threshold, 0);
        assert_eq!(result.failed, 0);
    }

    #[tokio::test]
    async fn promote_multiple_entries_in_one_batch() {
        let (pipeline, storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // Create 3 validated entries with different categories
        create_validated_entry(
            &storage,
            "site_interaction",
            None,
            Some("example.com"),
            "Site uses react",
            0.8,
        );
        create_validated_entry(
            &storage,
            "tool_optimization",
            None,
            None,
            "Increase timeout to 60s",
            0.75,
        );
        create_validated_entry(
            &storage,
            "user_preference",
            Some("domain"),
            None,
            "User works in fintech",
            0.9,
        );

        let thresholds = PromotionThresholds {
            min_confidence: 0.6,
            batch_size: 10,
        };

        let result = pipeline.promote(&thresholds, &mut manager).await.unwrap();
        assert_eq!(result.promoted, 3);

        // Verify TOOLS.md got updated
        let tools = tokio::fs::read_to_string(soul_dir.join("TOOLS.md"))
            .await
            .unwrap();
        assert!(tools.contains("Site uses react"));
        assert!(tools.contains("Increase timeout to 60s"));

        // Verify USER.md got updated
        let user = tokio::fs::read_to_string(soul_dir.join("USER.md"))
            .await
            .unwrap();
        assert!(user.contains("User works in fintech"));
    }

    #[tokio::test]
    async fn promote_respects_batch_size_limit() {
        let (pipeline, storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // Create 5 validated entries
        for i in 0..5 {
            create_validated_entry(
                &storage,
                "site_interaction",
                None,
                Some(&format!("site{}.com", i)),
                &format!("Entry {}", i),
                0.8,
            );
        }

        // batch_size of 2 should only process 2
        let thresholds = PromotionThresholds {
            min_confidence: 0.6,
            batch_size: 2,
        };

        let result = pipeline.promote(&thresholds, &mut manager).await.unwrap();
        assert_eq!(result.promoted, 2);

        // 3 entries should still be validated
        let remaining = storage.knowledge().query_validated(10).unwrap();
        assert_eq!(remaining.len(), 3);
    }

    #[tokio::test]
    async fn promote_entry_with_existing_promotion_target_preferred() {
        let (pipeline, storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // Create an entry with promotion_target already set to TOOLS
        let created = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: "user_preference".into(),
                subcategory: Some("language".into()),
                summary: "Custom routed entry".into(),
                details: "details".into(),
                promotion_target: Some("TOOLS".into()),
                ..Default::default()
            })
            .unwrap();

        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET confidence = 0.9, status = 'validated' WHERE id = ?1",
                    params![created.id],
                )?;
                Ok(())
            })
            .unwrap();

        let thresholds = PromotionThresholds::default();
        let result = pipeline.promote(&thresholds, &mut manager).await.unwrap();
        assert_eq!(result.promoted, 1);

        // Should have been written to TOOLS.md, not USER.md
        let tools = tokio::fs::read_to_string(soul_dir.join("TOOLS.md"))
            .await
            .unwrap();
        assert!(tools.contains("Custom routed entry"));
    }

    #[tokio::test]
    async fn promote_end_to_end_flush_validate_promote() {
        let (pipeline, storage) = setup();
        let tmp = tempfile::TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // Step 1: Insert entry into buffer and flush to SQLite
        pipeline.buffer().insert(
            LearningEntry::new(
                LearningCategory::SiteInteraction,
                "selector",
                "example.com uses semantic selectors",
            )
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                ..Default::default()
            })
            .with_details("Verified across multiple pages"),
        );

        let flush_count = pipeline.flush().unwrap();
        assert_eq!(flush_count, 1);

        // Step 2: Update entry to meet validation thresholds, then validate
        let pending = storage.knowledge().query_pending(10).unwrap();
        assert_eq!(pending.len(), 1);

        storage
            .database()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE knowledge SET hit_count = 5, confidence = 0.85 WHERE id = ?1",
                    params![pending[0].id],
                )?;
                Ok(())
            })
            .unwrap();

        let val_thresholds = ValidationThresholds {
            min_occurrences: 3,
            min_confidence: 0.6,
            min_alive_hours: 0,
        };
        let validated_count = pipeline.validate(&val_thresholds).unwrap();
        assert_eq!(validated_count, 1);

        // Step 3: Promote
        let promo_thresholds = PromotionThresholds::default();
        let result = pipeline
            .promote(&promo_thresholds, &mut manager)
            .await
            .unwrap();
        assert_eq!(result.promoted, 1);

        // Verify the full pipeline result
        let entry = storage.knowledge().get(&pending[0].id).unwrap().unwrap();
        assert_eq!(entry.status, "promoted");
        assert!(entry.promoted_at.is_some());

        let tools = tokio::fs::read_to_string(soul_dir.join("TOOLS.md"))
            .await
            .unwrap();
        assert!(tools.contains("example.com uses semantic selectors"));
    }
}
