//! Browser-facing `brain.*` RPC handlers for the M4-4 `nevoflux://brain`
//! page (and any other consumer that wants direct, non-agent-path access
//! to the knowledge base).
//!
//! Each handler:
//!   1. Reads the active [`crate::init_brain::SharedBrainSlot`] via
//!      [`crate::init_brain::CURRENT_BRAIN_SLOT`].
//!   2. If the slot is empty (`brain.enabled = false` OR the boot path
//!      hasn't run yet), returns `BRAIN_DISABLED` (or `ok=false` for
//!      `brain.health`, so the UI can render a disabled state without an
//!      error toast).
//!   3. Otherwise dispatches to the [`nevoflux_brain::BrainEngine`] trait
//!      method (or, for `brain.stats`, to the gbrain `get_stats` MCP tool
//!      via the live supervisor) and wraps the result in the
//!      `system_response` envelope.
//!
//! These handlers complement (but do not replace) the M3-4 brain tools
//! dispatched on the agent path; they are the minimum surface the
//! M4-4b `nevoflux://brain` page needs.

use std::sync::Arc;
use std::sync::OnceLock;

use nevoflux_brain::{BrainEngine, BrainError};
use serde_json::Value;

use crate::gbrain::page_index::{ListQuery, PageIndex, SortOrder};
use crate::gbrain::supervisor::McpToolCaller;
use crate::gbrain::GbrainSupervisor;
use crate::init_brain::CURRENT_BRAIN_SLOT;
use crate::kb_wizard::{err_response, ok_response};

/// Process-global KB page index cache. Lazily created; persists across
/// `brain.list` calls so a large atlas isn't re-walked on every page flip.
/// Invalidated by the mutating brain handlers (put / save_*).
fn page_index() -> &'static PageIndex {
    static INDEX: OnceLock<PageIndex> = OnceLock::new();
    INDEX.get_or_init(PageIndex::new)
}

/// Public so the put/save handlers can force-refresh the list after a write.
pub(crate) async fn invalidate_page_index() {
    page_index().invalidate().await;
}

/// Look up the live brain engine handle. Returns `None` when the brain
/// slot is empty (either not initialized yet or
/// `knowledge_base.brain.enabled = false`).
async fn current_engine() -> Option<Arc<dyn BrainEngine>> {
    let slot = CURRENT_BRAIN_SLOT.get()?;
    let guard = slot.read().await;
    guard.as_ref().map(|boot| boot.engine.clone())
}

/// Look up the live supervisor handle. Used by `brain.stats` to dispatch
/// gbrain MCP tools that the [`BrainEngine`] trait doesn't expose
/// directly.
async fn current_supervisor() -> Option<Arc<GbrainSupervisor>> {
    let slot = CURRENT_BRAIN_SLOT.get()?;
    let guard = slot.read().await;
    guard.as_ref().map(|boot| boot.supervisor.clone())
}

/// Standard error envelope returned by `brain.stats` / `brain.list` /
/// `brain.get` when the brain slot is empty.
fn brain_disabled_err(request_id: &str, command: &str) -> Value {
    err_response(
        request_id,
        command,
        "BRAIN_DISABLED",
        "knowledge_base.brain is not enabled or the daemon hasn't \
         finished initializing the brain yet",
    )
}

/// `brain.health` — one-shot readiness probe.
///
/// Returns `ok=true` when the brain slot holds a live engine; `ok=false`
/// (still a *success* envelope) when the slot is empty so the UI can
/// render a "disabled" badge instead of an error toast.
pub async fn handle_health(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let Some(_engine) = current_engine().await else {
        return ok_response(
            request_id,
            "brain.health",
            serde_json::json!({
                "ok": false,
                "brain_dir": null,
                "reason": "brain_disabled",
                "last_check_ts": chrono::Utc::now().timestamp(),
            }),
        );
    };

    // Engine is live. We don't ping it here (a real probe would race
    // with the supervisor's restart watchdog); presence-in-slot is
    // sufficient signal for the M4-4 UI.
    let brain_dir = dirs::home_dir()
        .map(|h| h.join(".gbrain").display().to_string())
        .unwrap_or_default();

    ok_response(
        request_id,
        "brain.health",
        serde_json::json!({
            "ok": true,
            "brain_dir": brain_dir,
            "last_check_ts": chrono::Utc::now().timestamp(),
        }),
    )
}

/// `brain.stats` — page / chunk / embedded counts.
///
/// The [`BrainEngine`] trait doesn't expose a dedicated stats method, so
/// this handler reaches into the supervisor and calls gbrain's
/// `get_stats` MCP tool directly. The tool returns a JSON-stringified
/// payload inside `result.content[0].text`; we parse it back to a JSON
/// object before returning to the browser.
pub async fn handle_stats(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let Some(supervisor) = current_supervisor().await else {
        return brain_disabled_err(request_id, "brain.stats");
    };

    let result = supervisor
        .call_tool("get_stats", serde_json::json!({}))
        .await;

    match result {
        Ok(envelope) => {
            // gbrain returns the MCP `tools/call` shape:
            // { result: { content: [{ type: "text", text: "<json>" }], ... } }
            // where `text` is a JSON-stringified stats object. We parse
            // it back so the browser sees structured data instead of a
            // string.
            let stats = envelope
                .get("result")
                .and_then(|r| r.get("content"))
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|first| {
                    first.get("data").cloned().or_else(|| {
                        first
                            .get("text")
                            .and_then(|t| t.as_str())
                            .and_then(|s| serde_json::from_str(s).ok())
                    })
                })
                .unwrap_or_else(|| serde_json::json!({}));
            ok_response(request_id, "brain.stats", stats)
        }
        Err(e) => err_response(
            request_id,
            "brain.stats",
            "BRAIN_BACKEND_ERROR",
            format!("get_stats failed: {e}"),
        ),
    }
}

/// `brain.list` — paginated + filtered page metadata for the browse list and
/// the share dialog.
///
/// Params (all optional): `dir` (slug-prefix), `q` (case-insensitive substring
/// on slug OR title), `sort` (`updated_desc` default | `updated_asc` | `slug`),
/// `offset` (default 0), `limit` (default 50, capped 200). Returns
/// `{ pages, total, offset, limit }` where `total` is the post-filter,
/// pre-slice count (drives the page-count UI). Backward-compatible: omitting
/// every new param yields the first 50 sorted by updated_desc.
///
/// Pages come from the daemon's own complete page index (atlas walk + a gbrain
/// `list_pages` <=100 cross-check), NOT `BrainEngine::list` — gbrain caps
/// `list_pages` at 100 with no offset, which cannot drive real pagination.
pub async fn handle_list(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let dir = params
        .get("dir")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let q = params
        .get("q")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sort = SortOrder::parse(params.get("sort").and_then(|v| v.as_str()));
    // JSON numbers arrive as u64/i64/f64; coerce defensively, clamp negatives
    // to 0. limit==0/absent -> default; >200 -> capped (in ListQuery::clamp).
    let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(ListQuery::DEFAULT_LIMIT);
    let query = ListQuery {
        dir,
        q,
        sort,
        offset,
        limit,
    }
    .clamp();

    // We need the supervisor (for the gbrain cross-check transport + the
    // brain_dir to walk). Empty slot -> disabled.
    let Some(supervisor) = current_supervisor().await else {
        return brain_disabled_err(request_id, "brain.list");
    };
    let atlas_dir = supervisor.brain_dir().join("atlas");
    let transport: Arc<dyn McpToolCaller> = supervisor.clone();

    let slice = page_index().query(&atlas_dir, &transport, &query).await;

    ok_response(
        request_id,
        "brain.list",
        serde_json::json!({
            "pages": slice.pages,
            "total": slice.total,
            "offset": slice.offset,
            "limit": slice.limit,
        }),
    )
}

/// `brain.get` — fetch a single page's compiled markdown by slug.
pub async fn handle_get(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let slug = match params.get("slug").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => {
            return err_response(
                request_id,
                "brain.get",
                "BAD_REQUEST",
                "missing or empty `slug`",
            );
        }
    };

    let Some(engine) = current_engine().await else {
        return brain_disabled_err(request_id, "brain.get");
    };

    match engine.get(slug).await {
        Ok(page) => ok_response(
            request_id,
            "brain.get",
            serde_json::to_value(&page).unwrap_or_else(|_| serde_json::json!({})),
        ),
        Err(BrainError::NotFound(_)) => err_response(
            request_id,
            "brain.get",
            "BRAIN_NOT_FOUND",
            format!("page `{slug}` not found"),
        ),
        Err(e) => err_response(
            request_id,
            "brain.get",
            "BRAIN_BACKEND_ERROR",
            format!("get failed: {e}"),
        ),
    }
}

/// Derive a URL-safe slug from a title (or fallback string).
fn derive_slug(title: &str, fallback: &str) -> String {
    let base = if title.trim().is_empty() {
        fallback
    } else {
        title
    };
    let mut slug: String = base
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    // collapse repeated dashes + trim
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug
    }
}

/// `brain.put` — low-level direct put.
///
/// Request:  `{ slug, markdown }` (markdown = full page body; compiled_truth
/// above the first `---`, timeline below). Response: `PutResult`.
pub async fn handle_put(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let slug = match params.get("slug").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return err_response(
                request_id,
                "brain.put",
                "BAD_REQUEST",
                "missing or empty `slug`",
            );
        }
    };
    let markdown = params
        .get("markdown")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let Some(engine) = current_engine().await else {
        return brain_disabled_err(request_id, "brain.put");
    };
    let page = nevoflux_brain::BrainPage::from_markdown(slug, markdown);
    match engine.put(page).await {
        Ok(result) => {
            invalidate_page_index().await;
            ok_response(
                request_id,
                "brain.put",
                serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({})),
            )
        }
        Err(e) => err_response(
            request_id,
            "brain.put",
            "BRAIN_BACKEND_ERROR",
            format!("put failed: {e}"),
        ),
    }
}

/// `brain.save_webpage` — F2: save a web page to the KB.
///
/// Request: `{ url, title, content, directory? = "inbox", tags? }`.
/// Builds a page under `{directory}/{slug}` (slug derived from title, or the
/// URL when the title is empty) and puts it. Response: `PutResult`.
pub async fn handle_save_webpage(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let url = params.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let title = params.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let directory = params
        .get("directory")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("inbox");
    let tags: Vec<String> = params
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let Some(engine) = current_engine().await else {
        return brain_disabled_err(request_id, "brain.save_webpage");
    };

    // Fallback slug source: the URL (host+path) when the title is empty.
    let slug = derive_slug(title, url);
    let full_slug = format!("{directory}/{slug}");
    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M").to_string();
    let display_title = if title.trim().is_empty() { url } else { title };
    let tags_line = if tags.is_empty() {
        String::new()
    } else {
        format!("\nTags: {}\n", tags.join(", "))
    };
    let raw = format!(
        "# {display_title}\n\n> Saved from {url}\n{tags_line}\n{content}\n---\n{timestamp}: saved from web page",
    );

    let page = nevoflux_brain::BrainPage::from_markdown(full_slug, raw);
    match engine.put(page).await {
        Ok(result) => {
            invalidate_page_index().await;
            ok_response(
                request_id,
                "brain.save_webpage",
                serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({})),
            )
        }
        Err(e) => err_response(
            request_id,
            "brain.save_webpage",
            "BRAIN_BACKEND_ERROR",
            format!("put failed: {e}"),
        ),
    }
}

/// `brain.save_conversation` — F3: save a conversation as a concept.
///
/// Request: `{ title, content, conversation_id?, directory? = "concepts" }`.
/// Builds a page under `{directory}/{slug}` and puts it. Response:
/// `PutResult`.
pub async fn handle_save_conversation(params: &Value) -> Value {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let title = params.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let conversation_id = params
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let directory = params
        .get("directory")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("concepts");

    let Some(engine) = current_engine().await else {
        return brain_disabled_err(request_id, "brain.save_conversation");
    };

    let slug = derive_slug(title, conversation_id);
    let full_slug = format!("{directory}/{slug}");
    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M").to_string();
    let display_title = if title.trim().is_empty() {
        "Untitled conversation"
    } else {
        title
    };
    let raw = format!(
        "# {display_title}\n\n{content}\n---\n{timestamp}: saved from conversation {conversation_id}",
    );

    let page = nevoflux_brain::BrainPage::from_markdown(full_slug, raw);
    match engine.put(page).await {
        Ok(result) => {
            invalidate_page_index().await;
            ok_response(
                request_id,
                "brain.save_conversation",
                serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({})),
            )
        }
        Err(e) => err_response(
            request_id,
            "brain.save_conversation",
            "BRAIN_BACKEND_ERROR",
            format!("put failed: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: CURRENT_BRAIN_SLOT is a process-global `OnceLock`. In the
    // test binary nothing initializes it, so `current_engine()` /
    // `current_supervisor()` both return `None`. These tests verify the
    // disabled-path branches — the happy-path branches require a live
    // gbrain subprocess and are deferred to manual end-to-end smoke.

    #[tokio::test]
    async fn health_returns_disabled_when_brain_slot_empty() {
        let resp = handle_health(&serde_json::json!({ "request_id": "test-1" })).await;
        assert_eq!(resp["type"], "system_response");
        assert_eq!(resp["payload"]["success"], true);
        // ok=false reflects the brain_disabled status.
        assert_eq!(resp["payload"]["data"]["ok"], false);
        assert_eq!(resp["payload"]["data"]["reason"], "brain_disabled");
    }

    #[tokio::test]
    async fn stats_returns_error_envelope_when_brain_disabled() {
        let resp = handle_stats(&serde_json::json!({ "request_id": "test-2" })).await;
        assert_eq!(resp["type"], "system_response");
        assert_eq!(resp["payload"]["success"], false);
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[tokio::test]
    async fn list_returns_error_envelope_when_brain_disabled() {
        let resp = handle_list(&serde_json::json!({ "request_id": "test-3" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[tokio::test]
    async fn list_disabled_envelope_has_command_and_code() {
        let resp = handle_list(&serde_json::json!({
            "request_id": "t-list",
            "q": "anything",
            "sort": "slug",
            "offset": 10,
            "limit": 25
        }))
        .await;
        assert_eq!(resp["payload"]["success"], false);
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
        assert_eq!(resp["payload"]["command"], "brain.list");
    }

    #[tokio::test]
    async fn get_returns_bad_request_when_slug_missing() {
        let resp = handle_get(&serde_json::json!({ "request_id": "test-4" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BAD_REQUEST");
    }

    #[tokio::test]
    async fn get_returns_disabled_when_slug_present_but_brain_off() {
        let resp = handle_get(&serde_json::json!({
            "request_id": "test-5",
            "slug": "test-page"
        }))
        .await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[test]
    fn derive_slug_basic() {
        assert_eq!(derive_slug("Hello World!", ""), "hello-world");
        assert_eq!(derive_slug("  Rust & WASM  ", ""), "rust-wasm");
        assert_eq!(derive_slug("", "fallback-name"), "fallback-name");
        assert_eq!(derive_slug("", ""), "untitled");
        assert_eq!(derive_slug("C++ / Go", ""), "c-go");
    }

    #[tokio::test]
    async fn put_bad_request_when_slug_missing() {
        let resp = handle_put(&serde_json::json!({ "request_id": "t1", "markdown": "x" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BAD_REQUEST");
    }

    #[tokio::test]
    async fn put_disabled_when_brain_off() {
        let resp =
            handle_put(&serde_json::json!({ "request_id":"t2", "slug":"s", "markdown":"m" })).await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[tokio::test]
    async fn save_webpage_disabled_when_brain_off() {
        let resp = handle_save_webpage(&serde_json::json!({
            "request_id":"t3", "url":"https://x.com", "title":"X", "content":"hi"
        }))
        .await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }

    #[tokio::test]
    async fn save_conversation_disabled_when_brain_off() {
        let resp = handle_save_conversation(&serde_json::json!({
            "request_id":"t4", "title":"Chat about Rust", "content":"..."
        }))
        .await;
        assert_eq!(resp["payload"]["error"]["code"], "BRAIN_DISABLED");
    }
}
