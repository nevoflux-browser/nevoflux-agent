//! Daemon-owned complete page index for the KB browse list + share dialog.
//!
//! gbrain `list_pages` hard-caps `limit` at 100 with no offset
//! (`clampSearchLimit(_,50,100)`), so it cannot drive real pagination over
//! a large brain. This module builds the daemon's OWN complete index by
//! walking `<brain_dir>/atlas/**/*.md` (the on-disk source of truth written
//! by gbrain put_page write-through), unions a `list_pages` (<=100) gbrain
//! cross-check by slug to surface any recent DB-only page, then filters +
//! sorts + paginates in-memory.
//!
//! The walk/filter/sort/paginate logic is factored into PURE functions so it
//! is unit-testable without gbrain (feed a constructed `Vec<PageMeta>` / a
//! tempfile atlas dir). The `PageIndex` builder + TTL cache (Tasks 1-2) wrap
//! these. `journal/` is intentionally NOT walked (append-only log, not pages),
//! matching the existing `atlas/`-only convention.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use nevoflux_brain::PageMeta;
use tokio::sync::Mutex as AsyncMutex;

use super::supervisor::McpToolCaller;

/// Sort order for the page list. `UpdatedDesc` is the default (matches the
/// pre-pagination `list_pages { sort: "updated_desc" }` behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    UpdatedDesc,
    UpdatedAsc,
    Slug,
}

impl SortOrder {
    /// Parse the `sort` RPC param. Unknown / missing -> `UpdatedDesc`.
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("updated_asc") => SortOrder::UpdatedAsc,
            Some("slug") => SortOrder::Slug,
            _ => SortOrder::UpdatedDesc,
        }
    }
}

/// Filter + pagination query, derived from the `brain.list` RPC params.
#[derive(Debug, Clone)]
pub struct ListQuery {
    /// Slug-prefix filter (existing `dir` semantics). Empty = no prefix filter.
    pub dir: String,
    /// Case-insensitive substring matched against slug OR title. Empty = no q filter.
    pub q: String,
    pub sort: SortOrder,
    /// Clamped to >= 0 by [`Self::clamp`].
    pub offset: usize,
    /// Clamped to 1..=200 by [`Self::clamp`].
    pub limit: usize,
}

impl ListQuery {
    pub const DEFAULT_LIMIT: usize = 50;
    pub const MAX_LIMIT: usize = 200;

    /// Clamp invalid offset/limit per spec section 8: offset >= 0 (usize is
    /// already non-negative; a missing/negative JSON value maps to 0 at the
    /// RPC layer), limit in 1..=200.
    pub fn clamp(mut self) -> Self {
        if self.limit == 0 {
            self.limit = Self::DEFAULT_LIMIT;
        }
        if self.limit > Self::MAX_LIMIT {
            self.limit = Self::MAX_LIMIT;
        }
        self
    }
}

/// The result slice returned to the RPC layer.
#[derive(Debug, Clone)]
pub struct ListSlice {
    pub pages: Vec<PageMeta>,
    /// Count AFTER filters, BEFORE offset/limit (drives the page-count UI).
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

/// Normalize a path relative to `atlas/` into a slug: strip the `.md`
/// extension and normalize OS separators to `/`. e.g. on Windows
/// `wiki\people\alice.md` -> `wiki/people/alice`.
pub fn slug_from_relpath(rel: &Path) -> String {
    let without_ext = match rel.extension() {
        Some(ext) if ext.eq_ignore_ascii_case("md") => rel.with_extension(""),
        _ => rel.to_path_buf(),
    };
    without_ext
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// Parse ONLY a leading YAML frontmatter block for a `title:` field. Returns
/// `None` when there is no frontmatter (file does not start with `---`) or no
/// `title` key. Early-stops at the closing `---` so we never YAML-parse the
/// whole body (which is arbitrary markdown).
pub fn parse_frontmatter_title(content: &str) -> Option<String> {
    // Frontmatter must be the very first line. Accept a leading BOM/whitespace
    // only as far as the first non-empty line being `---`.
    let mut lines = content.lines();
    let first = lines.next()?.trim_end_matches('\r');
    if first.trim() != "---" {
        return None;
    }
    let mut yaml = String::new();
    for line in lines {
        let l = line.trim_end_matches('\r');
        if l.trim() == "---" {
            // closing fence reached
            let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).ok()?;
            let title = parsed.get("title")?.as_str()?.trim().to_string();
            return if title.is_empty() { None } else { Some(title) };
        }
        yaml.push_str(l);
        yaml.push('\n');
    }
    // No closing fence -> not a valid frontmatter block.
    None
}

/// Convert a filesystem mtime into a chrono `DateTime<Utc>`. Falls back to
/// `Utc::now()` if the platform mtime is unavailable.
fn mtime_to_utc(modified: std::io::Result<SystemTime>) -> DateTime<Utc> {
    modified
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now)
}

/// Recursively walk `atlas_dir` collecting one [`PageMeta`] per `*.md` file.
/// Hand-rolled `std::fs::read_dir` (NOT `walkdir`/`ignore`) so the brain repo
/// bare-`*` `.gitignore` whitelist does not hide files. Missing `atlas_dir`
/// returns an empty Vec (NOT an error) per spec section 8. Non-`.md` files and
/// unreadable entries are skipped silently.
pub fn walk_atlas(atlas_dir: &Path) -> Vec<PageMeta> {
    let mut out = Vec::new();
    if !atlas_dir.is_dir() {
        return out;
    }
    // Slugs MUST include the `atlas/` segment to match gbrain's slug format.
    // gbrain stores slug = path relative to `sync.repo_path` (the brain dir),
    // e.g. `atlas/中医/yc` — `resolvePageFilePath` joins `brainDir + slug + ".md"`.
    // So we strip the brain root (atlas's parent), NOT atlas_dir itself. Stripping
    // atlas_dir produced `中医/yc`, which (a) never deduped against gbrain's
    // `atlas/中医/yc` in the list_pages cross-check (counted them as new →
    // e.g. 321 + 100 = 421) and (b) wouldn't resolve in gbrain `get_page`.
    let strip_root = atlas_dir.parent().unwrap_or(atlas_dir);
    walk_into(strip_root, atlas_dir, &mut out);
    out
}

fn walk_into(root: &Path, dir: &Path, out: &mut Vec<PageMeta>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            walk_into(root, &path, out);
        } else if file_type.is_file()
            && path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let slug = slug_from_relpath(rel);
            if slug.is_empty() {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let title = parse_frontmatter_title(&content).unwrap_or_else(|| {
                // Fallback: last slug segment.
                slug.rsplit('/').next().unwrap_or(&slug).to_string()
            });
            let updated_at = mtime_to_utc(entry.metadata().and_then(|m| m.modified()));
            out.push(PageMeta {
                slug,
                title,
                updated_at,
                source: None,
            });
        }
    }
}

/// Union a filesystem-walk index with gbrain `list_pages` entries: any
/// `extra` slug NOT already present from the walk is appended. Dedup is by
/// slug; the walk entry wins on conflict (its mtime + frontmatter title are
/// authoritative on disk).
pub fn union_by_slug(mut walked: Vec<PageMeta>, extra: Vec<PageMeta>) -> Vec<PageMeta> {
    use std::collections::HashSet;
    let seen: HashSet<String> = walked.iter().map(|p| p.slug.clone()).collect();
    for e in extra {
        if !seen.contains(&e.slug) {
            walked.push(e);
        }
    }
    walked
}

/// Apply the `dir` slug-prefix filter + case-insensitive `q` substring (slug
/// OR title) filter. Returns the filtered Vec (pre-sort, pre-slice).
pub fn apply_filters(pages: Vec<PageMeta>, query: &ListQuery) -> Vec<PageMeta> {
    let dir_prefix = query.dir.trim_end_matches('/');
    let q_lower = query.q.to_lowercase();
    pages
        .into_iter()
        .filter(|p| {
            let dir_ok = dir_prefix.is_empty()
                || p.slug == dir_prefix
                || p.slug.starts_with(&format!("{dir_prefix}/"));
            let q_ok = q_lower.is_empty()
                || p.slug.to_lowercase().contains(&q_lower)
                || p.title.to_lowercase().contains(&q_lower);
            dir_ok && q_ok
        })
        .collect()
}

/// Sort in place per [`SortOrder`]. `Slug` is ascending lexicographic; the
/// updated orders are by `updated_at` with slug as a stable tie-breaker so
/// bulk-imported same-timestamp pages have a deterministic order (avoiding
/// the cursor-tie issue that motivated this whole approach).
pub fn sort_pages(pages: &mut [PageMeta], order: SortOrder) {
    match order {
        SortOrder::UpdatedDesc => {
            pages.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.slug.cmp(&b.slug)))
        }
        SortOrder::UpdatedAsc => {
            pages.sort_by(|a, b| a.updated_at.cmp(&b.updated_at).then(a.slug.cmp(&b.slug)))
        }
        SortOrder::Slug => pages.sort_by(|a, b| a.slug.cmp(&b.slug)),
    }
}

/// Slice `[offset, offset+limit)` out of an already-filtered+sorted Vec,
/// recording `total` (pre-slice length). An offset past the end yields an
/// empty `pages` with the correct `total`.
pub fn paginate(pages: Vec<PageMeta>, query: &ListQuery) -> ListSlice {
    let total = pages.len();
    let start = query.offset.min(total);
    let end = start.saturating_add(query.limit).min(total);
    ListSlice {
        pages: pages[start..end].to_vec(),
        total,
        offset: query.offset,
        limit: query.limit,
    }
}

/// Full in-memory pipeline over a pre-collected page set: filter -> sort ->
/// paginate. The `PageIndex` builder (Task 1) feeds the union of walk +
/// `list_pages` here; the RPC handler (Task 3) calls it.
pub fn query_pages(all: Vec<PageMeta>, query: &ListQuery) -> ListSlice {
    let mut filtered = apply_filters(all, query);
    sort_pages(&mut filtered, query.sort);
    paginate(filtered, query)
}

/// Parse one gbrain `list_pages` entry into a [`PageMeta`]. gbrain returns
/// `{ slug, type, title, updated_at, deleted_at? }` (operations.ts list_pages
/// handler). Mirrors `GbrainEngine::meta_from_entry`: tolerant of missing
/// fields, `updated_at` parsed as RFC3339 (else now), `source` always None
/// for v1 (DB-only cross-check pages).
fn meta_from_list_entry(item: &serde_json::Value) -> Option<PageMeta> {
    let slug = item.get("slug").and_then(|v| v.as_str())?.to_string();
    if slug.is_empty() {
        return None;
    }
    let title = item
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&slug)
        .to_string();
    let updated_at = item
        .get("updated_at")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    Some(PageMeta {
        slug,
        title,
        updated_at,
        source: None,
    })
}

/// Fetch up to 100 most-recent pages via gbrain `list_pages` for the
/// cross-check. Unwraps the MCP `{ result: { content: [{ text }] } }`
/// envelope (the text is a JSON array of entries). Best-effort: any transport
/// / parse failure returns an empty Vec so the caller degrades to the
/// filesystem-only index.
pub async fn list_pages_crosscheck(transport: &Arc<dyn McpToolCaller>) -> Vec<PageMeta> {
    let args = serde_json::json!({ "limit": 100, "sort": "updated_desc" });
    let resp = match transport.call_tool_dyn("list_pages", args).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "page_index: list_pages cross-check failed; \
                serving filesystem-only index");
            return Vec::new();
        }
    };
    // Unwrap result.content[0].text -> JSON array (same shape GbrainEngine uses).
    let text = resp
        .get("result")
        .filter(|r| !r.get("isError").and_then(|v| v.as_bool()).unwrap_or(false))
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|first| first.get("text"))
        .and_then(|t| t.as_str());
    let Some(text) = text else {
        tracing::warn!("page_index: list_pages response missing result.content[0].text");
        return Vec::new();
    };
    let payload: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "page_index: list_pages text was not JSON");
            return Vec::new();
        }
    };
    let array = payload
        .as_array()
        .cloned()
        .or_else(|| payload.get("pages").and_then(|p| p.as_array()).cloned())
        .unwrap_or_default();
    array.iter().filter_map(meta_from_list_entry).collect()
}

/// Build the complete page set: walk the atlas, union the gbrain `list_pages`
/// cross-check by slug. This is the input to [`query_pages`]. The TTL cache
/// (Task 2) wraps this so rapid page flips don't re-walk.
pub async fn build_page_set(atlas_dir: &Path, transport: &Arc<dyn McpToolCaller>) -> Vec<PageMeta> {
    let walked = walk_atlas(atlas_dir);
    let extra = list_pages_crosscheck(transport).await;
    union_by_slug(walked, extra)
}

/// Default cache TTL. Short enough that a save shows up almost immediately,
/// long enough to absorb rapid list/preview/back navigation without
/// re-walking a large atlas each time.
pub const CACHE_TTL: Duration = Duration::from_secs(15);

struct CacheEntry {
    atlas_dir: PathBuf,
    built_at: Instant,
    pages: Vec<PageMeta>,
}

/// A short-TTL cache over [`build_page_set`], keyed by atlas dir. Held by the
/// RPC layer (one per process, behind an `Arc`). [`Self::invalidate`] is
/// called after writes (`brain.put` / `save_webpage` / `save_conversation` /
/// sync) so a freshly-saved page appears without waiting out the TTL.
pub struct PageIndex {
    inner: AsyncMutex<Option<CacheEntry>>,
    ttl: Duration,
}

impl Default for PageIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl PageIndex {
    pub fn new() -> Self {
        Self {
            inner: AsyncMutex::new(None),
            ttl: CACHE_TTL,
        }
    }

    #[cfg(test)]
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: AsyncMutex::new(None),
            ttl,
        }
    }

    /// Drop any cached set so the next query rebuilds. Cheap; call after any
    /// mutating brain RPC.
    pub async fn invalidate(&self) {
        *self.inner.lock().await = None;
    }

    /// Return the complete page set, rebuilding from disk + gbrain if the
    /// cache is empty, stale (older than the TTL), or built for a different
    /// atlas dir. Clones the cached Vec for the caller (the slice is small
    /// relative to the walk cost).
    pub async fn page_set(
        &self,
        atlas_dir: &Path,
        transport: &Arc<dyn McpToolCaller>,
    ) -> Vec<PageMeta> {
        let mut guard = self.inner.lock().await;
        let fresh = match guard.as_ref() {
            Some(e) => e.atlas_dir == atlas_dir && e.built_at.elapsed() < self.ttl,
            None => false,
        };
        if fresh {
            return guard.as_ref().unwrap().pages.clone();
        }
        let pages = build_page_set(atlas_dir, transport).await;
        *guard = Some(CacheEntry {
            atlas_dir: atlas_dir.to_path_buf(),
            built_at: Instant::now(),
            pages: pages.clone(),
        });
        pages
    }

    /// Convenience: build/get the set then run the filter/sort/paginate
    /// pipeline. This is the single entry point the RPC handler calls.
    pub async fn query(
        &self,
        atlas_dir: &Path,
        transport: &Arc<dyn McpToolCaller>,
        query: &ListQuery,
    ) -> ListSlice {
        let all = self.page_set(atlas_dir, transport).await;
        query_pages(all, query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn meta(slug: &str, title: &str, updated: &str) -> PageMeta {
        PageMeta {
            slug: slug.to_string(),
            title: title.to_string(),
            updated_at: DateTime::parse_from_rfc3339(updated)
                .unwrap()
                .with_timezone(&Utc),
            source: None,
        }
    }

    // ── walk_atlas ────────────────────────────────────────────────

    #[test]
    fn walk_collects_nested_md_with_normalized_slugs() {
        let tmp = tempfile::tempdir().unwrap();
        let atlas = tmp.path().join("atlas");
        fs::create_dir_all(atlas.join("wiki").join("people")).unwrap();
        fs::write(
            atlas.join("wiki").join("people").join("alice.md"),
            "# Alice",
        )
        .unwrap();
        fs::write(atlas.join("root-note.md"), "# Root").unwrap();

        let mut pages = walk_atlas(&atlas);
        pages.sort_by(|a, b| a.slug.cmp(&b.slug));
        assert_eq!(pages.len(), 2);
        // Slugs include the `atlas/` segment to match gbrain's slug format.
        assert_eq!(pages[0].slug, "atlas/root-note");
        assert_eq!(pages[1].slug, "atlas/wiki/people/alice");
    }

    #[test]
    fn walk_missing_atlas_is_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let pages = walk_atlas(&tmp.path().join("does-not-exist"));
        assert!(pages.is_empty());
    }

    #[test]
    fn walk_ignores_non_md_and_journal_is_not_passed() {
        // walk_atlas only ever receives the atlas dir; journal/ lives a level
        // up and is never walked. Here we prove non-.md files are skipped.
        let tmp = tempfile::tempdir().unwrap();
        let atlas = tmp.path().join("atlas");
        fs::create_dir_all(&atlas).unwrap();
        fs::write(atlas.join("keep.md"), "# Keep").unwrap();
        fs::write(atlas.join("skip.txt"), "nope").unwrap();
        fs::write(atlas.join("notes.json"), "{}").unwrap();
        let pages = walk_atlas(&atlas);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].slug, "atlas/keep");
    }

    #[test]
    fn walk_uses_frontmatter_title_else_slug_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let atlas = tmp.path().join("atlas");
        fs::create_dir_all(atlas.join("dir")).unwrap();
        fs::write(
            atlas.join("dir").join("withfm.md"),
            "---\ntitle: Real Title\ntags: [x]\n---\n# Body heading\nbody",
        )
        .unwrap();
        fs::write(
            atlas.join("dir").join("nofm.md"),
            "## Just a heading\nno frontmatter",
        )
        .unwrap();

        let mut pages = walk_atlas(&atlas);
        pages.sort_by(|a, b| a.slug.cmp(&b.slug));
        // nofm: fallback to last slug segment
        assert_eq!(pages[0].slug, "atlas/dir/nofm");
        assert_eq!(pages[0].title, "nofm");
        // withfm: frontmatter title
        assert_eq!(pages[1].slug, "atlas/dir/withfm");
        assert_eq!(pages[1].title, "Real Title");
    }

    // ── parse_frontmatter_title ───────────────────────────────────

    #[test]
    fn frontmatter_title_present() {
        assert_eq!(
            parse_frontmatter_title("---\ntitle: Hello World\n---\nbody"),
            Some("Hello World".to_string())
        );
    }

    #[test]
    fn frontmatter_absent_returns_none() {
        assert_eq!(parse_frontmatter_title("## heading\nno fm"), None);
        assert_eq!(parse_frontmatter_title(""), None);
    }

    #[test]
    fn frontmatter_without_title_returns_none() {
        assert_eq!(
            parse_frontmatter_title("---\ntags: [a, b]\n---\nbody"),
            None
        );
    }

    #[test]
    fn frontmatter_unterminated_returns_none() {
        // No closing fence -> not a valid block -> None (don't parse body).
        assert_eq!(
            parse_frontmatter_title("---\ntitle: X\nbody with no fence"),
            None
        );
    }

    // ── slug_from_relpath ─────────────────────────────────────────

    #[test]
    fn slug_strips_md_and_normalizes_separators() {
        assert_eq!(slug_from_relpath(Path::new("a/b/c.md")), "a/b/c");
        assert_eq!(slug_from_relpath(Path::new("top.md")), "top");
        // A path built from OS components round-trips with `/` joins.
        let nested: PathBuf = ["wiki", "people", "alice.md"].iter().collect();
        assert_eq!(slug_from_relpath(&nested), "wiki/people/alice");
    }

    // ── union_by_slug ─────────────────────────────────────────────

    #[test]
    fn union_appends_only_new_slugs() {
        let walked = vec![meta("a", "A", "2026-05-01T00:00:00Z")];
        let extra = vec![
            meta("a", "A-dup", "2026-05-09T00:00:00Z"), // dup -> walk wins, not appended
            meta("b", "B", "2026-05-02T00:00:00Z"),     // new -> appended
        ];
        let merged = union_by_slug(walked, extra);
        assert_eq!(merged.len(), 2);
        let a = merged.iter().find(|p| p.slug == "a").unwrap();
        assert_eq!(a.title, "A", "walk entry must win on slug conflict");
        assert!(merged.iter().any(|p| p.slug == "b"));
    }

    // ── apply_filters ─────────────────────────────────────────────

    #[test]
    fn filter_by_dir_prefix() {
        let pages = vec![
            meta("wiki/people/alice", "Alice", "2026-05-01T00:00:00Z"),
            meta("wiki/people/bob", "Bob", "2026-05-02T00:00:00Z"),
            meta("daily/2026-05-01", "Daily", "2026-05-03T00:00:00Z"),
        ];
        let q = ListQuery {
            dir: "wiki/people".into(),
            q: String::new(),
            sort: SortOrder::Slug,
            offset: 0,
            limit: 50,
        };
        let out = apply_filters(pages, &q);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|p| p.slug.starts_with("wiki/people/")));
    }

    #[test]
    fn filter_by_q_substring_on_slug_and_title_case_insensitive() {
        let pages = vec![
            meta("notes/rustlang", "Systems", "2026-05-01T00:00:00Z"),
            meta(
                "notes/python",
                "Scripting in RUST too",
                "2026-05-02T00:00:00Z",
            ),
            meta("notes/go", "Concurrency", "2026-05-03T00:00:00Z"),
        ];
        let q = ListQuery {
            dir: String::new(),
            q: "rust".into(),
            sort: SortOrder::Slug,
            offset: 0,
            limit: 50,
        };
        let out = apply_filters(pages, &q);
        // matches "rustlang" slug AND "RUST" in the python page's title
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|p| p.slug == "notes/rustlang"));
        assert!(out.iter().any(|p| p.slug == "notes/python"));
    }

    // ── sort_pages ────────────────────────────────────────────────

    #[test]
    fn sort_updated_desc_and_asc_and_slug() {
        let base = vec![
            meta("c", "C", "2026-05-02T00:00:00Z"),
            meta("a", "A", "2026-05-03T00:00:00Z"),
            meta("b", "B", "2026-05-01T00:00:00Z"),
        ];
        let mut desc = base.clone();
        sort_pages(&mut desc, SortOrder::UpdatedDesc);
        assert_eq!(
            desc.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );

        let mut asc = base.clone();
        sort_pages(&mut asc, SortOrder::UpdatedAsc);
        assert_eq!(
            asc.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );

        let mut slug = base.clone();
        sort_pages(&mut slug, SortOrder::Slug);
        assert_eq!(
            slug.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn sort_ties_break_by_slug() {
        let mut pages = vec![
            meta("z", "Z", "2026-05-01T00:00:00Z"),
            meta("a", "A", "2026-05-01T00:00:00Z"),
        ];
        sort_pages(&mut pages, SortOrder::UpdatedDesc);
        assert_eq!(
            pages[0].slug, "a",
            "same-timestamp ties order by slug ascending"
        );
    }

    // ── paginate ──────────────────────────────────────────────────

    #[test]
    fn paginate_total_is_pre_slice_and_slice_is_correct() {
        let pages: Vec<PageMeta> = (0..10)
            .map(|i| meta(&format!("p{i:02}"), "T", "2026-05-01T00:00:00Z"))
            .collect();
        let q = ListQuery {
            dir: String::new(),
            q: String::new(),
            sort: SortOrder::Slug,
            offset: 3,
            limit: 4,
        };
        let slice = paginate(pages, &q);
        assert_eq!(slice.total, 10);
        assert_eq!(slice.pages.len(), 4);
        assert_eq!(slice.pages[0].slug, "p03");
        assert_eq!(slice.pages[3].slug, "p06");
    }

    #[test]
    fn paginate_offset_past_end_is_empty_with_correct_total() {
        let pages = vec![meta("a", "A", "2026-05-01T00:00:00Z")];
        let q = ListQuery {
            dir: String::new(),
            q: String::new(),
            sort: SortOrder::Slug,
            offset: 100,
            limit: 50,
        };
        let slice = paginate(pages, &q);
        assert_eq!(slice.total, 1);
        assert!(slice.pages.is_empty());
    }

    #[test]
    fn limit_clamp_zero_to_default_and_cap_at_200() {
        let zero = ListQuery {
            dir: String::new(),
            q: String::new(),
            sort: SortOrder::UpdatedDesc,
            offset: 0,
            limit: 0,
        }
        .clamp();
        assert_eq!(zero.limit, ListQuery::DEFAULT_LIMIT);

        let huge = ListQuery {
            dir: String::new(),
            q: String::new(),
            sort: SortOrder::UpdatedDesc,
            offset: 0,
            limit: 9999,
        }
        .clamp();
        assert_eq!(huge.limit, ListQuery::MAX_LIMIT);
    }

    #[test]
    fn sort_order_parse_defaults_to_updated_desc() {
        assert_eq!(SortOrder::parse(None), SortOrder::UpdatedDesc);
        assert_eq!(SortOrder::parse(Some("garbage")), SortOrder::UpdatedDesc);
        assert_eq!(SortOrder::parse(Some("updated_asc")), SortOrder::UpdatedAsc);
        assert_eq!(SortOrder::parse(Some("slug")), SortOrder::Slug);
    }

    #[test]
    fn query_pages_end_to_end_filter_sort_paginate() {
        let pages = vec![
            meta("wiki/a", "Alpha", "2026-05-03T00:00:00Z"),
            meta("wiki/b", "Beta", "2026-05-01T00:00:00Z"),
            meta("other/c", "Gamma", "2026-05-02T00:00:00Z"),
        ];
        let q = ListQuery {
            dir: "wiki".into(),
            q: String::new(),
            sort: SortOrder::UpdatedDesc,
            offset: 0,
            limit: 1,
        };
        let slice = query_pages(pages, &q);
        assert_eq!(slice.total, 2, "two wiki/* pass the dir filter");
        assert_eq!(slice.pages.len(), 1, "limit 1");
        assert_eq!(slice.pages[0].slug, "wiki/a", "newest first");
    }

    // ── list_pages cross-check union (Task 1) ─────────────────────

    use super::super::supervisor::McpToolCallerError;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    /// Minimal McpToolCaller stub (same shape as gbrain/engine.rs tests):
    /// returns a canned response per tool name, records calls.
    struct StubToolCaller {
        responses: HashMap<String, serde_json::Value>,
        calls: StdMutex<Vec<(String, serde_json::Value)>>,
        fail: bool,
    }
    impl StubToolCaller {
        fn ok(responses: Vec<(&str, serde_json::Value)>) -> Arc<dyn McpToolCaller> {
            Arc::new(Self {
                responses: responses
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                calls: StdMutex::new(Vec::new()),
                fail: false,
            })
        }
        fn failing() -> Arc<dyn McpToolCaller> {
            Arc::new(Self {
                responses: HashMap::new(),
                calls: StdMutex::new(Vec::new()),
                fail: true,
            })
        }
    }
    #[async_trait::async_trait]
    impl McpToolCaller for StubToolCaller {
        async fn call_tool_dyn(
            &self,
            name: &str,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value, McpToolCallerError> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), arguments));
            if self.fail {
                return Err("stub transport failure".into());
            }
            match self.responses.get(name).cloned() {
                Some(v) => Ok(v),
                None => Err(format!("no stub for {name}").into()),
            }
        }
    }
    fn list_envelope(entries: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "result": { "content": [{ "type": "text", "text": entries.to_string() }] }
        })
    }

    #[tokio::test]
    async fn crosscheck_parses_entries() {
        let transport = StubToolCaller::ok(vec![(
            "list_pages",
            list_envelope(serde_json::json!([
                { "slug": "db/only", "title": "DB Only", "updated_at": "2026-05-20T00:00:00Z" },
                { "slug": "no/title", "updated_at": "2026-05-21T00:00:00Z" }
            ])),
        )]);
        let pages = list_pages_crosscheck(&transport).await;
        assert_eq!(pages.len(), 2);
        let dbonly = pages.iter().find(|p| p.slug == "db/only").unwrap();
        assert_eq!(dbonly.title, "DB Only");
        let notitle = pages.iter().find(|p| p.slug == "no/title").unwrap();
        assert_eq!(
            notitle.title, "no/title",
            "missing title falls back to slug"
        );
    }

    #[tokio::test]
    async fn crosscheck_failure_degrades_to_empty() {
        let transport = StubToolCaller::failing();
        let pages = list_pages_crosscheck(&transport).await;
        assert!(
            pages.is_empty(),
            "transport failure must NOT panic; returns empty"
        );
    }

    #[tokio::test]
    async fn build_page_set_unions_walk_with_db_only_slug() {
        // Regression for the "321 -> 421" bug: gbrain's list_pages slugs are
        // repo-relative and INCLUDE the `atlas/` segment (resolvePageFilePath
        // joins brainDir + slug + ".md"). The walk must produce the SAME
        // `atlas/...` slug so the union dedups instead of counting every
        // gbrain slug as new (+100 = the list_pages cap).
        // Atlas has atlas/wiki/a on disk; list_pages reports atlas/wiki/a (dup)
        // + atlas/db/recent (new, DB-only).
        let tmp = tempfile::tempdir().unwrap();
        let atlas = tmp.path().join("atlas");
        fs::create_dir_all(atlas.join("wiki")).unwrap();
        fs::write(
            atlas.join("wiki").join("a.md"),
            "---\ntitle: On Disk\n---\nbody",
        )
        .unwrap();

        let transport = StubToolCaller::ok(vec![(
            "list_pages",
            list_envelope(serde_json::json!([
                { "slug": "atlas/wiki/a", "title": "From DB", "updated_at": "2026-05-25T00:00:00Z" },
                { "slug": "atlas/db/recent", "title": "Recent DB-only", "updated_at": "2026-05-26T00:00:00Z" }
            ])),
        )]);
        let mut pages = build_page_set(&atlas, &transport).await;
        pages.sort_by(|a, b| a.slug.cmp(&b.slug));
        assert_eq!(
            pages.len(),
            2,
            "atlas/wiki/a deduped (gbrain + walk both atlas-prefixed), atlas/db/recent added"
        );
        let a = pages.iter().find(|p| p.slug == "atlas/wiki/a").unwrap();
        assert_eq!(
            a.title, "On Disk",
            "walk (disk) entry wins on slug conflict"
        );
        assert!(pages.iter().any(|p| p.slug == "atlas/db/recent"));
    }

    // ── TTL cache (Task 2) ────────────────────────────────────────

    #[tokio::test]
    async fn cache_serves_then_rebuilds_after_invalidate() {
        let tmp = tempfile::tempdir().unwrap();
        let atlas = tmp.path().join("atlas");
        fs::create_dir_all(&atlas).unwrap();
        fs::write(atlas.join("one.md"), "# One").unwrap();

        // list_pages returns empty so the set == the walk.
        let transport =
            StubToolCaller::ok(vec![("list_pages", list_envelope(serde_json::json!([])))]);
        let index = PageIndex::new();

        let first = index.page_set(&atlas, &transport).await;
        assert_eq!(first.len(), 1);

        // Add a file on disk; cache still serves the stale (1-entry) set.
        fs::write(atlas.join("two.md"), "# Two").unwrap();
        let cached = index.page_set(&atlas, &transport).await;
        assert_eq!(cached.len(), 1, "within TTL the cache serves the stale set");

        // After invalidate the next call rebuilds and sees both.
        index.invalidate().await;
        let rebuilt = index.page_set(&atlas, &transport).await;
        assert_eq!(rebuilt.len(), 2, "invalidate forces a rebuild");
    }

    #[tokio::test]
    async fn cache_rebuilds_after_ttl_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let atlas = tmp.path().join("atlas");
        fs::create_dir_all(&atlas).unwrap();
        fs::write(atlas.join("one.md"), "# One").unwrap();
        let transport =
            StubToolCaller::ok(vec![("list_pages", list_envelope(serde_json::json!([])))]);
        let index = PageIndex::with_ttl(Duration::from_millis(0)); // everything is instantly stale

        let _ = index.page_set(&atlas, &transport).await;
        fs::write(atlas.join("two.md"), "# Two").unwrap();
        let rebuilt = index.page_set(&atlas, &transport).await;
        assert_eq!(rebuilt.len(), 2, "expired TTL forces a rebuild");
    }
}
