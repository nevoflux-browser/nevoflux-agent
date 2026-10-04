//! Jev visibility I/O (spec §5.6): store the full text, grade it with Jev
//! (parts in parallel), render, log; fall back to the local rule.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nevoflux_builtin_wasm::{RenderRequest, Rendered};
use nevoflux_protocol::session_event::SessionEventPayload;

use super::client;
use super::oracle::{DecisionOracle, JevOracle, OracleContext, Verdict};
use super::privacy::{scope_for, Scope};
use super::visibility::{
    combine, page_kind, parts, pseudo_lines, questions, render as render_level, Level, Meta,
    PageKind, SMALL,
};
use crate::session_events::SessionEventWriter;
use crate::turn_stats::TurnStats;

/// What `recall` returns per call.
const RECALL_BYTES: usize = 32_000;

/// The daemon side of one render.
pub struct RenderEnv<'a> {
    pub jev: &'a crate::config::JevConfig,
    /// For a browser result: the URLs of the tabs it may come from, as the
    /// browser reported them just now (`None` when that is not known).
    pub page_urls: Option<Vec<String>>,
    pub events: Option<Arc<SessionEventWriter>>,
    pub stats: Option<Arc<TurnStats>>,
    /// Writes the full text under the chunk id; false when it could not, and
    /// then nothing is rendered (never hide what cannot be recalled).
    pub store: &'a dyn Fn(&str, &str) -> bool,
}

/// Grade and render one tool result, or `None` to leave it as it is.
pub async fn render(env: &RenderEnv<'_>, req: &RenderRequest<'_>) -> Option<Rendered> {
    let content = req.content;
    if content.len() <= SMALL {
        return None;
    }
    // The chunk id is ours, not the provider's: Gemini reuses the function
    // name as its call id, so two results could share one file.
    let chunk = mint_chunk_id();
    if !(env.store)(&chunk, content) {
        return None;
    }
    let started = Instant::now();
    let lines = pseudo_lines(content);
    let timeout = Duration::from_millis(env.jev.timeout_ms);
    let ctx = match page_kind(&req.call.name, &req.call.arguments) {
        // Only when every tab it may come from is known and ordinary.
        PageKind::Browser => match &env.page_urls {
            Some(urls)
                if !urls.is_empty()
                    && urls
                        .iter()
                        .all(|u| scope_for(u, &env.jev.sensitive_domains) == Scope::Full) =>
            {
                OracleContext::page("visibility", urls[0].clone(), timeout)
            }
            _ => OracleContext::unknown_page("visibility", timeout),
        },
        PageKind::Url(u) => OracleContext::page("visibility", u, timeout),
        PageKind::Unknown => OracleContext::unknown_page("visibility", timeout),
        PageKind::None => OracleContext::no_page("visibility", timeout),
    };
    // Sensitive or unknown pages are never sent (§5.8): the local rule.
    if ctx.scope(&env.jev.sensitive_domains) == Scope::MetadataOnly {
        return Some(named(
            &chunk,
            finish(
                env,
                req,
                &chunk,
                Level::Short,
                &lines,
                &[],
                "sensitive",
                started,
            ),
        ));
    }
    let Ok(jev_client) = client::shared(env.jev) else {
        return Some(named(
            &chunk,
            finish(
                env,
                req,
                &chunk,
                Level::Short,
                &lines,
                &[],
                "fallback",
                started,
            ),
        ));
    };
    let oracle = JevOracle::new(
        jev_client,
        env.jev.sensitive_domains.clone(),
        env.events.clone(),
        env.stats.clone(),
    );
    let (ps, _ungraded) = parts(&lines, req.query);
    let asks = ps.iter().map(|p| {
        let (state, qs) = questions(req.query, p);
        let oracle = &oracle;
        let ctx = &ctx;
        async move { oracle.ask(ctx, state, qs).await }
    });
    let mut answers = Vec::new();
    for verdict in futures::future::join_all(asks).await {
        match verdict {
            Verdict::Answered(r) => answers.push(r),
            // Each failing part was logged as jev/fallback by the oracle.
            Verdict::Fallback { .. } => {
                return Some(named(
                    &chunk,
                    finish(
                        env,
                        req,
                        &chunk,
                        Level::Short,
                        &lines,
                        &[],
                        "fallback",
                        started,
                    ),
                ));
            }
        }
    }
    let grade = combine(&ps, &answers);
    if grade.level == Level::Full && content.len() <= req.max_bytes {
        log(env, req, &chunk, Level::Full, &[], "jev", started);
        return Some(named(&chunk, req.content.to_string()));
    }
    Some(named(
        &chunk,
        finish(
            env,
            req,
            &chunk,
            grade.level,
            &lines,
            &grade.kept,
            "jev",
            started,
        ),
    ))
}

fn named(chunk: &str, content: String) -> Rendered {
    Rendered {
        content,
        chunk_id: Some(chunk.to_string()),
    }
}

fn finish(
    env: &RenderEnv<'_>,
    req: &RenderRequest<'_>,
    chunk: &str,
    level: Level,
    lines: &[String],
    kept: &[std::ops::Range<usize>],
    graded_by: &str,
    started: Instant,
) -> String {
    let meta = Meta {
        id: chunk,
        tool: &req.call.name,
        bytes: req.content.len(),
        graded_by,
    };
    let kept: &[std::ops::Range<usize>] = if level == Level::Long { kept } else { &[] };
    log(env, req, chunk, level, kept, graded_by, started);
    render_level(level, req.content, lines, kept, &meta, req.max_bytes)
}

fn log(
    env: &RenderEnv<'_>,
    req: &RenderRequest<'_>,
    chunk: &str,
    level: Level,
    kept: &[std::ops::Range<usize>],
    graded_by: &str,
    started: Instant,
) {
    if let Some(w) = &env.events {
        // Where the text may come from, so a later re-grade can re-check it.
        let pages = match page_kind(&req.call.name, &req.call.arguments) {
            PageKind::Browser => env.page_urls.clone().unwrap_or_default(),
            PageKind::Url(u) => vec![u],
            PageKind::Unknown | PageKind::None => Vec::new(),
        };
        w.append(SessionEventPayload::JevVisibility {
            id: chunk.to_string(),
            tool: req.call.name.clone(),
            bytes: req.content.len() as u64,
            level: level.as_str().to_string(),
            graded_by: graded_by.to_string(),
            kept_lines: kept.iter().map(|r| r.len() as u64).sum(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            call_id: Some(req.tool_call_id.to_string()),
            kept: kept
                .iter()
                .map(|r| [r.start as u64, r.end as u64])
                .collect(),
            pages,
        });
    }
}

/// A chunk id unique in this process and, through the time part, across
/// restarts: `c` + millis (hex) + a per-process counter.
fn mint_chunk_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("c{ms:x}{n:x}")
}

/// Up to [`RECALL_BYTES`] of `full` from byte `offset`, with a note naming
/// the next offset when more remains.
pub fn recall_slice(full: &str, offset: u64, chunk_id: &str) -> String {
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    if start >= full.len() {
        return format!(
            "[{chunk_id}: nothing more after offset {offset} ({} bytes total)]",
            full.len()
        );
    }
    let mut s = start;
    while !full.is_char_boundary(s) {
        s += 1;
    }
    let piece = super::visibility::cut_bytes(&full[s..], RECALL_BYTES);
    let end = s + piece.len();
    if end < full.len() {
        format!("{piece}\n… [{chunk_id}: continues — recall(\"{chunk_id}\", offset={end})]")
    } else {
        piece.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::test_support::answering;
    use nevoflux_builtin_wasm::{AgentMode, ToolCall, ToolContext};
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn ctx(tab: Option<&str>) -> ToolContext {
        ToolContext {
            session_id: "s1".into(),
            origin: "model".into(),
            mode: AgentMode::Agent,
            is_unattended: false,
            tab_url: tab.map(str::to_string),
            allowed_tools: None,
        }
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            call_id: None,
            name: name.into(),
            arguments: args,
            signature: None,
        }
    }

    fn jev(url: &str, timeout_ms: u64) -> crate::config::JevConfig {
        let mut j = crate::config::JevConfig::default();
        j.enabled = true;
        j.endpoint = url.to_string();
        j.api_key = "k".into();
        j.timeout_ms = timeout_ms;
        j
    }

    /// 100 lines of 60 bytes: 6 KB, over the 4 KB threshold.
    fn hundred_lines() -> String {
        (0..100)
            .map(|i| format!("{:<59}\n", format!("line {i}")))
            .collect()
    }

    struct Run {
        out: Option<String>,
        chunk: Option<String>,
        stored: HashMap<String, String>,
    }

    async fn run(
        j: &crate::config::JevConfig,
        tool: &str,
        args: serde_json::Value,
        tab: Option<&str>,
        content: &str,
        store_ok: bool,
        events: Option<Arc<SessionEventWriter>>,
    ) -> Run {
        let stored = RefCell::new(HashMap::new());
        let store = |id: &str, text: &str| {
            if store_ok {
                stored.borrow_mut().insert(id.to_string(), text.to_string());
            }
            store_ok
        };
        let env = RenderEnv {
            jev: j,
            page_urls: tab.map(|u| vec![u.to_string()]),
            events,
            stats: None,
            store: &store,
        };
        let c = call(tool, args);
        let x = ctx(tab);
        let req = RenderRequest {
            call: &c,
            tool_call_id: "call_1",
            ctx: &x,
            turn_tab_url: tab,
            query: "what is on line 30",
            content,
            max_bytes: 32_000,
        };
        let rendered = render(&env, &req).await;
        Run {
            out: rendered.as_ref().map(|r| r.content.clone()),
            chunk: rendered.and_then(|r| r.chunk_id),
            stored: stored.into_inner(),
        }
    }

    fn answer(choice: &str, keep: &[&str]) -> serde_json::Value {
        let mut answers = serde_json::Map::new();
        answers.insert(
            "visibility".into(),
            serde_json::json!({"choice": choice, "probabilities": {}}),
        );
        for b in ["b000", "b001", "b002", "b003"] {
            let p = if keep.contains(&b) { 0.9 } else { 0.0 };
            answers.insert(b.into(), serde_json::json!({ "noul": p }));
        }
        serde_json::json!({"answers": answers, "usage": {"input_tokens": 10, "output_tokens": 1}})
    }

    #[tokio::test]
    async fn a_small_result_is_left_alone_and_sends_nothing() {
        let (url, bodies) = answering(answer("hide", &[]), Duration::ZERO).await;
        let r = run(
            &jev(&url, 2000),
            "read",
            serde_json::json!({}),
            None,
            &"x".repeat(3000),
            true,
            None,
        )
        .await;
        assert!(r.out.is_none());
        assert!(bodies.lock().unwrap().is_empty());
        assert!(r.stored.is_empty());
    }

    #[tokio::test]
    async fn a_long_grade_renders_the_kept_blocks() {
        let (url, bodies) = answering(answer("long", &["b001"]), Duration::ZERO).await;
        let content = hundred_lines();
        let r = run(
            &jev(&url, 2000),
            "read",
            serde_json::json!({}),
            None,
            &content,
            true,
            None,
        )
        .await;
        let out = r.out.expect("rendered");
        assert!(out.contains("line 25") && out.contains("line 49"), "{out}");
        assert!(!out.contains("line 50 "), "{out}");
        assert_eq!(bodies.lock().unwrap().len(), 1);
        let (chunk, stored) = r.stored.iter().next().expect("stored");
        assert!(out.contains(&format!("recall(\"{chunk}\")")), "{out}");
        assert_eq!(stored, &content);
    }

    #[tokio::test]
    async fn a_long_grade_logs_its_call_and_kept_lines() {
        let (url, _) = answering(answer("long", &["b001"]), Duration::ZERO).await;
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let w = Arc::new(SessionEventWriter::new(db.clone(), "s1".into()));
        run(
            &jev(&url, 2000),
            "read",
            serde_json::json!({}),
            None,
            &hundred_lines(),
            true,
            Some(w),
        )
        .await;
        let events = nevoflux_storage::repositories::SessionEventRepository::new(&db)
            .list("s1")
            .unwrap();
        assert!(
            events.iter().any(|e| matches!(
                &e.payload,
                SessionEventPayload::JevVisibility { call_id: Some(c), kept, .. }
                    if c == "call_1" && kept == &vec![[25u64, 50u64]]
            )),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_grade_logs_the_pages_its_text_may_come_from() {
        let (url, _) = answering(answer("long", &["b001"]), Duration::ZERO).await;
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let w = Arc::new(SessionEventWriter::new(db.clone(), "s1".into()));
        run(
            &jev(&url, 2000),
            "browser_get_markdown",
            serde_json::json!({}),
            Some("https://shop.example/cart"),
            &hundred_lines(),
            true,
            Some(w),
        )
        .await;
        let events = nevoflux_storage::repositories::SessionEventRepository::new(&db)
            .list("s1")
            .unwrap();
        assert!(
            events.iter().any(|e| matches!(
                &e.payload,
                SessionEventPayload::JevVisibility { pages, .. }
                    if pages == &vec!["https://shop.example/cart".to_string()]
            )),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_hidden_result_keeps_only_the_stub() {
        let (url, _) = answering(answer("hide", &[]), Duration::ZERO).await;
        let r = run(
            &jev(&url, 2000),
            "read",
            serde_json::json!({}),
            None,
            &hundred_lines(),
            true,
            None,
        )
        .await;
        let out = r.out.expect("rendered");
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.contains("hide"));
    }

    #[tokio::test]
    async fn a_full_grade_that_fits_keeps_the_text_and_names_the_chunk() {
        let (url, _) = answering(answer("full", &[]), Duration::ZERO).await;
        let content = hundred_lines();
        let r = run(
            &jev(&url, 2000),
            "read",
            serde_json::json!({}),
            None,
            &content,
            true,
            None,
        )
        .await;
        assert_eq!(r.out.as_deref(), Some(content.as_str()));
        let chunk = r.chunk.expect("chunk id");
        assert_eq!(r.stored.get(&chunk), Some(&content));
    }

    #[tokio::test]
    async fn a_jev_timeout_renders_short_and_logs_both_events() {
        let (url, _) = answering(answer("hide", &[]), Duration::from_millis(300)).await;
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let w = Arc::new(SessionEventWriter::new(db.clone(), "s1".into()));
        let r = run(
            &jev(&url, 50),
            "read",
            serde_json::json!({}),
            None,
            &hundred_lines(),
            true,
            Some(w),
        )
        .await;
        let out = r.out.expect("rendered");
        assert!(
            out.contains("short") && out.contains("graded by fallback"),
            "{out}"
        );
        let events = nevoflux_storage::repositories::SessionEventRepository::new(&db)
            .list("s1")
            .unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(&e.payload, SessionEventPayload::JevFallback { .. })));
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            SessionEventPayload::JevVisibility { graded_by, level, .. } if graded_by == "fallback" && level == "short"
        )));
    }

    #[tokio::test]
    async fn a_sensitive_page_is_never_sent() {
        let (url, bodies) = answering(answer("full", &[]), Duration::ZERO).await;
        let r = run(
            &jev(&url, 2000),
            "browser_get_markdown",
            serde_json::json!({}),
            Some("https://www.paypal.com/x"),
            &hundred_lines(),
            true,
            None,
        )
        .await;
        let out = r.out.expect("rendered");
        assert!(out.contains("graded by sensitive"), "{out}");
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_mcp_result_is_never_sent() {
        let (url, bodies) = answering(answer("full", &[]), Duration::ZERO).await;
        let r = run(
            &jev(&url, 2000),
            "gmail__search",
            serde_json::json!({}),
            None,
            &hundred_lines(),
            true,
            None,
        )
        .await;
        assert!(r.out.expect("rendered").contains("graded by sensitive"));
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn nothing_is_rendered_when_the_full_text_cannot_be_stored() {
        let (url, bodies) = answering(answer("hide", &[]), Duration::ZERO).await;
        let r = run(
            &jev(&url, 2000),
            "read",
            serde_json::json!({}),
            None,
            &hundred_lines(),
            false,
            None,
        )
        .await;
        assert!(r.out.is_none());
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_dynamic_mcp_result_is_never_sent() {
        let (url, bodies) = answering(answer("full", &[]), Duration::ZERO).await;
        let args = serde_json::json!({"tool_name": "search_mail", "arguments": {}});
        let r = run(
            &jev(&url, 2000),
            "tool_call_dynamic",
            args,
            None,
            &hundred_lines(),
            true,
            None,
        )
        .await;
        assert!(r.out.expect("rendered").contains("graded by sensitive"));
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_browser_result_without_known_tabs_is_never_sent() {
        let (url, bodies) = answering(answer("full", &[]), Duration::ZERO).await;
        let r = run(
            &jev(&url, 2000),
            "browser_get_markdown",
            serde_json::json!({}),
            None,
            &hundred_lines(),
            true,
            None,
        )
        .await;
        assert!(r.out.expect("rendered").contains("graded by sensitive"));
        assert!(bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_browser_result_is_sent_when_its_tab_is_ordinary() {
        let (url, bodies) = answering(answer("hide", &[]), Duration::ZERO).await;
        let r = run(
            &jev(&url, 2000),
            "browser_get_markdown",
            serde_json::json!({}),
            Some("https://en.wikipedia.org/wiki/Rust"),
            &hundred_lines(),
            true,
            None,
        )
        .await;
        assert!(r.out.expect("rendered").contains("hide"));
        assert_eq!(bodies.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_results_with_the_same_call_id_get_distinct_chunks() {
        // Review I1: Gemini reuses the function name as the call id.
        let (url, _) = answering(answer("hide", &[]), Duration::ZERO).await;
        let a = run(
            &jev(&url, 2000),
            "read",
            serde_json::json!({}),
            None,
            &hundred_lines(),
            true,
            None,
        )
        .await;
        let other: String = hundred_lines().replace("line", "LINE");
        let b = run(
            &jev(&url, 2000),
            "read",
            serde_json::json!({}),
            None,
            &other,
            true,
            None,
        )
        .await;
        let ka: Vec<_> = a.stored.keys().cloned().collect();
        let kb: Vec<_> = b.stored.keys().cloned().collect();
        assert_eq!((ka.len(), kb.len()), (1, 1));
        assert_ne!(ka[0], kb[0]);
        assert!(a.out.unwrap().contains(&format!("recall(\"{}\")", ka[0])));
    }

    #[test]
    fn recall_slice_pages_through_a_long_text() {
        let full = "q".repeat(70_000);
        let first = recall_slice(&full, 0, "c1");
        assert!(first.starts_with(&"q".repeat(32_000)));
        assert!(first.contains("recall(\"c1\", offset=32000)"));
        let last = recall_slice(&full, 64_000, "c1");
        assert_eq!(last, "q".repeat(6_000));
        assert!(recall_slice(&full, 80_000, "c1").contains("nothing more"));
    }
}
