//! Enrichment provider trait + the v1 no-op implementation.
//!
//! Architecture doc §8 describes "Path A" — passive enrichment of pages
//! using external data sources (Crustdata, Happenstance, Exa, ...). v1 ships
//! only [`NoOpEnrichmentProvider`]; concrete providers land post-M3 once the
//! ingest pipeline is stable.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::error::BrainResult;

/// Reference to a knowledge-base entity that may be enriched.
#[derive(Debug, Clone)]
pub struct EntityRef {
    /// Slug of the page the entity belongs to.
    pub slug: String,
    /// Kind hint to help the provider pick the right backend.
    pub kind: EntityKind,
}

/// Coarse entity classification.
#[derive(Debug, Clone)]
pub enum EntityKind {
    /// A natural person.
    Person,
    /// A company / organization.
    Company,
    /// An abstract concept / topic.
    Concept,
    /// Anything else; carries a backend-specific label.
    Other(String),
}

/// Output of an [`EnrichmentProvider::enrich`] call.
#[derive(Debug, Default)]
pub struct EnrichmentResult {
    /// Markdown to append to the page's compiled-truth section.
    pub markdown_additions: String,
    /// Frontmatter keys to set/overwrite on the page.
    pub frontmatter_updates: HashMap<String, serde_json::Value>,
}

/// Trait for components that augment a knowledge-base entity with
/// external data.
#[async_trait]
pub trait EnrichmentProvider: Send + Sync {
    /// Produce enrichment data for `entity`.
    async fn enrich(&self, entity: EntityRef) -> BrainResult<EnrichmentResult>;
}

/// v1's only [`EnrichmentProvider`] — returns [`EnrichmentResult::default`]
/// (no additions, no frontmatter changes).
///
/// v2 may bring Crustdata / Happenstance / Exa providers.
pub struct NoOpEnrichmentProvider;

#[async_trait]
impl EnrichmentProvider for NoOpEnrichmentProvider {
    async fn enrich(&self, _entity: EntityRef) -> BrainResult<EnrichmentResult> {
        Ok(EnrichmentResult::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BrainResult;

    #[tokio::test]
    async fn no_op_enrichment_returns_default() {
        let provider = NoOpEnrichmentProvider;
        let entity = EntityRef {
            slug: "x".into(),
            kind: EntityKind::Concept,
        };
        let r: BrainResult<EnrichmentResult> = provider.enrich(entity).await;
        let r = r.unwrap();
        assert!(r.markdown_additions.is_empty());
        assert!(r.frontmatter_updates.is_empty());
    }
}
