//! BrainEngine implementation backed by an external gbrain serve subprocess.
//!
//! See M3-1 supervisor for spawn lifecycle. This engine just translates
//! BrainEngine trait calls into gbrain MCP `tools/call` requests via the
//! [`McpToolCaller`] abstraction. The supervisor is the production
//! implementation of that trait; tests can substitute a stub.
//!
//! ## Tool mapping (verified against `spike/notes/gbrain-tools-list.json`)
//!
//! | BrainEngine method | gbrain tool   | gbrain args                                   |
//! |--------------------|---------------|-----------------------------------------------|
//! | `search`           | `search`      | `{ query, limit?, offset? }`                  |
//! | `put`              | `put_page`    | `{ slug, content }` (full markdown)           |
//! | `get`              | `get_page`    | `{ slug }`                                    |
//! | `list`             | `list_pages`  | `{ limit?, sort?, tag?, type?, updated_after? }` (no `dir` — slug-prefix filtering happens client-side) |
//! | `delete`           | `delete_page` | `{ slug }`                                    |
//! | `sync`             | `sync_brain`  | `{}` (full re-sync with default knobs)        |
//!
//! Snapshot / source-management methods (`export_snapshot`,
//! `import_snapshot`, `add_source`, `remove_source`, `list_sources`)
//! return [`BrainError::NotImplemented`] because v1 scope leaves them
//! for M5 (sharing). gbrain DOES expose `sources_add`/`sources_list`/
//! `sources_remove`, but mapping those to nevoflux's `SourceSpec` /
//! `ImportTrust` model needs a deeper design pass than M3 covers.

use std::sync::Arc;

use async_trait::async_trait;
use nevoflux_brain::{
    BrainEngine, BrainError, BrainPage, BrainResult, Hit, ImportOpts, ImportReport, NbrainBundle,
    PageMeta, PutResult, SearchOpts, Selection, SourceMeta, SourceSpec, StripRules, SyncReport,
};
use serde_json::{json, Value};
use tracing::debug;

use super::supervisor::McpToolCaller;

/// Reduce a source name to a slug-safe segment (lowercase alnum + dashes).
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = s.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "source".into()
    } else {
        trimmed
    }
}

/// BrainEngine implementation that dispatches every operation to a gbrain
/// MCP server via an [`McpToolCaller`].
///
/// Production wiring uses [`super::GbrainSupervisor`] as the transport.
/// Tests pass an in-memory stub.
pub struct GbrainEngine {
    transport: Arc<dyn McpToolCaller>,
}

impl GbrainEngine {
    /// Wrap any [`McpToolCaller`] (typically a
    /// [`super::GbrainSupervisor`]) as a [`BrainEngine`].
    pub fn new(transport: Arc<dyn McpToolCaller>) -> Self {
        Self { transport }
    }

    /// Dispatch a `tools/call` and return the inner result envelope.
    /// Errors get mapped to [`BrainError::Backend`].
    async fn call_tool(&self, name: &str, args: Value) -> BrainResult<Value> {
        debug!(tool = name, args = %args, "gbrain tools/call");
        self.transport
            .call_tool_dyn(name, args)
            .await
            .map_err(|e| BrainError::Backend(format!("gbrain {name} failed: {e}")))
    }

    /// Extract `result.content[0].text` from a gbrain `tools/call`
    /// response. MCP's CallToolResult shape is
    /// `{ content: [{ type: "text", text: ... }], isError? }` — for the
    /// pages-related tools gbrain consistently uses a single text block.
    fn extract_text_result(resp: &Value) -> BrainResult<String> {
        let result = resp
            .get("result")
            .ok_or_else(|| BrainError::Backend(format!("missing result in: {resp}")))?;
        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(BrainError::Backend(format!(
                "gbrain reported isError=true: {result}"
            )));
        }
        result
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
            .map(String::from)
            .ok_or_else(|| {
                BrainError::Backend(format!("expected result.content[0].text in: {resp}"))
            })
    }

    /// Extract a JSON payload from a gbrain `tools/call` response. The
    /// text block contains a JSON-encoded string for most structured
    /// tools (list_pages, search, etc.) — parse it.
    fn extract_json_result(resp: &Value) -> BrainResult<Value> {
        let text = Self::extract_text_result(resp)?;
        serde_json::from_str(&text).map_err(|e| {
            BrainError::Backend(format!(
                "could not parse tools/call text as JSON: {e}; raw={text:?}"
            ))
        })
    }

    /// Map a gbrain page entry (from list_pages or get_page) into a
    /// [`PageMeta`]. Handles missing fields gracefully — gbrain's exact
    /// shape varies between tools and we keep this tolerant.
    fn meta_from_entry(item: &Value) -> PageMeta {
        let slug = item
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(&slug)
            .to_string();
        let updated_at = item
            .get("updated_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let source = item
            .get("source")
            .and_then(|v| v.as_str())
            .map(String::from);
        PageMeta {
            slug,
            title,
            updated_at,
            source,
        }
    }
}

#[async_trait]
impl BrainEngine for GbrainEngine {
    async fn search(&self, query: &str, opts: SearchOpts) -> BrainResult<Vec<Hit>> {
        // gbrain's `search` tool args: { query, limit?, offset? }.
        // `SearchOpts.filter` doesn't have a direct gbrain `search`
        // equivalent — gbrain's `query` tool has richer filters but is
        // explicitly LLM-billing; we stay on `search` (keyword FTS) for
        // v1 and drop the filter knob with a warning.
        let mut args = json!({ "query": query });
        if opts.top_k > 0 {
            args["limit"] = json!(opts.top_k);
        }
        if opts.filter.is_some() {
            tracing::warn!(
                "SearchOpts.filter is ignored by GbrainEngine v1; use BrainEngine::list for prefix filtering"
            );
        }
        let resp = self.call_tool("search", args).await?;
        let payload = Self::extract_json_result(&resp)?;
        // gbrain's `search` returns either an array directly or a
        // `{ results: [...] }` wrapper depending on version. Accept both.
        let array = payload
            .as_array()
            .cloned()
            .or_else(|| payload.get("results").and_then(|r| r.as_array()).cloned())
            .ok_or_else(|| BrainError::Backend(format!("expected hits array in: {payload}")))?;
        let mut hits = Vec::with_capacity(array.len());
        for h in &array {
            let page_meta = Self::meta_from_entry(h);
            let score = h.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let snippet = h.get("snippet").and_then(|v| v.as_str()).map(String::from);
            hits.push(Hit {
                page_meta,
                score,
                snippet,
            });
        }
        Ok(hits)
    }

    async fn put(&self, page: BrainPage) -> BrainResult<PutResult> {
        // gbrain's `put_page` requires `{ slug, content }` where
        // `content` is "Full markdown content with YAML frontmatter".
        // We serialize compiled_truth + "\n---\n" + timeline as the
        // body. M3 stays minimal here; richer frontmatter rendering
        // lands later with M4's manual-save flow.
        let content = if page.timeline.is_empty() {
            page.compiled_truth.clone()
        } else {
            format!("{}\n---\n{}", page.compiled_truth, page.timeline)
        };
        let args = json!({
            "slug": page.slug,
            "content": content,
        });
        let resp = self.call_tool("put_page", args).await?;
        // gbrain's put_page reply isn't strongly typed; the spike
        // captured it as a brief textual ack. Treat any non-error
        // response as "updated"; `created` cannot be cheaply inferred
        // without a prior get_page round-trip.
        let _ = Self::extract_text_result(&resp)?;
        Ok(PutResult {
            slug: page.slug,
            created: false,
            updated: true,
        })
    }

    async fn get(&self, slug: &str) -> BrainResult<BrainPage> {
        let args = json!({ "slug": slug });
        let resp = self.call_tool("get_page", args).await?;
        // gbrain's `get_page` returns either a JSON object with the
        // page fields (preferred) or a raw markdown string. Try JSON
        // first; fall back to markdown.
        let text = Self::extract_text_result(&resp)?;
        if let Ok(obj) = serde_json::from_str::<Value>(&text) {
            // Structured form — pull slug/title/content out.
            let resolved_slug = obj
                .get("slug")
                .and_then(|v| v.as_str())
                .unwrap_or(slug)
                .to_string();
            let content = obj
                .get("content")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| text.clone());
            let mut page = BrainPage::from_markdown(resolved_slug, content);
            if let Some(title) = obj.get("title").and_then(|v| v.as_str()) {
                page.title = title.to_string();
            }
            if let Some(updated_at) = obj
                .get("updated_at")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
            {
                page.updated_at = updated_at;
            }
            Ok(page)
        } else {
            // Plain markdown — let BrainPage parse it.
            Ok(BrainPage::from_markdown(slug.to_string(), text))
        }
    }

    async fn list(&self, dir: &str) -> BrainResult<Vec<PageMeta>> {
        // gbrain's `list_pages` doesn't accept a `dir` filter; it has
        // tag/type/sort filters. We request a generous limit and
        // client-side filter on slug prefix when `dir` is non-empty.
        // This is a v1 stopgap; a richer surface lands once gbrain
        // exposes a native prefix filter.
        let args = json!({
            "limit": 200,
            "sort": "updated_desc",
        });
        let resp = self.call_tool("list_pages", args).await?;
        let payload = Self::extract_json_result(&resp)?;
        let array = payload
            .as_array()
            .cloned()
            .or_else(|| payload.get("pages").and_then(|p| p.as_array()).cloned())
            .ok_or_else(|| BrainError::Backend(format!("expected pages array in: {payload}")))?;
        let dir_prefix = dir.trim_end_matches('/');
        let mut metas = Vec::with_capacity(array.len());
        for item in &array {
            let meta = Self::meta_from_entry(item);
            if dir_prefix.is_empty()
                || meta.slug == dir_prefix
                || meta.slug.starts_with(&format!("{dir_prefix}/"))
            {
                metas.push(meta);
            }
        }
        Ok(metas)
    }

    async fn delete(&self, slug: &str) -> BrainResult<()> {
        let args = json!({ "slug": slug });
        let resp = self.call_tool("delete_page", args).await?;
        let _ = Self::extract_text_result(&resp)?;
        Ok(())
    }

    async fn sync(&self) -> BrainResult<SyncReport> {
        // gbrain's `sync_brain` accepts a handful of flags (dry_run,
        // full, no_embed, no_pull, repo); v1 dispatches with defaults
        // and treats the textual response as opaque since the user
        // really just cares the call completed.
        let resp = self.call_tool("sync_brain", json!({})).await?;
        let _ = Self::extract_text_result(&resp)?;
        Ok(SyncReport::default())
    }

    // ---- M5 sharing scope — not implemented in v1. ----

    async fn export_snapshot(
        &self,
        sel: Selection,
        rules: StripRules,
    ) -> BrainResult<NbrainBundle> {
        use crate::brain_share::{manifest as mf, pack::Entry, seal, strip, SealMode};

        // 1. Resolve the selection to a slug list.
        let mut slugs: Vec<String> = match sel {
            Selection::Files(f) => f,
            Selection::Directory(d) => self.list(&d).await?.into_iter().map(|m| m.slug).collect(),
            Selection::Mixed { files, directories } => {
                let mut all = files;
                for d in directories {
                    all.extend(self.list(&d).await?.into_iter().map(|m| m.slug));
                }
                all
            }
        };
        slugs.retain(|s| !strip::is_excluded(s, &rules)); // invariant A.3
        slugs.sort();
        slugs.dedup();

        // 2. Pull each page, strip, build file entries + manifest files.
        let mut files = Vec::new();
        let mut manifest_files = Vec::new();
        for slug in &slugs {
            let page = match self.get(slug).await {
                Ok(p) => p,
                Err(BrainError::NotFound(_)) => continue, // skip missing
                Err(e) => return Err(e),
            };
            let body = strip::strip_page(&page, &rules);
            let path = format!("{slug}.md");
            let bytes = body.into_bytes();
            manifest_files.push(crate::brain_share::file_entry(&path, &bytes));
            files.push(Entry { path, bytes });
        }

        // 3. Build manifest.
        let manifest = mf::Manifest {
            format: mf::FORMAT.into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            sender: mf::Sender {
                fingerprint: None,
                display_name: String::new(),
                signature: None,
            },
            files: manifest_files,
            strip_rules_applied: mf::StripRulesApplied {
                compiled_only: rules.compiled_only,
                frontmatter_whitelist: rules.frontmatter_whitelist.clone(),
                frontmatter_redacted: rules.frontmatter_redacted.clone(),
                raw_excluded: rules.raw_excluded,
                directories_excluded: rules.directories_excluded.clone(),
            },
            title: String::new(),
            description: String::new(),
            tags: Vec::new(),
            expires_at: None,
        };

        // 4. Seal (random-key zero-knowledge default).
        let (artifact, key) = seal(&manifest, &files, SealMode::RandomKey)?;
        Ok(NbrainBundle { artifact, key })
    }

    async fn import_snapshot(
        &self,
        bundle: NbrainBundle,
        opts: ImportOpts,
    ) -> BrainResult<ImportReport> {
        use nevoflux_brain::ImportTrust;

        // 1. Open + verify the artifact.
        let (_manifest, files) = crate::brain_share::open(&bundle.artifact, &opts.unlock)?;

        // 2. Determine the landing slug prefix.
        //    ReadOnly (default) → isolated namespace; FullMerge → main tree.
        let prefix = match opts.trust {
            ImportTrust::ReadOnly => format!("imported/{}/", sanitize(&opts.source_name)),
            ImportTrust::FullMerge => String::new(),
        };

        // 3. Replay each file via put_page; record slug conflicts (FullMerge).
        let mut report = ImportReport::default();
        for f in files {
            let rel_slug = f.path.strip_suffix(".md").unwrap_or(&f.path);
            let slug = format!("{prefix}{rel_slug}");
            if matches!(opts.trust, ImportTrust::FullMerge) {
                // Conflict = a page already exists at this slug.
                if self.get(&slug).await.is_ok() {
                    report.conflicts.push(slug.clone());
                    continue;
                }
            }
            let content = String::from_utf8_lossy(&f.bytes).into_owned();
            let page = BrainPage::from_markdown(slug, content);
            self.put(page).await?;
            report.files_imported += 1;
        }

        // 4. Reindex.
        self.sync().await?;
        Ok(report)
    }

    async fn add_source(&self, _src: SourceSpec) -> BrainResult<()> {
        // TODO(M5): wire to gbrain `sources_add`. gbrain expects an
        // `id` (immutable citation key) which our SourceSpec doesn't
        // model yet, so the mapping needs a design pass.
        Err(BrainError::NotImplemented)
    }

    async fn remove_source(&self, _name: &str) -> BrainResult<()> {
        // TODO(M5): wire to gbrain `sources_remove`.
        Err(BrainError::NotImplemented)
    }

    async fn list_sources(&self) -> BrainResult<Vec<SourceMeta>> {
        // TODO(M5): wire to gbrain `sources_list`.
        Err(BrainError::NotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use nevoflux_brain::{
        BrainEngine, BrainError, BrainPage, ImportOpts, ImportTrust, SearchOpts, Selection,
        StripRules,
    };
    use serde_json::{json, Value};
    use tokio::sync::Mutex;

    use super::super::supervisor::McpToolCaller;
    use super::GbrainEngine;

    /// In-memory transport for tests: records every call and returns
    /// a canned response by tool name.
    struct StubToolCaller {
        responses: Mutex<HashMap<String, Value>>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl StubToolCaller {
        fn new(responses: Vec<(&'static str, Value)>) -> Self {
            let mut map = HashMap::new();
            for (name, resp) in responses {
                map.insert(name.to_string(), resp);
            }
            Self {
                responses: Mutex::new(map),
                calls: Mutex::new(Vec::new()),
            }
        }

        async fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl McpToolCaller for StubToolCaller {
        async fn call_tool_dyn(
            &self,
            name: &str,
            arguments: Value,
        ) -> Result<Value, super::super::supervisor::McpToolCallerError> {
            self.calls.lock().await.push((name.to_string(), arguments));
            match self.responses.lock().await.get(name).cloned() {
                Some(v) => Ok(v),
                None => Err(format!("no stub response configured for {name}").into()),
            }
        }
    }

    /// Wrap a list of result-payload entries in the MCP CallToolResult
    /// shape gbrain actually returns: `{ id, jsonrpc, result: { content:
    /// [{ type: "text", text: <JSON-encoded payload> }] } }`.
    fn wrap_text(text: &str) -> Value {
        json!({
            "id": 1,
            "jsonrpc": "2.0",
            "result": {
                "content": [{
                    "type": "text",
                    "text": text,
                }]
            }
        })
    }

    fn wrap_json(payload: Value) -> Value {
        wrap_text(&payload.to_string())
    }

    #[tokio::test]
    async fn search_translates_response_to_hits() {
        let stub = Arc::new(StubToolCaller::new(vec![(
            "search",
            wrap_json(json!([
                {
                    "slug": "x",
                    "title": "X",
                    "score": 0.9,
                    "snippet": "snip-x",
                    "updated_at": "2026-05-25T00:00:00Z"
                },
                {
                    "slug": "y",
                    "title": "Y",
                    "score": 0.7,
                    "updated_at": "2026-05-24T00:00:00Z"
                }
            ])),
        )]));
        let engine = GbrainEngine::new(stub.clone());
        let hits = engine.search("query", SearchOpts::default()).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].page_meta.slug, "x");
        assert_eq!(hits[0].score, 0.9);
        assert_eq!(hits[0].snippet.as_deref(), Some("snip-x"));
        assert_eq!(hits[1].page_meta.slug, "y");
        assert!(hits[1].snippet.is_none());

        let calls = stub.calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "search");
        assert_eq!(calls[0].1.get("query").unwrap().as_str().unwrap(), "query");
        assert_eq!(calls[0].1.get("limit").unwrap().as_i64().unwrap(), 10);
    }

    #[tokio::test]
    async fn search_handles_results_wrapped_shape() {
        // Some gbrain versions wrap hits in `{ results: [...] }` — make
        // sure the engine tolerates both shapes.
        let stub = Arc::new(StubToolCaller::new(vec![(
            "search",
            wrap_json(json!({
                "results": [
                    { "slug": "wrapped", "title": "W", "score": 0.5 }
                ]
            })),
        )]));
        let engine = GbrainEngine::new(stub);
        let hits = engine.search("q", SearchOpts::default()).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].page_meta.slug, "wrapped");
    }

    #[tokio::test]
    async fn get_extracts_markdown_when_response_is_plain_text() {
        let stub = Arc::new(StubToolCaller::new(vec![(
            "get_page",
            wrap_text("compiled section\n---\ntimeline section"),
        )]));
        let engine = GbrainEngine::new(stub);
        let page = engine.get("note").await.unwrap();
        assert_eq!(page.slug, "note");
        assert!(page.compiled_truth.contains("compiled section"));
        assert!(page.timeline.contains("timeline section"));
    }

    #[tokio::test]
    async fn get_extracts_markdown_when_response_is_structured_json() {
        let stub = Arc::new(StubToolCaller::new(vec![(
            "get_page",
            wrap_json(json!({
                "slug": "note",
                "title": "Note Title",
                "content": "body\n---\nhistory",
                "updated_at": "2026-05-20T12:00:00Z"
            })),
        )]));
        let engine = GbrainEngine::new(stub);
        let page = engine.get("note").await.unwrap();
        assert_eq!(page.slug, "note");
        assert_eq!(page.title, "Note Title");
        assert!(page.compiled_truth.contains("body"));
        assert!(page.timeline.contains("history"));
    }

    #[tokio::test]
    async fn put_sends_slug_and_content_with_separator() {
        let stub = Arc::new(StubToolCaller::new(vec![("put_page", wrap_text("ok"))]));
        let engine = GbrainEngine::new(stub.clone());
        let page = BrainPage {
            slug: "test".into(),
            title: "Test".into(),
            compiled_truth: "compiled body".into(),
            timeline: "timeline body".into(),
            frontmatter: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let result = engine.put(page).await.unwrap();
        assert_eq!(result.slug, "test");
        assert!(result.updated);

        let calls = stub.calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "put_page");
        let content = calls[0].1.get("content").unwrap().as_str().unwrap();
        assert!(content.contains("compiled body"));
        assert!(content.contains("---"));
        assert!(content.contains("timeline body"));
    }

    #[tokio::test]
    async fn put_with_empty_timeline_omits_separator() {
        let stub = Arc::new(StubToolCaller::new(vec![("put_page", wrap_text("ok"))]));
        let engine = GbrainEngine::new(stub.clone());
        let page = BrainPage {
            slug: "test".into(),
            title: "Test".into(),
            compiled_truth: "just one section".into(),
            timeline: String::new(),
            frontmatter: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        engine.put(page).await.unwrap();
        let calls = stub.calls().await;
        let content = calls[0].1.get("content").unwrap().as_str().unwrap();
        assert_eq!(content, "just one section");
        assert!(!content.contains("---"));
    }

    #[tokio::test]
    async fn list_translates_response_and_filters_by_dir_prefix() {
        let stub = Arc::new(StubToolCaller::new(vec![(
            "list_pages",
            wrap_json(json!([
                { "slug": "wiki/people/alice", "title": "Alice", "updated_at": "2026-05-25T00:00:00Z" },
                { "slug": "wiki/people/bob",   "title": "Bob",   "updated_at": "2026-05-24T00:00:00Z" },
                { "slug": "daily/2026-05-25",  "title": "Daily", "updated_at": "2026-05-25T00:00:00Z" }
            ])),
        )]));
        let engine = GbrainEngine::new(stub.clone());
        let metas = engine.list("wiki/people").await.unwrap();
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().all(|m| m.slug.starts_with("wiki/people/")));

        // Empty dir returns everything.
        let all = engine.list("").await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn delete_calls_delete_page_with_slug() {
        let stub = Arc::new(StubToolCaller::new(vec![("delete_page", wrap_text("ok"))]));
        let engine = GbrainEngine::new(stub.clone());
        engine.delete("doomed").await.unwrap();
        let calls = stub.calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "delete_page");
        assert_eq!(calls[0].1.get("slug").unwrap().as_str().unwrap(), "doomed");
    }

    #[tokio::test]
    async fn sync_calls_sync_brain_with_no_args() {
        let stub = Arc::new(StubToolCaller::new(vec![("sync_brain", wrap_text("ok"))]));
        let engine = GbrainEngine::new(stub.clone());
        let report = engine.sync().await.unwrap();
        assert_eq!(report.added, 0);
        assert_eq!(report.updated, 0);
        assert_eq!(report.deleted, 0);
        let calls = stub.calls().await;
        assert_eq!(calls[0].0, "sync_brain");
        assert_eq!(calls[0].1, json!({}));
    }

    #[tokio::test]
    async fn snapshot_and_source_methods_return_not_implemented() {
        let stub = Arc::new(StubToolCaller::new(vec![]));
        let engine = GbrainEngine::new(stub);

        let export = engine
            .export_snapshot(Selection::Files(vec![]), StripRules::default())
            .await;
        assert!(matches!(export, Err(BrainError::NotImplemented)));

        let import = engine
            .import_snapshot(
                nevoflux_brain::NbrainBundle {
                    artifact: vec![],
                    key: None,
                },
                ImportOpts {
                    source_name: "x".into(),
                    trust: ImportTrust::ReadOnly,
                    unlock: nevoflux_brain::Unlock::Key([0u8; 32]),
                },
            )
            .await;
        assert!(matches!(import, Err(BrainError::NotImplemented)));

        let add = engine
            .add_source(nevoflux_brain::SourceSpec {
                name: "x".into(),
                directory: "/tmp/x".into(),
                trust: ImportTrust::ReadOnly,
            })
            .await;
        assert!(matches!(add, Err(BrainError::NotImplemented)));

        let remove = engine.remove_source("x").await;
        assert!(matches!(remove, Err(BrainError::NotImplemented)));

        let list = engine.list_sources().await;
        assert!(matches!(list, Err(BrainError::NotImplemented)));
    }

    #[tokio::test]
    async fn backend_error_surfaces_when_transport_fails() {
        let stub = Arc::new(StubToolCaller::new(vec![])); // no stub
        let engine = GbrainEngine::new(stub);
        let result = engine.get("x").await;
        match result {
            Err(BrainError::Backend(msg)) => {
                assert!(
                    msg.contains("get_page"),
                    "expected tool name in error: {msg}"
                );
            }
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn is_error_response_surfaces_as_backend_error() {
        let stub = Arc::new(StubToolCaller::new(vec![(
            "get_page",
            json!({
                "id": 1,
                "jsonrpc": "2.0",
                "result": {
                    "isError": true,
                    "content": [{ "type": "text", "text": "page not found" }]
                }
            }),
        )]));
        let engine = GbrainEngine::new(stub);
        let result = engine.get("missing").await;
        assert!(matches!(result, Err(BrainError::Backend(_))));
    }
}

#[cfg(test)]
mod export_import_tests {
    use super::*;
    use crate::gbrain::supervisor::{McpToolCaller, McpToolCallerError};
    use nevoflux_brain::{ImportOpts, ImportTrust, Selection, StripRules, Unlock};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A stub gbrain that answers get_page/list_pages and records put_page +
    /// sync_brain calls.
    struct StubGbrain {
        pages: HashMap<String, String>,     // slug -> markdown content
        puts: Mutex<Vec<(String, String)>>, // (slug, content)
        syncs: Mutex<u32>,
    }

    fn text_envelope(text: &str) -> serde_json::Value {
        json!({ "result": { "content": [{ "type": "text", "text": text }] } })
    }

    #[async_trait]
    impl McpToolCaller for StubGbrain {
        async fn call_tool_dyn(
            &self,
            name: &str,
            arguments: Value,
        ) -> Result<Value, McpToolCallerError> {
            match name {
                "get_page" => {
                    let slug = arguments["slug"].as_str().unwrap_or_default();
                    match self.pages.get(slug) {
                        Some(content) => Ok(text_envelope(content)),
                        None => Ok(
                            json!({ "result": { "isError": true, "content": [{ "type": "text", "text": "not found" }] } }),
                        ),
                    }
                }
                "list_pages" => {
                    let arr: Vec<Value> = self
                        .pages
                        .keys()
                        .map(|s| json!({ "slug": s, "title": s }))
                        .collect();
                    Ok(text_envelope(&serde_json::to_string(&arr).unwrap()))
                }
                "put_page" => {
                    let slug = arguments["slug"].as_str().unwrap_or_default().to_string();
                    let content = arguments["content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    self.puts.lock().unwrap().push((slug, content));
                    Ok(text_envelope("ok"))
                }
                "sync_brain" => {
                    *self.syncs.lock().unwrap() += 1;
                    Ok(text_envelope("synced"))
                }
                other => Ok(text_envelope(&format!("unhandled {other}"))),
            }
        }
    }

    fn engine_with(pages: &[(&str, &str)]) -> GbrainEngine {
        let pages = pages
            .iter()
            .map(|(s, c)| (s.to_string(), c.to_string()))
            .collect();
        GbrainEngine::new(Arc::new(StubGbrain {
            pages,
            puts: Mutex::new(Vec::new()),
            syncs: Mutex::new(0),
        }))
    }

    #[tokio::test]
    async fn export_produces_openable_artifact() {
        let engine = engine_with(&[("concepts/yc", "# YC\nStartup accelerator.")]);
        let bundle = engine
            .export_snapshot(
                Selection::Files(vec!["concepts/yc".into()]),
                StripRules::default(),
            )
            .await
            .unwrap();
        let key = bundle.key.expect("random-key mode");
        let (m, files) = crate::brain_share::open(&bundle.artifact, &Unlock::Key(key)).unwrap();
        assert_eq!(m.format, "nbrain/1");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "concepts/yc.md");
        assert!(String::from_utf8_lossy(&files[0].bytes).contains("Startup accelerator"));
    }

    #[tokio::test]
    async fn export_then_import_roundtrip() {
        // Export from one engine.
        let src = engine_with(&[
            ("concepts/yc", "# YC\nAccelerator."),
            ("people/pg", "# PG\nFounder."),
        ]);
        let bundle = src
            .export_snapshot(
                Selection::Directory(String::new()), // all pages
                StripRules::default(),
            )
            .await
            .unwrap();
        let key = bundle.key.unwrap();

        // Import into a fresh (empty) engine.
        let dst_stub = Arc::new(StubGbrain {
            pages: HashMap::new(),
            puts: Mutex::new(Vec::new()),
            syncs: Mutex::new(0),
        });
        let dst = GbrainEngine::new(dst_stub.clone());

        let report = dst
            .import_snapshot(
                NbrainBundle {
                    artifact: bundle.artifact,
                    key: None,
                },
                ImportOpts {
                    source_name: "Alice".into(),
                    trust: ImportTrust::ReadOnly,
                    unlock: Unlock::Key(key),
                },
            )
            .await
            .unwrap();

        assert_eq!(report.files_imported, 2);
        assert!(report.conflicts.is_empty());

        // Files landed under the isolated namespace and sync ran.
        let puts = dst_stub.puts.lock().unwrap();
        assert_eq!(puts.len(), 2);
        assert!(puts
            .iter()
            .all(|(slug, _)| slug.starts_with("imported/alice/")));
        assert_eq!(*dst_stub.syncs.lock().unwrap(), 1);
    }
}
