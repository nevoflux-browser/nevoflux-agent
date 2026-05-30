//! Data types shared by the brain crate's traits.
//!
//! These mirror the architecture doc §6.3 surface. Content fields are plain
//! `String` for v1; M3 will introduce richer markdown / frontmatter types
//! once the gbrain backend dictates a concrete schema.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A knowledge-base page.
///
/// v1 stores raw markdown in [`Self::compiled_truth`] / [`Self::timeline`];
/// M3 will replace these with structured representations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainPage {
    /// Stable identifier (filename without `.md`).
    pub slug: String,
    /// Human-readable title.
    pub title: String,
    /// "Compiled truth" section — distilled knowledge.
    pub compiled_truth: String,
    /// "Timeline" section — append-only history of edits/notes.
    pub timeline: String,
    /// Arbitrary frontmatter key/value pairs (raw JSON values for v1).
    pub frontmatter: HashMap<String, serde_json::Value>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last-modification timestamp.
    pub updated_at: DateTime<Utc>,
}

impl BrainPage {
    /// Build a [`BrainPage`] from raw markdown.
    ///
    /// Splits on the first `\n---\n` separator: everything above becomes
    /// `compiled_truth`, everything below becomes `timeline`. Without a
    /// separator the entire input is treated as `compiled_truth`.
    ///
    /// TODO(M3): real frontmatter parsing. v1 leaves `frontmatter` empty
    /// and reuses the slug as the title.
    pub fn from_markdown(slug: String, raw_markdown: String) -> Self {
        let (compiled_truth, timeline) = match raw_markdown.split_once("\n---\n") {
            Some((above, below)) => (above.to_string(), below.to_string()),
            None => (raw_markdown, String::new()),
        };
        let now = Utc::now();
        Self {
            title: slug.clone(),
            slug,
            compiled_truth,
            timeline,
            frontmatter: HashMap::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

/// Lightweight page summary returned by listing / search APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMeta {
    /// Stable identifier (matches [`BrainPage::slug`]).
    pub slug: String,
    /// Human-readable title.
    pub title: String,
    /// Last-modification timestamp.
    pub updated_at: DateTime<Utc>,
    /// Source name (which `SourceSpec` produced this page), if any.
    pub source: Option<String>,
}

/// A search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    /// Metadata for the matched page.
    pub page_meta: PageMeta,
    /// Backend-defined relevance score (higher = more relevant).
    pub score: f32,
    /// Optional snippet around the match.
    pub snippet: Option<String>,
}

/// Options for [`crate::BrainEngine::search`].
///
/// v1 keeps this minimal; gbrain surfaces many more knobs (date ranges,
/// source filters, rerank toggles) that will land as M3 fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOpts {
    /// Maximum number of hits to return.
    pub top_k: usize,
    /// Optional free-form filter expression (backend-specific).
    pub filter: Option<String>,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self {
            top_k: 10,
            filter: None,
        }
    }
}

/// Result of a `put` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutResult {
    /// Final slug of the persisted page.
    pub slug: String,
    /// True iff the page did not previously exist.
    pub created: bool,
    /// True iff an existing page was overwritten.
    pub updated: bool,
}

/// Summary of a `sync` call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncReport {
    /// Pages added since the previous sync.
    pub added: u64,
    /// Pages updated since the previous sync.
    pub updated: u64,
    /// Pages removed since the previous sync.
    pub deleted: u64,
}

/// Selection of pages/directories for export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Selection {
    /// Explicit page slugs.
    Files(Vec<String>),
    /// Everything under a directory.
    Directory(String),
    /// Combination of both.
    Mixed {
        /// Explicit page slugs.
        files: Vec<String>,
        /// Directory paths.
        directories: Vec<String>,
    },
}

/// Privacy rules applied when exporting a snapshot.
///
/// Defaults are deliberately privacy-safe: only the `compiled_truth`
/// section is included, raw timeline data is excluded, and the frontmatter
/// whitelist is empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StripRules {
    /// If true, drop the `timeline` section and ship `compiled_truth` only.
    pub compiled_only: bool,
    /// Frontmatter keys allowed through the export filter.
    pub frontmatter_whitelist: Vec<String>,
    /// If true, raw / unprocessed sections are removed entirely.
    pub raw_excluded: bool,
    /// Frontmatter keys explicitly stripped even if otherwise allowed.
    pub frontmatter_redacted: Vec<String>,
    /// How to handle links pointing outside the exported set.
    pub broken_link_policy: BrokenLinkPolicy,
    /// Directory prefixes excluded from export. `.raw` is always excluded
    /// regardless of this list (invariant A.3).
    pub directories_excluded: Vec<String>,
}

impl Default for StripRules {
    fn default() -> Self {
        Self {
            compiled_only: true,
            frontmatter_whitelist: Vec::new(),
            raw_excluded: true,
            frontmatter_redacted: Vec::new(),
            broken_link_policy: BrokenLinkPolicy::default(),
            directories_excluded: Vec::new(),
        }
    }
}

/// How to handle wiki-style links that point outside the exported set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BrokenLinkPolicy {
    /// Leave `[[link]]` as literal text (default).
    #[default]
    KeepAsText,
}

/// Shareable artifact produced by `export_snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NbrainBundle {
    /// Complete `.nbrain` artifact bytes (header + ciphertext). Write to
    /// disk or upload as-is.
    pub artifact: Vec<u8>,
    /// Random 256-bit content key (zero-knowledge mode). Distribute
    /// out-of-band / in a URL fragment. `None` in password mode.
    pub key: Option<[u8; 32]>,
}

/// Material a receiver supplies to unlock a `.nbrain` artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Unlock {
    /// Raw 256-bit content key (zero-knowledge mode).
    Key([u8; 32]),
    /// User password (advanced fallback); key re-derived via Argon2id.
    Password(String),
}

/// Trust level granted to an imported source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportTrust {
    /// Imported pages cannot be mutated locally.
    ReadOnly,
    /// Imported pages may be merged into the local knowledge base.
    FullMerge,
}

/// Options for `import_snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportOpts {
    /// Logical name to associate with the imported source.
    pub source_name: String,
    /// Trust level to apply.
    pub trust: ImportTrust,
    /// Material to unlock the artifact.
    pub unlock: Unlock,
}

/// Summary of an `import_snapshot` call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportReport {
    /// Number of pages successfully imported.
    pub files_imported: u64,
    /// Slugs that conflicted during merge.
    pub conflicts: Vec<String>,
}

/// Spec for a new source backed by a local directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpec {
    /// Logical name for the source.
    pub name: String,
    /// On-disk directory holding the source's pages.
    pub directory: PathBuf,
    /// Trust level granted to the source.
    pub trust: ImportTrust,
}

/// Metadata about a registered source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceMeta {
    /// Logical name for the source.
    pub name: String,
    /// On-disk directory holding the source's pages.
    pub directory: PathBuf,
    /// Trust level granted to the source.
    pub trust: ImportTrust,
    /// Number of pages currently provided by the source.
    pub page_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brain_page_from_markdown_splits_on_separator() {
        let raw = "compiled truth here\n---\ntimeline here";
        let page = BrainPage::from_markdown("slug".into(), raw.into());
        assert_eq!(page.slug, "slug");
        assert!(page.compiled_truth.contains("compiled truth"));
        assert!(page.timeline.contains("timeline"));
    }

    #[test]
    fn brain_page_no_separator_falls_back_to_compiled_only() {
        let raw = "just a single section";
        let page = BrainPage::from_markdown("slug".into(), raw.into());
        assert_eq!(page.compiled_truth.trim(), "just a single section");
        assert_eq!(page.timeline, "");
    }

    #[test]
    fn strip_rules_default_is_privacy_safe() {
        let r = StripRules::default();
        assert!(r.compiled_only);
        assert!(r.raw_excluded);
        assert!(r.frontmatter_whitelist.is_empty());
    }
}
