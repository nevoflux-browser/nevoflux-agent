use super::source::LearningSource;
use super::types::{LearningEntry, PrivacyLevel};
use tracing::{debug, info};

/// Collects learning entries from registered sources,
/// deduplicates, filters, and outputs them.
/// Phase 0: log-only output. Phase 2: writes to DashMap buffer.
pub struct LearningCollector {
    sources: Vec<Box<dyn LearningSource>>,
    domain_blacklist: Vec<String>,
}

impl LearningCollector {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            domain_blacklist: Vec::new(),
        }
    }

    pub fn register_source(&mut self, source: Box<dyn LearningSource>) {
        self.sources.push(source);
    }

    pub fn set_domain_blacklist(&mut self, blacklist: Vec<String>) {
        self.domain_blacklist = blacklist;
    }

    /// Collect entries from all registered sources.
    pub fn collect_all(&mut self) -> Vec<LearningEntry> {
        let mut all_entries = Vec::new();

        for source in &self.sources {
            let entries = source.collect();
            debug!(
                source = source.source_name(),
                count = entries.len(),
                "Collected learning entries"
            );
            all_entries.extend(entries);
        }

        let filtered = self.filter_privacy(all_entries);
        let filtered = self.filter_blacklisted_domains(filtered);
        let deduped = self.dedup(filtered);

        info!(count = deduped.len(), "Learning collector cycle complete");

        deduped
    }

    /// Filter out private entries -- they must never be persisted.
    pub fn filter_privacy(&self, entries: Vec<LearningEntry>) -> Vec<LearningEntry> {
        entries
            .into_iter()
            .filter(|e| e.privacy_level != PrivacyLevel::Private)
            .collect()
    }

    /// Filter out entries from blacklisted domains.
    fn filter_blacklisted_domains(&self, entries: Vec<LearningEntry>) -> Vec<LearningEntry> {
        if self.domain_blacklist.is_empty() {
            return entries;
        }
        entries
            .into_iter()
            .filter(|e| {
                if let Some(domain) = &e.context.domain {
                    !self.domain_blacklist.iter().any(|b| domain.contains(b))
                } else {
                    true
                }
            })
            .collect()
    }

    /// Deduplicate entries by (domain, selector, category, source_event) exact match.
    /// Merges duplicates by incrementing occurrence_count.
    pub fn dedup(&self, entries: Vec<LearningEntry>) -> Vec<LearningEntry> {
        let mut result: Vec<LearningEntry> = Vec::new();

        for entry in entries {
            if let Some(existing) = result.iter_mut().find(|e| Self::is_duplicate(e, &entry)) {
                existing.occurrence_count += 1;
                existing.last_seen_at = entry.last_seen_at;
                if entry.confidence > existing.confidence {
                    existing.confidence = entry.confidence;
                }
            } else {
                result.push(entry);
            }
        }

        result
    }

    fn is_duplicate(a: &LearningEntry, b: &LearningEntry) -> bool {
        a.category == b.category
            && a.context.domain == b.context.domain
            && a.context.selector == b.context.selector
            && a.source_event == b.source_event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning::source::LearningSource;
    use crate::learning::types::*;
    use std::sync::{Arc, Mutex};

    struct FakeSource {
        entries: Arc<Mutex<Vec<LearningEntry>>>,
    }

    impl LearningSource for FakeSource {
        fn source_name(&self) -> &str {
            "fake"
        }

        fn collect(&self) -> Vec<LearningEntry> {
            let mut entries = self.entries.lock().unwrap();
            entries.drain(..).collect()
        }
    }

    #[test]
    fn collector_registers_source_and_collects() {
        let entries = Arc::new(Mutex::new(vec![LearningEntry::new(
            LearningCategory::SiteInteraction,
            "click",
            "click failed",
        )]));

        let source = FakeSource {
            entries: entries.clone(),
        };

        let mut collector = LearningCollector::new();
        collector.register_source(Box::new(source));

        let collected = collector.collect_all();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].source_event, "click");
    }

    #[test]
    fn collector_dedup_exact_match() {
        let entry1 = LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed")
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                selector: Some(".btn".into()),
                ..Default::default()
            });

        let entry2 = LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed")
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                selector: Some(".btn".into()),
                ..Default::default()
            });

        let collector = LearningCollector::new();
        let deduped = collector.dedup(vec![entry1, entry2]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].occurrence_count, 2);
    }

    #[test]
    fn collector_no_dedup_different_domains() {
        let entry1 = LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed")
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                selector: Some(".btn".into()),
                ..Default::default()
            });

        let entry2 = LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed")
            .with_context(LearningContext {
                domain: Some("other.com".into()),
                selector: Some(".btn".into()),
                ..Default::default()
            });

        let collector = LearningCollector::new();
        let deduped = collector.dedup(vec![entry1, entry2]);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn collector_filters_private_entries() {
        let entry = LearningEntry::new(
            LearningCategory::UserPreference,
            "password",
            "user typed password",
        )
        .with_privacy(PrivacyLevel::Private);

        let collector = LearningCollector::new();
        let filtered = collector.filter_privacy(vec![entry]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn collector_keeps_non_private_entries() {
        let entries = vec![
            LearningEntry::new(LearningCategory::SiteInteraction, "a", "s")
                .with_privacy(PrivacyLevel::Public),
            LearningEntry::new(LearningCategory::SiteInteraction, "b", "s")
                .with_privacy(PrivacyLevel::Internal),
            LearningEntry::new(LearningCategory::SiteInteraction, "c", "s")
                .with_privacy(PrivacyLevel::Sensitive),
        ];

        let collector = LearningCollector::new();
        let filtered = collector.filter_privacy(entries);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn collector_filters_blacklisted_domains() {
        let entries = Arc::new(Mutex::new(vec![
            LearningEntry::new(LearningCategory::SiteInteraction, "a", "s").with_context(
                LearningContext {
                    domain: Some("bank.com".into()),
                    ..Default::default()
                },
            ),
            LearningEntry::new(LearningCategory::SiteInteraction, "b", "s").with_context(
                LearningContext {
                    domain: Some("github.com".into()),
                    ..Default::default()
                },
            ),
            LearningEntry::new(LearningCategory::SiteInteraction, "c", "s"), // no domain
        ]));

        let source = FakeSource {
            entries: entries.clone(),
        };

        let mut collector = LearningCollector::new();
        collector.set_domain_blacklist(vec!["bank.com".into()]);
        collector.register_source(Box::new(source));

        let collected = collector.collect_all();
        // bank.com should be filtered out, github.com and no-domain should remain
        assert_eq!(collected.len(), 2);
        assert!(collected
            .iter()
            .all(|e| e.context.domain.as_deref() != Some("bank.com")));
    }
}
