//! Turn start with Jev on (spec §5.5, §5.7): the tool set first — a change
//! of tools rewrites the cache, so it forces the history decision — then
//! the earlier turns, then the tools their native pairs still need.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nevoflux_builtin_wasm::{Message, SkillSummary, ToolDefinition};
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
    asked, candidates, decide_set, pinned, probabilities, questions, with_core,
    MAX_NOULS_PER_REQUEST,
};
use super::wire::Question;
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
    // ACP agents call tools through the MCP bridge, which has no `act` and
    // no missed-tool loading: v1 is the native loop only.
    jev.is_usable()
        && jev.points.tools
        && cloud
        && !cfg.llm.active_provider_is_acp()
        && client::egress_allowed(crate::local::latch::is_on(), &jev.endpoint)
}

/// Jev, its skills point, egress and a cloud, non-ACP provider: the
/// conditions for Jev choosing a skill (spec §5.7).
pub fn skills_point_on(cfg: &crate::config::AgentConfig) -> bool {
    let jev = &cfg.jev;
    let cloud = cfg
        .llm
        .active_provider()
        .and_then(|p| cfg.llm.resolve_wire(p))
        .is_some_and(|w| w != ProviderType::Local);
    // The skill step is an assistant tool call the model never made: no
    // reasoning, no signature. DeepSeek/MiMo thinking modes and Gemini 3
    // reject such a message, so they get no skill step.
    let thinking = super::rebuild::needs_reasoning_back(cfg)
        || cfg
            .llm
            .active_provider()
            .and_then(|p| cfg.llm.resolve_wire(p))
            == Some(ProviderType::Gemini);
    jev.is_usable()
        && jev.points.skills
        && cloud
        && !thinking
        && !cfg.llm.active_provider_is_acp()
        && client::egress_allowed(crate::local::latch::is_on(), &jev.endpoint)
}

/// The run's skills under `filter`, read from turn start, which runs on the
/// async runtime: the registry loads behind a blocking lock, so the read is
/// moved to a thread that may block.
pub fn skill_catalog<H: nevoflux_builtin_wasm::HostFunctions>(
    agent: &nevoflux_builtin_wasm::Agent<H>,
    filter: Option<&[String]>,
) -> Vec<SkillSummary> {
    tokio::task::block_in_place(|| agent.skills_for_input(filter))
}

/// Whether a turn offers Jev the skills: not when the user invoked a skill
/// explicitly (`/skill` pins its own), not when the run cannot call
/// `skill_load` (`can_load`), not with the skills point off.
pub fn skill_catalog_wanted(
    cfg: &crate::config::AgentConfig,
    explicit_skill: bool,
    can_load: bool,
) -> bool {
    !explicit_skill && can_load && skills_point_on(cfg)
}

/// The set the last turn ran with: the last `tools/select` logged for it
/// (just before its `turn/start`, or during it). `None` when that turn had
/// none — it was offered every tool — even if an older turn had a set. A
/// subagent's nested run is not a turn.
pub fn current_set(events: &[SessionEvent]) -> Option<Vec<String>> {
    let nested = nested(events);
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(i, e)| !nested[*i] && matches!(e.payload, SessionEventPayload::TurnStart { .. }))
        .map(|(i, _)| i)
        .collect();
    // The last turn's set is logged after the turn before it started.
    let from = match starts.len() {
        0 | 1 => 0,
        n => starts[n - 2] + 1,
    };
    events
        .iter()
        .enumerate()
        .skip(from)
        .rev()
        .find_map(|(i, e)| match &e.payload {
            SessionEventPayload::ToolsSelect { names, .. } if !nested[i] => Some(names.clone()),
            _ => None,
        })
}

/// A plan re-run continues the current turn: its set as it stands now (the
/// turn-start set plus tools loaded since) and the tools whose native pairs
/// the re-run's history carries. `None` when the turn had no set.
pub fn rerun_set(events: &[SessionEvent], history: &[Message]) -> Option<Vec<String>> {
    current_set(events).map(|set| with_core(set.into_iter().chain(pinned(history))))
}

/// [`rerun_set`] from the session log.
pub fn rerun_tools(
    database: &Arc<nevoflux_storage::Database>,
    session_id: &str,
    history: &[Message],
) -> Option<Vec<String>> {
    let events = nevoflux_storage::repositories::SessionEventRepository::new(database)
        .list(session_id)
        .ok()?;
    rerun_set(&events, history)
}

/// What a turn starts with.
#[derive(Debug, Clone, Default)]
pub struct TurnStart {
    /// Earlier turns from the log (`None`: the rebuild point is off or the
    /// log could not be read — the caller's text history applies).
    pub history: Option<Vec<Message>>,
    /// The tool set (`None`: offer the mode's tools as before).
    pub tools: Option<Vec<String>>,
    /// A skill to load as the turn's first step (spec §5.7).
    pub skill: Option<String>,
}

/// Jev's probabilities for every question, or `None` on any fallback.
/// Split into requests of at most [`MAX_NOULS_PER_REQUEST`]; no questions
/// asks nothing.
async fn ask_nouls(
    cfg: &crate::config::AgentConfig,
    writer: Option<Arc<SessionEventWriter>>,
    query: &str,
    questions: BTreeMap<String, Question>,
) -> Option<BTreeMap<String, f64>> {
    if questions.is_empty() {
        return Some(BTreeMap::new());
    }
    let c = client::shared(&cfg.jev).ok()?;
    let oracle = JevOracle::new(c, cfg.jev.sensitive_domains.clone(), writer, None);
    let ctx = OracleContext::no_page(
        "tools",
        Duration::from_millis(cfg.jev.timeout_ms) * TOOLS_TIMEOUT_FACTOR,
    );
    let state = serde_json::json!({
        "query": query.chars().take(QUERY_CHARS).collect::<String>(),
    });
    let all: Vec<(String, Question)> = questions.into_iter().collect();
    let asks = all.chunks(MAX_NOULS_PER_REQUEST).map(|part| {
        let (oracle, ctx, state) = (&oracle, &ctx, state.clone());
        let part: BTreeMap<String, Question> = part.iter().cloned().collect();
        async move { oracle.ask(ctx, state, part).await }
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
/// on), then the tools those turns' native pairs still need, then a skill
/// (when `skills` are given and the skills point is on). Tool and skill
/// Nouls share the requests (spec §5.4).
#[allow(clippy::too_many_arguments)]
pub async fn turn_start(
    cfg: &crate::config::AgentConfig,
    database: &Arc<nevoflux_storage::Database>,
    session_id: &str,
    query: &str,
    max_messages: usize,
    table: Vec<TableTurn>,
    catalog: Option<&[ToolDefinition]>,
    skills: Option<&[SkillSummary]>,
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
    let is_warm = warm(
        last_request_ms(&events),
        now_ms,
        cache_rate(wire, &cfg.jev.cache),
    );
    // A soul with no tools has nothing to choose from: no set, not core.
    let catalog = catalog.filter(|c| !c.is_empty() && tools_point_on(cfg));
    // The last turn's set, if it still fits the catalog (a soul switch or
    // a changed allowlist makes it stale: choose again).
    let current = current_set(&events).filter(|set| {
        catalog.is_some_and(|c| {
            set.iter().all(|n| {
                super::tools::CORE_TOOLS.contains(&n.as_str()) || c.iter().any(|t| t.name == *n)
            })
        })
    });
    let skills = skills.filter(|s| !s.is_empty() && skills_point_on(cfg));
    let tool_cands = catalog.map(candidates);
    let skill_cands = skills.map(super::skills::candidates);
    let mut questions_all = BTreeMap::new();
    if let Some(c) = &tool_cands {
        questions_all.extend(questions(c));
    }
    if let Some(c) = &skill_cands {
        questions_all.extend(super::skills::questions(c));
    }
    // Each subset counts only when Jev answered most of it (§5.8): a
    // partial answer for skills does not sink the tools, and vice versa.
    let answers = ask_nouls(cfg, Some(writer.clone()), query, questions_all).await;
    let (tool_p, skill_p) = match &answers {
        Some(p) => {
            let (t, k) = super::skills::split(p);
            (
                tool_cands.as_ref().and_then(|c| asked(t, c)),
                skill_cands.as_ref().and_then(|c| asked(k, c)),
            )
        }
        None => (None, None),
    };
    let mut set: Option<(Option<Vec<String>>, &'static str)> = None;
    if catalog.is_some() {
        set = Some(match tool_p {
            Some(p) => {
                let d = decide_set(current.as_deref(), &p, cfg.jev.tools_k, is_warm);
                (Some(d.names), d.reason)
            }
            // §5.8: keep the current set; none yet → the full list.
            None => (current.clone(), "fallback"),
        });
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    // Any change of what the last turn was offered — a new set, a set
    // replacing the full list — rewrites a warm cache (spec §5.7).
    let offered_changed = match &set {
        Some((names, _)) => *names != current,
        None => false,
    };
    let forced = (offered_changed && is_warm).then_some("tool_change");

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

    // A skill (§5.7): at most one, not one the history already loaded; a
    // Jev failure injects nothing (§5.8).
    let skill = skill_p.and_then(|p| {
        let loaded = super::skills::loaded_in(history.as_deref().unwrap_or(&[]));
        super::skills::choose_skill(&p, cfg.jev.skill_threshold, &loaded)
    });

    // The unload constraint: a tool with a native pair in the history stays;
    // an injected skill's `skill_load` pair needs its tool too.
    let tools = match set {
        Some((Some(names), reason)) => {
            let pins = history.as_deref().map(pinned).unwrap_or_default();
            let skill_tool = skill.as_ref().map(|_| "skill_load".to_string());
            let names = with_core(names.into_iter().chain(pins).chain(skill_tool));
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
    if let Some((name, p)) = &skill {
        writer.append(SessionEventPayload::SkillInject {
            name: name.clone(),
            p: *p,
            elapsed_ms,
        });
    }
    TurnStart {
        history,
        tools,
        skill: skill.map(|(n, _)| n),
    }
}

/// What [`start_turn`] gives a run.
#[derive(Debug, Clone, Default)]
pub struct StartedTurn {
    pub history: Vec<Message>,
    pub tools: Option<Vec<String>>,
    pub skill: Option<String>,
}

/// A turn's starting history, tool set and skill for chat and tasks: the
/// log's history when `use_log` (and the rebuild point is on), `text`
/// otherwise or when the log has no earlier turns; Jev's tool set when
/// `catalog` is given and the tools point is on; a skill when `skills` are
/// given and the skills point is on. With Jev off: `text`, nothing read.
#[allow(clippy::too_many_arguments)]
pub async fn start_turn(
    cfg: &crate::config::AgentConfig,
    database: &Arc<nevoflux_storage::Database>,
    session_id: &str,
    query: &str,
    max_messages: usize,
    text: Vec<Message>,
    table: Vec<TableTurn>,
    catalog: Option<&[ToolDefinition]>,
    skills: Option<&[SkillSummary]>,
    use_log: bool,
) -> StartedTurn {
    let use_log = use_log && rebuild_point_on(cfg);
    let catalog = catalog.filter(|_| tools_point_on(cfg));
    let skills = skills.filter(|_| skills_point_on(cfg));
    if session_id.is_empty() || !(use_log || catalog.is_some() || skills.is_some()) {
        return StartedTurn {
            history: text,
            ..Default::default()
        };
    }
    let started = Instant::now();
    let ts = turn_start(
        cfg,
        database,
        session_id,
        query,
        max_messages,
        table,
        catalog,
        skills,
    )
    .await;
    super::wait::log(
        Some(&SessionEventWriter::new(
            database.clone(),
            session_id.to_string(),
        )),
        "turn_start",
        started.elapsed(),
    );
    let history = if use_log {
        super::rebuild::prefer_log(ts.history, text)
    } else {
        text
    };
    StartedTurn {
        history,
        tools: ts.tools,
        skill: ts.skill,
    }
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
            None,
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

    #[tokio::test]
    async fn jev_off_leaves_the_text_history_and_offers_every_tool() {
        let db = db_with(earlier_turn());
        let c = crate::config::AgentConfig::default();
        let text = vec![Message::user("q"), Message::assistant("a")];
        let st = start_turn(
            &c,
            &db,
            "s1",
            "next?",
            50,
            text.clone(),
            vec![],
            Some(&catalog()),
            None,
            true,
        )
        .await;
        let (h, tools) = (st.history, st.tools);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].content, "q");
        assert_eq!(tools, None);
        assert!(!logged(&db)
            .iter()
            .any(|e| matches!(e, SessionEventPayload::ToolsSelect { .. })));
    }

    fn graded_earlier_turn() -> Vec<serde_json::Value> {
        let mut ev = earlier_turn();
        let big = "x".repeat(6000);
        ev.insert(2, json!({"type": "assistant/message", "content": "", "tool_calls": [{"id": "t1", "name": "read", "args": {}}], "model": "m", "provider": "anthropic"}));
        ev.insert(
            3,
            json!({"type": "tool/call", "id": "t1", "name": "read", "args": {}, "origin": "model"}),
        );
        ev.insert(4, json!({"type": "tool/result", "id": "t1", "content": big, "is_error": false, "duration_ms": 1}));
        ev.insert(5, json!({"type": "jev/visibility", "id": "c1", "tool": "read", "bytes": 6000, "level": "full",
                            "graded_by": "jev", "kept_lines": 0, "elapsed_ms": 1, "call_id": "t1"}));
        ev
    }

    #[test]
    fn acp_providers_do_not_get_a_tool_set() {
        // ACP agents call tools through the MCP bridge: no `act`, no
        // missed-tool loading there (spec: v1 is the native loop only).
        let mut c = cfg("http://127.0.0.1:1", 2000);
        c.llm.provider = Some("claude-code".into());
        assert!(!tools_point_on(&c));
    }

    #[tokio::test]
    async fn an_empty_catalog_selects_nothing() {
        // A soul with `tools: none`: nothing to choose, so no set — never
        // a set of core tools the soul does not have.
        let (url, bodies) = answering(answer(), Duration::ZERO).await;
        let db = db_with(earlier_turn());
        let ts = turn_start(
            &cfg(&url, 2000),
            &db,
            "s1",
            "q",
            50,
            vec![],
            Some(&[]),
            None,
        )
        .await;
        assert_eq!(ts.tools, None);
        assert!(bodies.lock().unwrap().iter().all(|b| !b.contains("noul")));
    }

    #[tokio::test]
    async fn an_answer_without_the_nouls_is_a_fallback() {
        let empty = json!({"answers": {}, "usage": {"input_tokens": 1, "output_tokens": 1}});
        let (url, _) = answering(empty.clone(), Duration::ZERO).await;
        let db = db_with(earlier_turn());
        assert_eq!(
            start(&cfg(&url, 2000), &db).await.tools,
            None,
            "no set yet: the full list"
        );

        let (url, _) = answering(empty, Duration::ZERO).await;
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
        assert_eq!(
            start(&cfg(&url, 2000), &db).await.tools,
            Some(core_plus(&["think"]))
        );
        assert!(logged(&db).iter().any(|e| matches!(e,
            SessionEventPayload::ToolsSelect { reason, .. } if reason == "fallback")));
    }

    #[test]
    fn a_set_from_before_a_turn_without_one_is_not_current() {
        let ev: Vec<SessionEvent> = [
            prior_set(&["browser_navigate", "think"]),
            json!({"type": "turn/start", "turn": 1}),
            json!({"type": "turn/end", "turn": 1}),
            json!({"type": "turn/start", "turn": 2}),
            json!({"type": "turn/end", "turn": 2}),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, v)| SessionEvent {
            seq: i as i64 + 1,
            ts: 0,
            payload: serde_json::from_value(v).unwrap(),
        })
        .collect();
        assert_eq!(current_set(&ev), None, "turn 2 ran with the full list");
        assert_eq!(
            current_set(&ev[..3]),
            Some(vec!["browser_navigate".to_string(), "think".to_string()])
        );
    }

    #[tokio::test]
    async fn a_set_with_tools_outside_the_catalog_is_chosen_again() {
        // A soul switch: the old set names a tool the new catalog lacks. No
        // tool is strong enough for a warm tool_change, so only the stale
        // set itself can make it choose again.
        let weak = json!({"answers": {"web_search": {"noul": 0.6}, "think": {"noul": 0.5}, "read": {"noul": 0.1}},
                          "usage": {"input_tokens": 1, "output_tokens": 1}});
        let (url, _) = answering(weak, Duration::ZERO).await;
        let mut ev = earlier_turn();
        ev.insert(
            1,
            prior_set(&[
                "bash",
                "browser_get_markdown",
                "browser_get_tabs",
                "browser_navigate",
                "think",
            ]),
        );
        let db = db_with(ev);
        let ts = start(&cfg(&url, 2000), &db).await;
        let tools = ts.tools.expect("a set");
        assert!(!tools.contains(&"bash".to_string()), "{tools:?}");
        assert!(tools.contains(&"web_search".to_string()), "{tools:?}");
    }

    #[tokio::test]
    async fn switching_from_the_full_list_forces_the_history() {
        // Warm earlier turns ran with every tool; the first set rewrites
        // the cache, so the history is decided as for a tool change.
        let (url, _) = answering(answer(), Duration::ZERO).await;
        let db = db_with(graded_earlier_turn());
        start(&cfg(&url, 2000), &db).await;
        let p = logged(&db);
        assert!(
            p.iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { reason, .. } if reason == "tool_change")),
            "{p:?}"
        );
    }

    #[test]
    fn a_rerun_keeps_tools_loaded_mid_turn_and_tools_with_pairs() {
        let ev: Vec<SessionEvent> = [
            prior_set(&["browser_get_markdown", "browser_get_tabs", "browser_navigate", "web_search"]),
            json!({"type": "turn/start", "turn": 1}),
            json!({"type": "tools/select", "reason": "missed", "names": ["browser_get_markdown", "browser_get_tabs", "browser_navigate", "think", "web_search"], "added": ["think"], "removed": [], "elapsed_ms": 0}),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, v)| SessionEvent { seq: i as i64 + 1, ts: 0, payload: serde_json::from_value(v).unwrap() })
        .collect();
        let history = vec![
            Message::user("q"),
            Message::assistant_with_tool_calls_and_reasoning(
                String::new(),
                vec![nevoflux_builtin_wasm::ToolCall {
                    id: "t1".into(),
                    call_id: None,
                    name: "read".into(),
                    arguments: json!({}),
                    signature: None,
                }],
                None,
            ),
            Message::tool("t1".to_string(), "ok".to_string()),
        ];
        let set = rerun_set(&ev, &history).expect("a set");
        assert!(
            set.contains(&"think".to_string()) && set.contains(&"read".to_string()),
            "{set:?}"
        );
        assert_eq!(rerun_set(&[], &history), None);
    }

    fn skill(name: &str, desc: &str) -> nevoflux_builtin_wasm::SkillSummary {
        nevoflux_builtin_wasm::SkillSummary {
            name: name.into(),
            description: desc.into(),
            tags: vec![],
        }
    }
    fn skills() -> Vec<nevoflux_builtin_wasm::SkillSummary> {
        vec![
            skill("research", "Deep research. Many sources."),
            skill("cooking", "Recipes."),
        ]
    }
    fn answer_with(skill_p: f64) -> serde_json::Value {
        json!({"answers": {
            "web_search": {"noul": 0.95}, "think": {"noul": 0.1}, "read": {"noul": 0.05},
            "skill/research": {"noul": skill_p}, "skill/cooking": {"noul": 0.02},
            "remaining_steps": {"type": "score", "probabilities": {"1": 1.0}},
            "visibility": {"choice": "full", "probabilities": {}}
        }, "usage": {"input_tokens": 100, "output_tokens": 5}})
    }
    async fn start_with_skills(
        c: &crate::config::AgentConfig,
        db: &Arc<nevoflux_storage::Database>,
    ) -> TurnStart {
        turn_start(
            c,
            db,
            "s1",
            "research the history of rust",
            50,
            vec![],
            Some(&catalog()),
            Some(&skills()),
        )
        .await
    }

    #[tokio::test]
    async fn a_likely_skill_is_chosen_and_skill_load_joins_the_set() {
        let (url, _) = answering(answer_with(0.9), Duration::ZERO).await;
        let db = db_with(earlier_turn());
        let ts = start_with_skills(&cfg(&url, 2000), &db).await;
        assert_eq!(ts.skill.as_deref(), Some("research"));
        assert!(ts.tools.unwrap().contains(&"skill_load".to_string()));
        assert!(logged(&db).iter().any(|e| matches!(e,
            SessionEventPayload::SkillInject { name, .. } if name == "research")));
    }

    #[tokio::test]
    async fn a_skill_under_the_threshold_is_not_chosen() {
        let (url, _) = answering(answer_with(0.7), Duration::ZERO).await;
        let db = db_with(earlier_turn());
        assert_eq!(start_with_skills(&cfg(&url, 2000), &db).await.skill, None);
    }

    #[tokio::test]
    async fn a_skill_already_in_history_is_not_injected_again() {
        let (url, _) = answering(answer_with(0.9), Duration::ZERO).await;
        let mut ev = earlier_turn();
        ev.insert(2, json!({"type": "assistant/message", "content": "", "tool_calls": [{"id": "s1", "name": "skill_load", "args": {"name": "research"}}], "model": "skill", "provider": "jev"}));
        ev.insert(3, json!({"type": "tool/call", "id": "s1", "name": "skill_load", "args": {"name": "research"}, "origin": "model"}));
        ev.insert(4, json!({"type": "tool/result", "id": "s1", "content": "research body", "is_error": false, "duration_ms": 1}));
        let db = db_with(ev);
        assert_eq!(start_with_skills(&cfg(&url, 2000), &db).await.skill, None);
    }

    #[tokio::test]
    async fn skill_answers_missing_inject_nothing_but_tools_still_decide() {
        let tools_only = json!({"answers": {"web_search": {"noul": 0.95}, "think": {"noul": 0.1}, "read": {"noul": 0.05}},
                                "usage": {"input_tokens": 1, "output_tokens": 1}});
        let (url, _) = answering(tools_only, Duration::ZERO).await;
        let db = db_with(earlier_turn());
        let ts = start_with_skills(&cfg(&url, 2000), &db).await;
        assert_eq!(ts.skill, None);
        assert!(ts.tools.unwrap().contains(&"web_search".to_string()));
    }

    #[tokio::test]
    async fn skills_off_asks_no_skill_nouls() {
        let (url, bodies) = answering(answer_with(0.9), Duration::ZERO).await;
        let db = db_with(earlier_turn());
        let mut c = cfg(&url, 2000);
        c.jev.points.skills = false;
        let ts = start_with_skills(&c, &db).await;
        assert_eq!(ts.skill, None);
        assert!(!bodies.lock().unwrap().join("\n").contains("skill/"));
    }

    #[tokio::test]
    async fn skills_without_the_tools_point_ask_skill_nouls_alone() {
        let (url, bodies) = answering(answer_with(0.9), Duration::ZERO).await;
        let db = db_with(earlier_turn());
        let mut c = cfg(&url, 2000);
        c.jev.points.tools = false;
        let ts = start_with_skills(&c, &db).await;
        assert_eq!(ts.skill.as_deref(), Some("research"));
        assert_eq!(ts.tools, None);
        let all = bodies.lock().unwrap().join("\n");
        assert!(all.contains("skill/research") && !all.contains("`web_search`"));
    }

    #[test]
    fn an_explicit_skill_turn_offers_no_skill_catalog() {
        let c = cfg("http://127.0.0.1:1", 2000);
        assert!(skill_catalog_wanted(&c, false, true));
        assert!(
            !skill_catalog_wanted(&c, true, true),
            "/skill pins its own skill"
        );
        let mut off = c.clone();
        off.jev.points.skills = false;
        assert!(!skill_catalog_wanted(&off, false, true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_skill_catalog_is_read_inside_the_runtime() {
        // The registry loads behind a blocking lock; turn start runs on the
        // async runtime, where blocking panics unless it is moved off.
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let host = crate::agent_host::DaemonHostFunctions::new(
            Arc::new(crate::config::AgentConfig::default()),
            tokio::runtime::Handle::current(),
        )
        .with_services(crate::wasm::services::HostServices::new(db));
        let agent = nevoflux_builtin_wasm::Agent::new(host);
        let _ = skill_catalog(&agent, None);
    }

    #[test]
    fn a_run_that_cannot_load_skills_is_offered_none() {
        // A soul without `skill_load`, a toolless soul, an evolve run: the
        // agent would decline the injection, so Jev is not asked at all.
        let c = cfg("http://127.0.0.1:1", 2000);
        assert!(!skill_catalog_wanted(&c, false, false));
    }

    #[test]
    fn thinking_providers_get_no_skill_injection() {
        // The made-up skill step is an assistant tool call with no
        // reasoning or signature: DeepSeek/MiMo thinking and Gemini 3 reject
        // it. Anthropic (thinking never requested) is fine.
        let mut c = cfg("http://127.0.0.1:1", 2000);
        assert!(skills_point_on(&c));
        c.llm.provider = Some("deepseek".into());
        assert!(!skills_point_on(&c));
        c.llm.provider = Some("gemini".into());
        assert!(!skills_point_on(&c));
    }
}
