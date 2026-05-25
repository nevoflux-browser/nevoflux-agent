//! Ingest-signal scaffold (architecture doc §8 "Path A" hooks).
//!
//! Defines the [`IngestSignal`] trait and the v1-only [`ManualSaveIngest`]
//! implementation. Future variants (page-visit, conversation-end, calendar
//! event) land in M3+.

use async_trait::async_trait;

use crate::engine::BrainEngine;
use crate::error::BrainResult;
use crate::types::BrainPage;

/// All possible ingest signals.
///
/// v1 only emits [`BrainSignal::ManualSave`]; the enum is non-exhaustive in
/// spirit (we will add v2 variants without breaking the API by simply
/// extending it).
#[derive(Clone, Debug)]
pub enum BrainSignal {
    /// User explicitly chose to persist some content into the brain.
    ManualSave {
        /// Where the content originated.
        source: SaveSource,
        /// The content itself.
        content: SaveContent,
    },
    // v2 will add: PageVisit, ConvEnd, CalendarEvent, ...
}

/// Origin of a manually saved page.
#[derive(Clone, Debug)]
pub enum SaveSource {
    /// A page visited in the browser.
    WebPage {
        /// Canonical URL.
        url: String,
        /// Page title (used as a slug fallback).
        title: String,
    },
    /// A conversation in the chat UI.
    ChatConversation {
        /// Stable conversation identifier.
        conversation_id: String,
    },
    // v2: Canvas, Upload
}

/// Payload for a manual save.
#[derive(Clone, Debug)]
pub struct SaveContent {
    /// Raw markdown to persist.
    pub markdown: String,
    /// Caller-suggested slug (used verbatim when present).
    pub suggested_slug: Option<String>,
    /// Caller-suggested directory (currently informational; M3 will route on it).
    pub suggested_directory: Option<String>,
}

/// Result of [`IngestSignal::process`].
#[derive(Debug)]
pub struct IngestResult {
    /// Slug of the page that was created/updated/skipped.
    pub slug: String,
    /// What happened.
    pub action: IngestAction,
}

/// Outcome of an ingest pipeline run.
#[derive(Debug)]
pub enum IngestAction {
    /// The signal produced a brand-new page.
    Created,
    /// The signal updated an existing page.
    Updated,
    /// The signal was intentionally dropped; the string explains why.
    Skipped(String),
}

/// Trait for components that turn a [`BrainSignal`] into engine mutations.
#[async_trait]
pub trait IngestSignal: Send + Sync {
    /// Process a single signal.
    async fn process(
        &self,
        signal: BrainSignal,
        engine: &dyn BrainEngine,
    ) -> BrainResult<IngestResult>;
}

/// v1's only [`IngestSignal`] implementation: convert a [`BrainSignal::ManualSave`]
/// into a direct `engine.put(...)` call. M3 will add `PageVisitIngest`,
/// `ConvEndIngest`, etc.
pub struct ManualSaveIngest;

#[async_trait]
impl IngestSignal for ManualSaveIngest {
    async fn process(
        &self,
        signal: BrainSignal,
        engine: &dyn BrainEngine,
    ) -> BrainResult<IngestResult> {
        match signal {
            BrainSignal::ManualSave { source, content } => {
                let slug = content
                    .suggested_slug
                    .unwrap_or_else(|| derive_slug_from_source(&source));
                let page = BrainPage::from_markdown(slug.clone(), content.markdown);
                let put_result = engine.put(page).await?;
                Ok(IngestResult {
                    slug,
                    action: if put_result.created {
                        IngestAction::Created
                    } else {
                        IngestAction::Updated
                    },
                })
            }
        }
    }
}

/// Quick deterministic slug fallback. M3 will replace this with a richer
/// deriver (collision detection, normalization, etc.).
fn derive_slug_from_source(source: &SaveSource) -> String {
    match source {
        SaveSource::WebPage { title, .. } => title
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect(),
        SaveSource::ChatConversation { conversation_id } => {
            format!("chat-{conversation_id}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Hit, ImportOpts, ImportReport, NbrainBundle, PageMeta, PutResult, SearchOpts, Selection,
        SourceMeta, SourceSpec, StripRules, SyncReport,
    };
    use crate::BrainError;

    // Stub BrainEngine for testing ManualSaveIngest.
    struct StubEngine {
        last_put: std::sync::Mutex<Option<BrainPage>>,
    }

    #[async_trait::async_trait]
    impl BrainEngine for StubEngine {
        async fn search(&self, _q: &str, _o: SearchOpts) -> BrainResult<Vec<Hit>> {
            Ok(vec![])
        }
        async fn put(&self, page: BrainPage) -> BrainResult<PutResult> {
            let slug = page.slug.clone();
            *self.last_put.lock().unwrap() = Some(page);
            Ok(PutResult {
                slug,
                created: true,
                updated: false,
            })
        }
        async fn get(&self, _slug: &str) -> BrainResult<BrainPage> {
            Err(BrainError::NotImplemented)
        }
        async fn list(&self, _dir: &str) -> BrainResult<Vec<PageMeta>> {
            Ok(vec![])
        }
        async fn delete(&self, _slug: &str) -> BrainResult<()> {
            Ok(())
        }
        async fn sync(&self) -> BrainResult<SyncReport> {
            Ok(SyncReport::default())
        }
        async fn export_snapshot(
            &self,
            _sel: Selection,
            _rules: StripRules,
        ) -> BrainResult<NbrainBundle> {
            Err(BrainError::NotImplemented)
        }
        async fn import_snapshot(
            &self,
            _bundle: NbrainBundle,
            _opts: ImportOpts,
        ) -> BrainResult<ImportReport> {
            Err(BrainError::NotImplemented)
        }
        async fn add_source(&self, _src: SourceSpec) -> BrainResult<()> {
            Ok(())
        }
        async fn remove_source(&self, _n: &str) -> BrainResult<()> {
            Ok(())
        }
        async fn list_sources(&self) -> BrainResult<Vec<SourceMeta>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn manual_save_ingest_calls_engine_put() {
        let engine = StubEngine {
            last_put: std::sync::Mutex::new(None),
        };
        let signal = BrainSignal::ManualSave {
            source: SaveSource::WebPage {
                url: "https://example.com".into(),
                title: "Example".into(),
            },
            content: SaveContent {
                markdown: "# Hello".into(),
                suggested_slug: Some("hello".into()),
                suggested_directory: None,
            },
        };
        let ingest = ManualSaveIngest;
        let result = ingest.process(signal, &engine).await.unwrap();
        assert_eq!(result.slug, "hello");
        assert!(matches!(result.action, IngestAction::Created));
        assert!(engine.last_put.lock().unwrap().is_some());
    }

    #[test]
    fn derive_slug_from_webpage_normalizes_title() {
        let s = derive_slug_from_source(&SaveSource::WebPage {
            url: "https://example.com".into(),
            title: "Hello, World!".into(),
        });
        assert_eq!(s, "hello--world-");
    }

    #[test]
    fn derive_slug_from_chat_uses_id_prefix() {
        let s = derive_slug_from_source(&SaveSource::ChatConversation {
            conversation_id: "abc123".into(),
        });
        assert_eq!(s, "chat-abc123");
    }
}
