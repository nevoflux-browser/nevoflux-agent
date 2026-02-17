// crates/daemon/src/learning/retriever.rs
//
// Knowledge retriever with session-scoped cache. Provides the read path for the
// learning system by caching the five soul documents and querying SQLite for
// relevant knowledge entries filtered by domain and category.

use std::sync::Arc;

use nevoflux_storage::{Knowledge, SiteAdaptation, Storage};

use super::soul::manager::FiveDocCache;

/// Configuration for knowledge retrieval.
#[derive(Debug, Clone)]
pub struct RetrievalConfig {
    /// Maximum number of knowledge entries to return per query.
    pub top_k: usize,
    /// Minimum decay score for an entry to be included in results.
    pub min_decay_score: f64,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            top_k: 5,
            min_decay_score: 0.05, // Exclude archived entries
        }
    }
}

/// Result of a knowledge retrieval query.
#[derive(Debug, Clone)]
pub struct RetrievalResult {
    /// Relevant knowledge entries from SQLite, sorted by relevance score descending.
    pub entries: Vec<ScoredKnowledge>,
    /// Site-specific adaptation entries from the site_adaptations table.
    pub site_adaptations: Vec<SiteAdaptation>,
}

/// A knowledge entry annotated with its computed relevance score.
#[derive(Debug, Clone)]
pub struct ScoredKnowledge {
    pub entry: Knowledge,
    pub relevance_score: f64,
}

/// Retrieves relevant knowledge for a given context.
///
/// Holds a session-scoped cache of the five soul documents (loaded at session
/// start) and queries SQLite for knowledge entries matching domain/category
/// filters. Entries are scored using lazy decay and confidence, then filtered
/// and truncated to the configured top-K.
pub struct KnowledgeRetriever {
    soul_cache: Arc<FiveDocCache>,
    storage: Arc<Storage>,
    config: RetrievalConfig,
}

impl KnowledgeRetriever {
    /// Create a new retriever initialized from a soul cache and storage handle.
    pub fn new(soul_cache: Arc<FiveDocCache>, storage: Arc<Storage>) -> Self {
        Self {
            soul_cache,
            storage,
            config: RetrievalConfig::default(),
        }
    }

    /// Create a retriever with custom configuration.
    pub fn with_config(
        soul_cache: Arc<FiveDocCache>,
        storage: Arc<Storage>,
        config: RetrievalConfig,
    ) -> Self {
        Self {
            soul_cache,
            storage,
            config,
        }
    }

    /// Replace the cached soul documents with a fresh snapshot.
    pub fn invalidate_cache(&mut self, new_cache: Arc<FiveDocCache>) {
        self.soul_cache = new_cache;
    }

    /// Get a reference to the cached soul documents.
    pub fn soul_cache(&self) -> &FiveDocCache {
        &self.soul_cache
    }

    /// Retrieve relevant knowledge for a given domain and category.
    ///
    /// 1. Queries SQLite for knowledge entries matching the domain (if provided).
    ///    When no domain is given, queries validated entries instead.
    /// 2. Computes a basic relevance score for each entry: `decay * confidence`.
    ///    (The full composite formula is deferred to Task 23.)
    /// 3. Filters out archived entries and those below `min_decay_score`.
    /// 4. Sorts by relevance descending and truncates to `top_k`.
    /// 5. Queries site adaptations for the domain (if provided).
    pub fn retrieve(
        &self,
        domain: Option<&str>,
        _category: Option<&str>,
    ) -> crate::error::Result<RetrievalResult> {
        // 1. Query knowledge entries by domain (if provided)
        let candidates = if let Some(d) = domain {
            self.storage.knowledge().query_by_domain(d, 100)?
        } else {
            // No domain filter -- query validated entries as a starting point
            self.storage.knowledge().query_validated(100)?
        };

        // 2. Compute relevance scores and filter
        let now = chrono::Utc::now();
        let mut scored: Vec<ScoredKnowledge> = candidates
            .into_iter()
            .filter(|e| e.status != "archived") // Exclude archived
            .map(|entry| {
                let score = self.compute_relevance(&entry, now);
                ScoredKnowledge {
                    entry,
                    relevance_score: score,
                }
            })
            .filter(|sk| sk.relevance_score >= self.config.min_decay_score)
            .collect();

        // 3. Sort by relevance descending
        scored.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // 4. Take top-K
        scored.truncate(self.config.top_k);

        // 5. Query site adaptations
        let site_adaptations = if let Some(d) = domain {
            self.storage.site_adaptations().query_by_domain(d, 10)?
        } else {
            Vec::new()
        };

        Ok(RetrievalResult {
            entries: scored,
            site_adaptations,
        })
    }

    /// Compute a basic relevance score for an entry.
    ///
    /// Uses the decay formula from `decay::calculate_decay` combined with the
    /// entry's confidence. The full composite scoring (category_match *
    /// domain_match * decay * confidence) will be added in Task 23.
    fn compute_relevance(
        &self,
        entry: &Knowledge,
        now: chrono::DateTime<chrono::Utc>,
    ) -> f64 {
        use super::decay::calculate_decay;
        use chrono::DateTime;

        // Parse last_hit_at (or fall back to updated_at)
        let last_hit = entry
            .last_hit_at
            .as_deref()
            .or(Some(entry.updated_at.as_str()))
            .and_then(|s| s.parse::<DateTime<chrono::Utc>>().ok())
            .unwrap_or(now); // If unparseable, treat as fresh

        let decay = calculate_decay(
            last_hit,
            &entry.category,
            entry.effectiveness,
            entry.hit_count as u32,
            now,
        );

        // Basic relevance = decay * confidence
        decay * entry.confidence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nevoflux_storage::{CreateKnowledgeParams, Storage};

    fn make_cache() -> Arc<FiveDocCache> {
        Arc::new(FiveDocCache {
            identity_raw: "# Identity".to_string(),
            soul_raw: "# Soul".to_string(),
            user_raw: "# User".to_string(),
            tools_raw: "# Tools".to_string(),
            agents_raw: "# Agents".to_string(),
            last_parsed_at: Utc::now(),
        })
    }

    fn create_test_entry(
        storage: &Storage,
        category: &str,
        domain: Option<&str>,
        summary: &str,
    ) -> Knowledge {
        let entry = storage
            .knowledge()
            .create(CreateKnowledgeParams {
                category: category.to_string(),
                domain: domain.map(|d| d.to_string()),
                summary: summary.to_string(),
                details: "test details".to_string(),
                ..Default::default()
            })
            .unwrap();
        // Default status is "pending"; queries may need validated status.
        entry
    }

    #[test]
    fn retriever_empty_database_returns_empty() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();
        let retriever = KnowledgeRetriever::new(cache, storage);

        let result = retriever.retrieve(Some("example.com"), None).unwrap();
        assert!(result.entries.is_empty());
        assert!(result.site_adaptations.is_empty());
    }

    #[test]
    fn retriever_returns_matching_entries() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        // Create an entry for github.com
        let entry = create_test_entry(
            &storage,
            "site_interaction",
            Some("github.com"),
            "github uses data-testid",
        );
        // query_by_domain returns entries regardless of status, so this should work
        let retriever = KnowledgeRetriever::new(cache, Arc::clone(&storage));

        let result = retriever.retrieve(Some("github.com"), None).unwrap();
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].entry.id, entry.id);
    }

    #[test]
    fn retriever_filters_archived_entries() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        // Create an entry and archive it
        let entry = create_test_entry(
            &storage,
            "site_interaction",
            Some("example.com"),
            "archived entry",
        );
        storage
            .knowledge()
            .update_status(&entry.id, "archived")
            .unwrap();

        let retriever = KnowledgeRetriever::new(cache, Arc::clone(&storage));
        let result = retriever.retrieve(Some("example.com"), None).unwrap();
        assert!(
            result.entries.is_empty(),
            "archived entries should be excluded"
        );
    }

    #[test]
    fn retriever_respects_top_k_limit() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        // Create more entries than top_k (default = 5)
        for i in 0..8 {
            create_test_entry(
                &storage,
                "site_interaction",
                Some("example.com"),
                &format!("entry {}", i),
            );
        }

        let retriever = KnowledgeRetriever::new(cache, Arc::clone(&storage));
        let result = retriever.retrieve(Some("example.com"), None).unwrap();
        assert!(
            result.entries.len() <= 5,
            "should respect top_k=5, got {}",
            result.entries.len()
        );
    }

    #[test]
    fn retriever_sorts_by_relevance_descending() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        // Create entries; they all start fresh so should have similar decay,
        // but the scoring formula is decay * confidence. We can manipulate
        // confidence by not having a direct setter, but we can verify ordering.
        for i in 0..3 {
            create_test_entry(
                &storage,
                "site_interaction",
                Some("example.com"),
                &format!("entry {}", i),
            );
        }

        let retriever = KnowledgeRetriever::new(cache, Arc::clone(&storage));
        let result = retriever.retrieve(Some("example.com"), None).unwrap();

        // Verify descending order
        for window in result.entries.windows(2) {
            assert!(
                window[0].relevance_score >= window[1].relevance_score,
                "entries should be sorted by relevance descending: {} >= {}",
                window[0].relevance_score,
                window[1].relevance_score,
            );
        }
    }

    #[test]
    fn retriever_no_domain_queries_validated() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        // Create a pending entry (should not appear in validated query)
        create_test_entry(&storage, "site_interaction", None, "pending entry");

        // Create a validated entry
        let validated = create_test_entry(
            &storage,
            "tool_optimization",
            None,
            "validated entry",
        );
        storage
            .knowledge()
            .update_status(&validated.id, "validated")
            .unwrap();

        let retriever = KnowledgeRetriever::new(cache, Arc::clone(&storage));
        let result = retriever.retrieve(None, None).unwrap();

        // Only the validated entry should be returned
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].entry.id, validated.id);
    }

    #[test]
    fn invalidate_cache_updates_cache() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();
        let mut retriever = KnowledgeRetriever::new(cache, storage);

        assert_eq!(retriever.soul_cache().identity_raw, "# Identity");

        // Build a new cache with different content
        let new_cache = Arc::new(FiveDocCache {
            identity_raw: "# New Identity".to_string(),
            soul_raw: "# New Soul".to_string(),
            user_raw: "# New User".to_string(),
            tools_raw: "# New Tools".to_string(),
            agents_raw: "# New Agents".to_string(),
            last_parsed_at: Utc::now(),
        });

        retriever.invalidate_cache(new_cache);
        assert_eq!(retriever.soul_cache().identity_raw, "# New Identity");
    }

    #[test]
    fn retriever_with_custom_config() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        // Create entries
        for i in 0..5 {
            create_test_entry(
                &storage,
                "site_interaction",
                Some("example.com"),
                &format!("entry {}", i),
            );
        }

        let config = RetrievalConfig {
            top_k: 2,
            min_decay_score: 0.05,
        };
        let retriever =
            KnowledgeRetriever::with_config(cache, Arc::clone(&storage), config);

        let result = retriever.retrieve(Some("example.com"), None).unwrap();
        assert!(
            result.entries.len() <= 2,
            "custom top_k=2 should limit results, got {}",
            result.entries.len()
        );
    }

    #[test]
    fn retriever_includes_site_adaptations() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        // Create a site adaptation
        use nevoflux_storage::CreateSiteAdaptationParams;
        storage
            .site_adaptations()
            .create(
                CreateSiteAdaptationParams::new(
                    "example.com",
                    "selector_result",
                    r#"{"selector": ".content"}"#,
                )
                .with_id("SA-test01"),
            )
            .unwrap();

        let retriever = KnowledgeRetriever::new(cache, Arc::clone(&storage));
        let result = retriever.retrieve(Some("example.com"), None).unwrap();

        assert_eq!(result.site_adaptations.len(), 1);
        assert_eq!(result.site_adaptations[0].id, "SA-test01");
    }

    #[test]
    fn retriever_no_domain_returns_no_site_adaptations() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        // Create a site adaptation (should not be returned without domain)
        use nevoflux_storage::CreateSiteAdaptationParams;
        storage
            .site_adaptations()
            .create(CreateSiteAdaptationParams::new(
                "example.com",
                "selector_result",
                r#"{}"#,
            ))
            .unwrap();

        let retriever = KnowledgeRetriever::new(cache, Arc::clone(&storage));
        let result = retriever.retrieve(None, None).unwrap();

        assert!(
            result.site_adaptations.is_empty(),
            "no domain = no site adaptations"
        );
    }

    #[test]
    fn retrieval_config_default_values() {
        let config = RetrievalConfig::default();
        assert_eq!(config.top_k, 5);
        assert!((config.min_decay_score - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn relevance_score_is_positive_for_fresh_entries() {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let cache = make_cache();

        create_test_entry(
            &storage,
            "site_interaction",
            Some("example.com"),
            "fresh entry",
        );

        let retriever = KnowledgeRetriever::new(cache, Arc::clone(&storage));
        let result = retriever.retrieve(Some("example.com"), None).unwrap();

        assert_eq!(result.entries.len(), 1);
        assert!(
            result.entries[0].relevance_score > 0.0,
            "fresh entry should have positive relevance, got {}",
            result.entries[0].relevance_score,
        );
    }
}
