//! Turn start with Jev on (spec §5.5, §5.7): the tool set first — a change
//! of tools rewrites the cache, so it forces the history decision — then
//! the earlier turns, then the tools their native pairs still need.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nevoflux_builtin_wasm::{Message, ToolDefinition};
use nevoflux_llm::ProviderType;
use nevoflux_protocol::session_event::{SessionEvent, SessionEventPayload};

use super::client;
use super::economics::{cache_rate, warm};
use super::history::{nested, TableTurn};
use super::oracle::{DecisionOracle, JevOracle, OracleContext, Verdict};
use super::rebuild::{
    history_for_turn, history_opts, last_request_ms, rebuild_point_on, RebuildEnv,
};
use super::tools::{
    candidates, decide_set, pinned, probabilities, questions, with_core, MAX_NOULS_PER_REQUEST,
};
use crate::session_events::SessionEventWriter;

/// The tools request asks ~100 Nouls at once; it may take this many Jev
/// timeouts.
const TOOLS_TIMEOUT_FACTOR: u32 = 3;
/// The query as sent in the tools question.
const QUERY_CHARS: usize = 1_000;

/// Jev, its tools point, egress and a cloud provider: the conditions for
/// Jev choosing the tools (spec §5.5, §3.2).
pub fn tools_point_on(cfg: &crate::config::AgentConfig) -> bool {
    let jev = &cfg.jev;
    let cloud = cfg
        .llm
        .active_provider()
        .and_then(|p| cfg.llm.resolve_wire(p))
        .is_some_and(|w| w != ProviderType::Local);
    jev.is_usable()
        && jev.points.tools
        && cloud
        && client::egress_allowed(crate::local::latch::is_on(), &jev.endpoint)
}

/// The set the last `tools/select` event left, if any (a subagent's nested
/// run aside).
pub fn current_set(events: &[SessionEvent]) -> Option<Vec<String>> {
    let nested = nested(events);
    events
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, e)| match &e.payload {
            SessionEventPayload::ToolsSelect { names, .. } if !nested[i] => Some(names.clone()),
            _ => None,
        })
}

/// What a turn starts with.
#[derive(Debug, Clone, Default)]
pub struct TurnStart {
    /// Earlier turns from the log (`None`: the rebuild point is off or the
    /// log could not be read — the caller's text history applies).
    pub history: Option<Vec<Message>>,
    /// The tool set (`None`: offer the mode's tools as before).
    pub tools: Option<Vec<String>>,
}

/// Jev's "will the request need it?" for every candidate, or `None` on any
/// fallback. Split into requests of at most [`MAX_NOULS_PER_REQUEST`].
async fn ask_tools(
    cfg: &crate::config::AgentConfig,
    writer: Option<Arc<SessionEventWriter>>,
    query: &str,
    cands: &[(String, String)],
) -> Option<BTreeMap<String, f64>> {
    let c = client::shared(&cfg.jev).ok()?;
    let oracle = JevOracle::new(c, cfg.jev.sensitive_domains.clone(), writer, None);
    let ctx = OracleContext::no_page(
        "tools",
        Duration::from_millis(cfg.jev.timeout_ms) * TOOLS_TIMEOUT_FACTOR,
    );
    let state = serde_json::json!({
        "query": query.chars().take(QUERY_CHARS).collect::<String>(),
    });
    let asks = cands.chunks(MAX_NOULS_PER_REQUEST).map(|part| {
        let (oracle, ctx, state) = (&oracle, &ctx, state.clone());
        async move { oracle.ask(ctx, state, questions(part)).await }
    });
    let mut answers = Vec::new();
    for v in futures::future::join_all(asks).await {
        match v {
            Verdict::Answered(r) => answers.push(r),
            Verdict::Fallback { .. } => return None,
        }
    }
    Some(probabilities(&answers))
}

/// Turn start with Jev on: the tool set (when `catalog` is given and the
/// tools point is on), then the earlier turns (when the rebuild point is
/// on), then the tools those turns' native pairs still need.
pub async fn turn_start(
    cfg: &crate::config::AgentConfig,
    database: &Arc<nevoflux_storage::Database>,
    session_id: &str,
    query: &str,
    max_messages: usize,
    table: Vec<TableTurn>,
    catalog: Option<&[ToolDefinition]>,
) -> TurnStart {
    let Ok(events) =
        nevoflux_storage::repositories::SessionEventRepository::new(database).list(session_id)
    else {
        return TurnStart::default();
    };
    let Some(wire) = cfg
        .llm
        .active_provider()
        .and_then(|p| cfg.llm.resolve_wire(p))
    else {
        return TurnStart::default();
    };
    let writer = Arc::new(SessionEventWriter::new(
        database.clone(),
        session_id.to_string(),
    ));
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // Tools first: a change rewrites the cache, so it forces the history.
    let started = Instant::now();
    let current = current_set(&events);
    let mut set: Option<(Option<Vec<String>>, &'static str)> = None;
    if let Some(catalog) = catalog.filter(|_| tools_point_on(cfg)) {
        let cands = candidates(catalog);
        set = Some(
            match ask_tools(cfg, Some(writer.clone()), query, &cands).await {
                Some(p) => {
                    let is_warm = warm(
                        last_request_ms(&events),
                        now_ms,
                        cache_rate(wire, &cfg.jev.cache),
                    );
                    let d = decide_set(current.as_deref(), &p, cfg.jev.tools_k, is_warm);
                    (Some(d.names), d.reason)
                }
                // §5.8: keep the current set; none yet → the full list.
                None => (current.clone(), "fallback"),
            },
        );
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let forced = set
        .as_ref()
        .is_some_and(|(_, r)| *r == "tool_change")
        .then_some("tool_change");

    let history = if rebuild_point_on(cfg) {
        let env = RebuildEnv {
            jev: &cfg.jev,
            wire,
            events,
            query,
            writer: Some(writer.clone()),
            stats: None,
            opts: history_opts(cfg, max_messages, table),
            now_ms,
            forced,
        };
        Some(history_for_turn(&env).await)
    } else {
        None
    };

    // The unload constraint: a tool with a native pair in the history stays.
    let tools = match set {
        Some((Some(names), reason)) => {
            let pins = history.as_deref().map(pinned).unwrap_or_default();
            let names = with_core(names.into_iter().chain(pins));
            let prev = current.unwrap_or_default();
            writer.append(SessionEventPayload::ToolsSelect {
                reason: reason.to_string(),
                added: names
                    .iter()
                    .filter(|n| !prev.contains(n))
                    .cloned()
                    .collect(),
                removed: prev
                    .iter()
                    .filter(|n| !names.contains(n))
                    .cloned()
                    .collect(),
                names: names.clone(),
                elapsed_ms,
            });
            Some(names)
        }
        _ => None,
    };
    TurnStart { history, tools }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::test_support::answering;
    use nevoflux_protocol::session_event::SessionEventPayload;
    use serde_json::json;
    use std::time::Duration;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} does it."),
            input_schema: json!({"type":"object"}),
        }
    }
    fn catalog() -> Vec<ToolDefinition> {
        [
            "browser_navigate",
            "browser_get_markdown",
            "browser_get_tabs",
            "web_search",
            "think",
            "read",
        ]
        .into_iter()
        .map(def)
        .collect()
    }
    /// Jev's answer: web_search likely, the rest not; plus a Score for any H question.
    fn answer() -> serde_json::Value {
        json!({"answers": {
            "web_search": {"noul": 0.95}, "think": {"noul": 0.1}, "read": {"noul": 0.05},
            "remaining_steps": {"type": "score", "probabilities": {"1": 1.0}},
            "visibility": {"choice": "full", "probabilities": {}}
        }, "usage": {"input_tokens": 100, "output_tokens": 5}})
    }
    fn cfg(url: &str, timeout_ms: u64) -> crate::config::AgentConfig {
        let mut c = crate::config::AgentConfig::default();
        c.llm.provider = Some("anthropic".into());
        c.jev.enabled = true;
        c.jev.endpoint = url.to_string();
        c.jev.api_key = "k".into();
        c.jev.timeout_ms = timeout_ms;
        c.jev.tools_k = 1;
        c
    }
    fn db_with(events: Vec<serde_json::Value>) -> Arc<nevoflux_storage::Database> {
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let repo = nevoflux_storage::repositories::SessionEventRepository::new(&db);
        for e in events {
            repo.append("s1", &serde_json::from_value(e).unwrap())
                .unwrap();
        }
        db
    }
    fn logged(db: &nevoflux_storage::Database) -> Vec<SessionEventPayload> {
        nevoflux_storage::repositories::SessionEventRepository::new(db)
            .list("s1")
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect()
    }
    fn earlier_turn() -> Vec<serde_json::Value> {
        vec![
            json!({"type": "turn/start", "turn": 1}),
            json!({"type": "user/message", "content": "find flights", "origin": "user"}),
            json!({"type": "assistant/message", "content": "done", "tool_calls": [], "model": "m", "provider": "anthropic"}),
            json!({"type": "turn/end", "turn": 1}),
        ]
    }
    fn prior_set(names: &[&str]) -> serde_json::Value {
        json!({"type": "tools/select", "reason": "initial", "names": names, "added": [], "removed": [], "elapsed_ms": 1})
    }
    async fn start(
        c: &crate::config::AgentConfig,
        db: &Arc<nevoflux_storage::Database>,
    ) -> TurnStart {
        turn_start(
            c,
            db,
            "s1",
            "which flight is cheapest?",
            50,
            vec![],
            Some(&catalog()),
        )
        .await
    }
    fn core_plus(extra: &[&str]) -> Vec<String> {
        crate::jev::tools::with_core(extra.iter().map(|s| s.to_string()))
    }
    fn with_read_pair(content: &str) -> Vec<serde_json::Value> {
        let mut ev = earlier_turn();
        ev.insert(2, json!({"type": "assistant/message", "content": "", "tool_calls": [{"id": "t1", "name": "read", "args": {}}], "model": "m", "provider": "anthropic"}));
        ev.insert(
            3,
            json!({"type": "tool/call", "id": "t1", "name": "read", "args": {}, "origin": "model"}),
        );
        ev.insert(4, json!({"type": "tool/result", "id": "t1", "content": content, "is_error": false, "duration_ms": 1}));
        ev
    }

    #[tokio::test]
    async fn the_first_jev_turn_chooses_core_plus_top_k() {
        let (url, _) = answering(answer(), Duration::ZERO).await;
        let db = db_with(earlier_turn());
        let ts = start(&cfg(&url, 2000), &db).await;
        assert_eq!(ts.tools, Some(core_plus(&["web_search"])));
        assert!(logged(&db).iter().any(|e| matches!(e,
            SessionEventPayload::ToolsSelect { reason, .. } if reason == "initial")));
    }

    #[tokio::test]
    async fn jev_down_keeps_the_current_set() {
        let (url, _) = answering(answer(), Duration::from_millis(3000)).await;
        let mut ev = earlier_turn();
        ev.insert(
            1,
            prior_set(&[
                "browser_get_markdown",
                "browser_get_tabs",
                "browser_navigate",
                "think",
            ]),
        );
        let db = db_with(ev);
        let ts = start(&cfg(&url, 100), &db).await;
        assert_eq!(ts.tools, Some(core_plus(&["think"])));
        assert!(logged(&db).iter().any(|e| matches!(e,
            SessionEventPayload::ToolsSelect { reason, .. } if reason == "fallback")));
    }

    #[tokio::test]
    async fn jev_down_with_no_set_sends_everything() {
        let (url, _) = answering(answer(), Duration::from_millis(3000)).await;
        let db = db_with(earlier_turn());
        let ts = start(&cfg(&url, 100), &db).await;
        assert_eq!(ts.tools, None);
    }

    #[tokio::test]
    async fn a_tool_change_forces_the_history_decision() {
        let (url, _) = answering(answer(), Duration::ZERO).await;
        // A warm cache: the repository stamps events now.
        let mut ev = earlier_turn();
        ev.insert(
            1,
            prior_set(&[
                "browser_get_markdown",
                "browser_get_tabs",
                "browser_navigate",
                "think",
            ]),
        );
        // A graded 6 KB result so the history has something to re-grade.
        let big = "x".repeat(6000);
        ev.insert(3, json!({"type": "assistant/message", "content": "", "tool_calls": [{"id": "t1", "name": "read", "args": {}}], "model": "m", "provider": "anthropic"}));
        ev.insert(
            4,
            json!({"type": "tool/call", "id": "t1", "name": "read", "args": {}, "origin": "model"}),
        );
        ev.insert(5, json!({"type": "tool/result", "id": "t1", "content": big, "is_error": false, "duration_ms": 1}));
        ev.insert(6, json!({"type": "jev/visibility", "id": "c1", "tool": "read", "bytes": 6000, "level": "full",
                            "graded_by": "jev", "kept_lines": 0, "elapsed_ms": 1, "call_id": "t1"}));
        let db = db_with(ev);
        let ts = start(&cfg(&url, 2000), &db).await;
        assert_eq!(
            ts.tools
                .as_ref()
                .map(|t| t.contains(&"web_search".to_string())),
            Some(true)
        );
        let p = logged(&db);
        assert!(
            p.iter().any(|e| matches!(e,
            SessionEventPayload::ToolsSelect { reason, .. } if reason == "tool_change")),
            "{p:?}"
        );
        assert!(
            p.iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { reason, .. } if reason == "tool_change")),
            "{p:?}"
        );
    }

    #[tokio::test]
    async fn a_tool_with_a_pair_in_history_stays_loaded() {
        // The `read` pair is kept (small, so whole), so `read` stays although
        // the new top K is [web_search].
        let (url, _) = answering(answer(), Duration::ZERO).await;
        let db = db_with(with_read_pair("small"));
        let ts = start(&cfg(&url, 2000), &db).await;
        let tools = ts.tools.expect("a set");
        assert!(tools.contains(&"read".to_string()), "{tools:?}");
        assert!(ts
            .history
            .unwrap()
            .iter()
            .any(|m| m.tool_calls.iter().any(|c| c.name == "read")));
    }

    #[tokio::test]
    async fn tools_off_selects_nothing() {
        let (url, bodies) = answering(answer(), Duration::ZERO).await;
        let db = db_with(earlier_turn());
        let mut c = cfg(&url, 2000);
        c.jev.points.tools = false;
        let ts = start(&c, &db).await;
        assert_eq!(ts.tools, None);
        assert!(!bodies
            .lock()
            .unwrap()
            .iter()
            .any(|b| b.contains("web_search")));
        assert!(!logged(&db)
            .iter()
            .any(|e| matches!(e, SessionEventPayload::ToolsSelect { .. })));
    }

    #[tokio::test]
    async fn the_tools_request_sends_no_page_text() {
        let (url, bodies) = answering(answer(), Duration::ZERO).await;
        let db = db_with(with_read_pair("SECRET"));
        start(&cfg(&url, 2000), &db).await;
        let all = bodies.lock().unwrap().join("\n");
        assert!(all.contains("which flight is cheapest?") && all.contains("web_search"));
        assert!(!all.contains("SECRET"));
    }
}
