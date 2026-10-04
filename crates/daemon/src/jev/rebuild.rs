//! Rebuild (spec §5.6.4, §5.7): at turn start, keep the history as graded
//! before or re-grade its chunks against the new query, whichever the
//! economics favour; a Jev failure keeps (§5.8).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt, TryStreamExt};
use nevoflux_builtin_wasm::Message;
use nevoflux_llm::ProviderType;
use nevoflux_protocol::session_event::{SessionEvent, SessionEventPayload};

use super::client;
use super::economics::{
    cache_rate, decide, keep_cost_cached, rebuild_cost, remaining_requests, warm, CacheRate,
    Decision,
};
use super::history::{derive_history, grades, window, Grade, HistoryOpts, TableTurn, WindowChunk};
use super::oracle::{DecisionOracle, JevOracle, OracleContext, Verdict};
use super::privacy::{scope_for, Scope};
use super::visibility::{combine, parts, pseudo_lines, questions, Level, SMALL};
use crate::session_events::SessionEventWriter;
use crate::turn_stats::{estimate_tokens, TurnStats};

/// Chunks re-graded concurrently.
const REGRADE_CONCURRENCY: usize = 8;
/// The query as sent in the H question.
const QUERY_CHARS: usize = 1_000;
/// A turn-start rebuild (H question plus re-grade) may take this many Jev
/// timeouts in all; past that the history is kept as it is.
const TURN_START_BUDGET: u32 = 3;
/// How far back from a cache breakpoint Anthropic looks for an earlier
/// cached prefix, in content blocks.
const LOOKBACK_BLOCKS: usize = 20;

/// What a turn-start rebuild works from.
pub struct RebuildEnv<'a> {
    pub jev: &'a crate::config::JevConfig,
    pub wire: ProviderType,
    /// The session log; the current turn is not in it yet.
    pub events: Vec<SessionEvent>,
    pub query: &'a str,
    pub writer: Option<Arc<SessionEventWriter>>,
    pub stats: Option<Arc<TurnStats>>,
    pub opts: HistoryOpts,
    pub now_ms: i64,
    /// A rebuild the caller forces (`tool_change`: the tools changed, so the
    /// cache is rewritten anyway). The cache is priced cold and the logged
    /// reason is this one; the economics still decide keep or rebuild.
    pub forced: Option<&'static str>,
}

/// Jev, its rebuild point, egress and a cloud provider: the conditions for
/// taking history from the log (spec §5.2, §3.2).
pub fn rebuild_point_on(cfg: &crate::config::AgentConfig) -> bool {
    let jev = &cfg.jev;
    let cloud = cfg
        .llm
        .active_provider()
        .and_then(|p| cfg.llm.resolve_wire(p))
        .is_some_and(|w| w != ProviderType::Local);
    jev.is_usable()
        && jev.points.rebuild
        && cloud
        && client::egress_allowed(crate::local::latch::is_on(), &jev.endpoint)
}

/// Providers that want each assistant tool-call message's reasoning back,
/// which the log does not keep: history goes to them as text only.
pub fn needs_reasoning_back(cfg: &crate::config::AgentConfig) -> bool {
    let Some(p) = cfg.llm.active_provider() else {
        return false;
    };
    cfg.llm.resolve_wire(p) == Some(ProviderType::DeepSeek)
        || cfg
            .llm
            .base_url_for_provider(p)
            .is_some_and(|u| u.contains("xiaomimimo.com"))
}

/// Estimated tokens of a history.
pub fn tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| {
            estimate_tokens(&m.content)
                + m.tool_calls
                    .iter()
                    .map(|c| estimate_tokens(&c.arguments.to_string()) + 8)
                    .sum::<u64>()
        })
        .sum()
}

/// Content blocks a provider counts for these messages (text, each tool use).
fn blocks(messages: &[Message]) -> usize {
    messages.iter().map(|m| 1 + m.tool_calls.len()).sum()
}

/// Tokens of `now` the provider can still read from cache: the part that is
/// byte-identical to `before` (the history the previous turn cached), and
/// only when the new history end is within the lookback of that entry.
pub fn cached_prefix(before: &[Message], now: &[Message]) -> u64 {
    let same = before
        .iter()
        .zip(now)
        .take_while(|(a, b)| serde_json::to_string(a).ok() == serde_json::to_string(b).ok())
        .count();
    if same == 0 || same < before.len() || blocks(&now[same..]) > LOOKBACK_BLOCKS {
        return 0;
    }
    tokens(&now[..same])
}

fn oracle(env: &RebuildEnv<'_>) -> Option<JevOracle> {
    let c = client::shared(env.jev).ok()?;
    Some(JevOracle::new(
        c,
        env.jev.sensitive_domains.clone(),
        env.writer.clone(),
        env.stats.clone(),
    ))
}

/// Remaining tool steps for the new query (one Score question), or `None`.
async fn ask_h(env: &RebuildEnv<'_>) -> Option<u32> {
    let oracle = oracle(env)?;
    let mut q = super::signals::questions();
    q.retain(|k, _| k == "remaining_steps");
    let state = serde_json::json!({
        "query": env.query.chars().take(QUERY_CHARS).collect::<String>(),
        "step": 0,
    });
    let ctx = OracleContext::no_page("rebuild", Duration::from_millis(env.jev.timeout_ms));
    match oracle.ask(&ctx, state, q).await {
        Verdict::Answered(r) => super::signals::parse(0, &r).h,
        Verdict::Fallback { .. } => None,
    }
}

/// The window's chunks a re-grade may send: graded by Jev before (never
/// `sensitive` or `fallback`), over the small threshold, and from no page
/// that is sensitive now.
fn regradable(env: &RebuildEnv<'_>, old: &HashMap<String, Grade>) -> Vec<WindowChunk> {
    window(&env.events, u32::MAX, old)
        .into_iter()
        .filter(|w| w.grade.graded_by == "jev" || w.grade.graded_by == "rebuild")
        .filter(|w| w.content.len() > SMALL)
        .filter(|w| {
            w.grade
                .pages
                .iter()
                .all(|u| scope_for(u, &env.jev.sensitive_domains) == Scope::Full)
        })
        .collect()
}

/// New grades from a re-grade, not yet logged: they become the history's
/// grades only if the rebuild is taken.
pub struct Regraded {
    pub grades: HashMap<String, Grade>,
    pub jev_tokens: f64,
    logs: Vec<(WindowChunk, Grade, u64)>,
}

/// Re-grade `todo` against `query` (spec §5.6.4: full text, groups under
/// 64k, in parallel). `Err` at the first Jev fallback.
async fn regrade(
    env: &RebuildEnv<'_>,
    todo: Vec<WindowChunk>,
    query: &str,
) -> Result<Regraded, String> {
    let oracle = oracle(env).ok_or("jev not configured")?;
    let timeout = Duration::from_millis(env.jev.timeout_ms);
    let graded: Vec<(WindowChunk, Grade, f64, u64)> = stream::iter(todo)
        .map(|w| {
            let oracle = &oracle;
            async move {
                let started = Instant::now();
                let lines = pseudo_lines(&w.content);
                let (ps, _) = parts(&lines, query);
                let ctx = match w.grade.pages.first() {
                    Some(u) => OracleContext::page("rebuild", u.clone(), timeout),
                    None => OracleContext::no_page("rebuild", timeout),
                };
                let mut answers = Vec::new();
                let mut spent = 0.0;
                for p in &ps {
                    let (state, qs) = questions(query, p);
                    match oracle.ask(&ctx, state, qs).await {
                        Verdict::Answered(r) => {
                            spent += (r.usage.input_tokens + r.usage.output_tokens) as f64;
                            answers.push(r);
                        }
                        Verdict::Fallback { reason } => return Err(reason),
                    }
                }
                let grade = combine(&ps, &answers);
                let new = Grade {
                    chunk: w.grade.chunk.clone(),
                    level: grade.level,
                    kept: if grade.level == Level::Long {
                        grade.kept
                    } else {
                        Vec::new()
                    },
                    graded_by: "rebuild".to_string(),
                    pages: w.grade.pages.clone(),
                };
                Ok((w, new, spent, started.elapsed().as_millis() as u64))
            }
        })
        .buffer_unordered(REGRADE_CONCURRENCY)
        .try_collect()
        .await?;
    let mut out = Regraded {
        grades: HashMap::new(),
        jev_tokens: 0.0,
        logs: Vec::new(),
    };
    for (w, g, cost, ms) in graded {
        out.jev_tokens += cost;
        out.grades.insert(g.chunk.clone(), g.clone());
        out.logs.push((w, g, ms));
    }
    Ok(out)
}

/// Log a taken re-grade's grades as `jev/visibility{graded_by:"rebuild"}`.
fn persist(env: &RebuildEnv<'_>, r: &Regraded) {
    let Some(w) = &env.writer else { return };
    for (chunk, g, ms) in &r.logs {
        w.append(SessionEventPayload::JevVisibility {
            id: g.chunk.clone(),
            tool: chunk.tool.clone(),
            bytes: chunk.content.len() as u64,
            level: g.level.as_str().to_string(),
            graded_by: g.graded_by.clone(),
            kept_lines: g.kept.iter().map(|r| r.len() as u64).sum(),
            elapsed_ms: *ms,
            call_id: Some(chunk.call_id.clone()),
            kept: g
                .kept
                .iter()
                .map(|r| [r.start as u64, r.end as u64])
                .collect(),
            pages: g.pages.clone(),
        });
    }
}

pub(crate) fn last_request_ms(events: &[SessionEvent]) -> Option<i64> {
    events
        .iter()
        .rev()
        .find(|e| matches!(e.payload, SessionEventPayload::AssistantMessage { .. }))
        .map(|e| e.ts)
}

/// The number of the last turn in the log (a subagent's nested turn aside).
fn last_turn(events: &[SessionEvent]) -> Option<u32> {
    let i = super::history::current_turn_start(events)?;
    match events[i].payload {
        SessionEventPayload::TurnStart { turn } => Some(turn),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn log_rebuild(
    env: &RebuildEnv<'_>,
    reason: &str,
    decision: &str,
    keep: f64,
    rebuild: f64,
    h: u32,
    before: u64,
    after: u64,
    regraded: u32,
) {
    if let Some(w) = &env.writer {
        w.append(SessionEventPayload::ContextRebuild {
            reason: reason.to_string(),
            decision: decision.to_string(),
            keep_cost: keep,
            rebuild_cost: rebuild,
            h,
            before_tokens: before,
            after_tokens: after,
            regraded,
        });
    }
}

/// Keeping `keep` for `h` requests: what the previous turn cached is read,
/// the rest is written.
fn keep_price(
    env: &RebuildEnv<'_>,
    keep: &[Message],
    old: &HashMap<String, Grade>,
    h: u32,
    rate: CacheRate,
) -> f64 {
    let p = tokens(keep);
    let cached = if env.forced.is_none() && warm(last_request_ms(&env.events), env.now_ms, rate) {
        let before = last_turn(&env.events)
            .map(|t| derive_history(&env.events, t, old, &env.opts))
            .unwrap_or_default();
        cached_prefix(&before, keep)
    } else {
        0
    };
    keep_cost_cached(p as f64, cached as f64, h, rate)
}

/// Turn start with Jev on: the earlier turns to send, re-graded when the
/// economics favour it (forced by an expired cache), kept otherwise.
pub async fn history_for_turn(env: &RebuildEnv<'_>) -> Vec<Message> {
    let old = grades(&env.events);
    let keep = derive_history(&env.events, u32::MAX, &old, &env.opts);
    if keep.is_empty() {
        return keep;
    }
    let p = tokens(&keep);
    let todo = regradable(env, &old);
    if todo.is_empty() {
        // Nothing a re-grade could change: no reason to ask Jev anything.
        return keep;
    }
    let budget = Duration::from_millis(env.jev.timeout_ms) * TURN_START_BUDGET;
    let deadline = tokio::time::Instant::now() + budget;
    let h = remaining_requests(
        tokio::time::timeout_at(deadline, ask_h(env))
            .await
            .ok()
            .flatten(),
    );
    let rate = cache_rate(env.wire, &env.jev.cache);
    let is_warm = env.forced.is_none() && warm(last_request_ms(&env.events), env.now_ms, rate);
    let keep_c = keep_price(env, &keep, &old, h, rate);
    let reason = env.forced.unwrap_or(if is_warm {
        "cost_formula"
    } else {
        "ttl_expired"
    });
    if is_warm {
        // The best a rebuild could do: every window chunk shown short.
        let mut floor: HashMap<String, Grade> = old.clone();
        for w in window(&env.events, u32::MAX, &old) {
            floor.insert(
                w.grade.chunk.clone(),
                Grade {
                    level: Level::Short,
                    kept: Vec::new(),
                    ..w.grade
                },
            );
        }
        let a_min = tokens(&derive_history(&env.events, u32::MAX, &floor, &env.opts));
        if decide(keep_c, rebuild_cost(a_min as f64, h, rate, 0.0), false) == Decision::Keep {
            return keep;
        }
    }
    let n = todo.len() as u32;
    match tokio::time::timeout_at(deadline, regrade(env, todo, env.query)).await {
        Err(_) | Ok(Err(_)) => {
            log_rebuild(env, reason, "keep_fallback", keep_c, keep_c, h, p, p, 0);
            keep
        }
        Ok(Ok(new)) => {
            let mut merged = old;
            merged.extend(new.grades.clone());
            let rebuilt = derive_history(&env.events, u32::MAX, &merged, &env.opts);
            let a = tokens(&rebuilt);
            let rebuild_c = rebuild_cost(a as f64, h, rate, new.jev_tokens);
            match decide(keep_c, rebuild_c, false) {
                Decision::Rebuild => {
                    persist(env, &new);
                    log_rebuild(env, reason, "rebuild", keep_c, rebuild_c, h, p, a, n);
                    rebuilt
                }
                Decision::Keep => {
                    log_rebuild(env, reason, "keep", keep_c, rebuild_c, h, p, p, n);
                    keep
                }
            }
        }
    }
}

/// Mid-turn rebuild of the earlier turns (spec §5.7), re-graded against the
/// current query. `polluted`: rebuild when that costs at most ρ more than
/// keeping a warm cache; a rebuild also re-writes this turn's
/// `current_tokens`. `tool_change`: the tools changed, so the cache is
/// rewritten either way — both options pay to write (no current-turn term)
/// and the plain cost formula decides. `env.events` must hold the earlier
/// turns only. `None` keeps them (also on a Jev failure, §5.8).
pub async fn mid_turn_rebuild(
    env: &RebuildEnv<'_>,
    h: u32,
    current_tokens: u64,
    reason: &'static str,
) -> Option<Vec<Message>> {
    let polluted = reason == "polluted";
    let old = grades(&env.events);
    let keep = derive_history(&env.events, u32::MAX, &old, &env.opts);
    if keep.is_empty() {
        return None;
    }
    let p = tokens(&keep);
    let rate = cache_rate(env.wire, &env.jev.cache);
    let cached = if polluted { p as f64 } else { 0.0 };
    let keep_c = keep_cost_cached(p as f64, cached, h, rate);
    let todo = regradable(env, &old);
    if todo.is_empty() {
        return None;
    }
    let n = todo.len() as u32;
    match regrade(env, todo, env.query).await {
        Err(_) => {
            log_rebuild(env, reason, "keep_fallback", keep_c, keep_c, h, p, p, 0);
            None
        }
        Ok(new) => {
            let mut merged = old;
            merged.extend(new.grades.clone());
            let rebuilt = derive_history(&env.events, u32::MAX, &merged, &env.opts);
            let a = tokens(&rebuilt);
            let rewrite = if polluted {
                current_tokens as f64 * (rate.write - rate.read).max(0.0)
            } else {
                0.0
            };
            let rebuild_c = rebuild_cost(a as f64, h, rate, new.jev_tokens) + rewrite;
            match decide(keep_c, rebuild_c, polluted) {
                Decision::Rebuild => {
                    persist(env, &new);
                    log_rebuild(env, reason, "rebuild", keep_c, rebuild_c, h, p, a, n);
                    Some(rebuilt)
                }
                Decision::Keep => {
                    log_rebuild(env, reason, "keep", keep_c, rebuild_c, h, p, p, n);
                    None
                }
            }
        }
    }
}

/// The messages table's earlier rows as [`TableTurn`]s: each user row, and
/// the soul that answered it when that is not `active` (named by
/// `display_name`, as `convert_history_messages` does).
pub fn table_turns(
    rows: &[nevoflux_storage::Message],
    active: Option<&str>,
    display_name: &dyn Fn(&str) -> String,
) -> Vec<TableTurn> {
    let mut out: Vec<TableTurn> = Vec::new();
    for m in rows {
        match m.role {
            nevoflux_storage::MessageRole::User => out.push(TableTurn {
                user: m.content.clone(),
                other_speaker: None,
            }),
            nevoflux_storage::MessageRole::Assistant
                if m.content_type != nevoflux_storage::ContentType::ToolUse =>
            {
                let persona = m
                    .metadata
                    .as_ref()
                    .and_then(|md| md.get(crate::server::PERSONA_METADATA_KEY))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                if let (Some(a), Some(last)) = (active, out.last_mut()) {
                    if persona != Some(a) {
                        last.other_speaker = Some(
                            persona
                                .map(display_name)
                                .unwrap_or_else(|| "assistant".into()),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The user rows of a text history (a task's caller-supplied history).
pub fn table_from_text(history: &[Message]) -> Vec<TableTurn> {
    history
        .iter()
        .filter(|m| matches!(m.role, nevoflux_builtin_wasm::MessageRole::User))
        .map(|m| TableTurn {
            user: m.content.clone(),
            other_speaker: None,
        })
        .collect()
}

/// History options for the log path under `cfg`.
pub fn history_opts(
    cfg: &crate::config::AgentConfig,
    max_messages: usize,
    table: Vec<TableTurn>,
) -> HistoryOpts {
    HistoryOpts {
        max_messages,
        max_bytes: 32_000,
        text_only: needs_reasoning_back(cfg),
        table,
    }
}

/// The log's history when it has turns, the caller's otherwise (a task's
/// caller may supply history the log never saw, e.g. A2A).
pub fn prefer_log(log: Option<Vec<Message>>, text: Vec<Message>) -> Vec<Message> {
    match log {
        Some(h) if !h.is_empty() => h,
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::economics::DEFAULT_H;
    use crate::jev::test_support::answering;
    use serde_json::json;

    const NOW: i64 = 1_790_000_000_000;

    fn log_at(lines: Vec<(i64, serde_json::Value)>) -> Vec<SessionEvent> {
        lines
            .into_iter()
            .enumerate()
            .map(|(i, (ts, v))| SessionEvent {
                seq: i as i64 + 1,
                ts,
                payload: serde_json::from_value(v).unwrap(),
            })
            .collect()
    }

    fn hundred_lines(tag: &str) -> String {
        (0..100)
            .map(|i| format!("{:<59}\n", format!("{tag} line {i}")))
            .collect()
    }

    /// One earlier turn: a graded 6 KB read, the answer at `answered_ms`.
    fn one_turn(answered_ms: i64, graded_by: &str, content: &str) -> Vec<SessionEvent> {
        log_at(vec![
            (NOW - 900_000, json!({"type": "turn/start", "turn": 1})),
            (
                NOW - 900_000,
                json!({"type": "user/message", "content": "read the log", "origin": "user"}),
            ),
            (
                NOW - 899_000,
                json!({"type": "assistant/message", "content": "", "tool_calls": [{"id": "t1", "name": "read", "args": {}}], "model": "m", "provider": "p"}),
            ),
            (
                NOW - 899_000,
                json!({"type": "tool/call", "id": "t1", "name": "read", "args": {}, "origin": "model"}),
            ),
            (
                NOW - 899_000,
                json!({"type": "tool/result", "id": "t1", "content": content, "is_error": false, "duration_ms": 1}),
            ),
            (
                NOW - 899_000,
                json!({"type": "jev/visibility", "id": "c1", "tool": "read", "bytes": content.len(), "level": "full",
                                    "graded_by": graded_by, "kept_lines": 0, "elapsed_ms": 1, "call_id": "t1"}),
            ),
            (
                answered_ms,
                json!({"type": "assistant/message", "content": "it is long", "tool_calls": [], "model": "m", "provider": "p"}),
            ),
        ])
    }

    /// A small follow-up turn `n`, answered at `answered_ms`.
    fn next_turn(n: u32, answered_ms: i64) -> Vec<SessionEvent> {
        log_at(vec![
            (
                answered_ms - 1_000,
                json!({"type": "turn/start", "turn": n}),
            ),
            (
                answered_ms - 1_000,
                json!({"type": "user/message", "content": "and the date?", "origin": "user"}),
            ),
            (
                answered_ms,
                json!({"type": "assistant/message", "content": "monday", "tool_calls": [], "model": "m", "provider": "p"}),
            ),
        ])
    }

    /// One earlier turn with `n` graded reads, each from `page` when given.
    fn many_chunks(n: usize, answered_ms: i64, page: Option<&str>, tag: &str) -> Vec<SessionEvent> {
        let mut lines = vec![
            (NOW - 900_000, json!({"type": "turn/start", "turn": 1})),
            (
                NOW - 900_000,
                json!({"type": "user/message", "content": "read them", "origin": "user"}),
            ),
        ];
        for i in 0..n {
            let id = format!("t{i}");
            let content = hundred_lines(tag);
            let pages: Vec<&str> = page.into_iter().collect();
            lines.push((NOW - 899_000, json!({"type": "assistant/message", "content": "", "tool_calls": [{"id": id, "name": "read", "args": {}}], "model": "m", "provider": "p"})));
            lines.push((NOW - 899_000, json!({"type": "tool/call", "id": id, "name": "read", "args": {}, "origin": "model"})));
            lines.push((NOW - 899_000, json!({"type": "tool/result", "id": id, "content": content, "is_error": false, "duration_ms": 1})));
            lines.push((NOW - 899_000, json!({"type": "jev/visibility", "id": format!("c{i}"), "tool": "read", "bytes": 6000, "level": "full",
                "graded_by": "jev", "kept_lines": 0, "elapsed_ms": 1, "call_id": id, "pages": pages})));
        }
        lines.push((answered_ms, json!({"type": "assistant/message", "content": "read", "tool_calls": [], "model": "m", "provider": "p"})));
        log_at(lines)
    }

    fn jev_cfg(url: &str, timeout_ms: u64) -> crate::config::JevConfig {
        let mut j = crate::config::JevConfig::default();
        j.enabled = true;
        j.endpoint = url.to_string();
        j.api_key = "k".into();
        j.timeout_ms = timeout_ms;
        j
    }

    fn answer(level: &str, steps_index: &str) -> serde_json::Value {
        json!({"answers": {
            "visibility": {"choice": level, "probabilities": {}},
            "b000": {"noul": 0.0}, "b001": {"noul": 0.0}, "b002": {"noul": 0.0}, "b003": {"noul": 0.0},
            "remaining_steps": {"type": "score", "probabilities": {steps_index: 1.0}}
        }, "usage": {"input_tokens": 100, "output_tokens": 5}})
    }

    fn writer() -> (Arc<SessionEventWriter>, Arc<nevoflux_storage::Database>) {
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        (
            Arc::new(SessionEventWriter::new(db.clone(), "s1".into())),
            db,
        )
    }

    fn logged(db: &nevoflux_storage::Database) -> Vec<SessionEventPayload> {
        nevoflux_storage::repositories::SessionEventRepository::new(db)
            .list("s1")
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect()
    }

    async fn run(
        j: &crate::config::JevConfig,
        events: Vec<SessionEvent>,
        w: Arc<SessionEventWriter>,
    ) -> Vec<Message> {
        let env = RebuildEnv {
            forced: None,
            jev: j,
            wire: ProviderType::Anthropic,
            events,
            query: "what changed in the log?",
            writer: Some(w),
            stats: None,
            opts: HistoryOpts {
                max_messages: 50,
                max_bytes: 32_000,
                ..HistoryOpts::default()
            },
            now_ms: NOW,
        };
        history_for_turn(&env).await
    }

    #[tokio::test]
    async fn a_warm_cache_with_a_cheap_keep_does_not_regrade() {
        let (url, bodies) = answering(answer("hide", "1"), Duration::ZERO).await;
        let (w, db) = writer();
        let mut events = one_turn(NOW - 120_000, "jev", &hundred_lines("a"));
        events.extend(next_turn(2, NOW - 60_000));
        let expected = derive_history(
            &events,
            u32::MAX,
            &grades(&events),
            &HistoryOpts {
                max_messages: 50,
                max_bytes: 32_000,
                ..HistoryOpts::default()
            },
        );
        let h = run(&jev_cfg(&url, 2000), events, w).await;
        assert_eq!(h.len(), expected.len());
        assert_eq!(h[2].content, expected[2].content);
        assert_eq!(bodies.lock().unwrap().len(), 1, "only the H question");
        assert!(!logged(&db)
            .iter()
            .any(|p| matches!(p, SessionEventPayload::ContextRebuild { .. })));
    }

    #[tokio::test]
    async fn an_expired_cache_regrades_and_logs_the_rebuild() {
        let (url, _) = answering(answer("hide", "1"), Duration::ZERO).await;
        let (w, db) = writer();
        let events = one_turn(NOW - 600_000, "jev", &hundred_lines("a"));
        let h = run(&jev_cfg(&url, 2000), events, w).await;
        let p = logged(&db);
        assert!(
            p.iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { reason, decision, regraded: 1, .. }
                if reason == "ttl_expired" && decision == "rebuild")),
            "{p:?}"
        );
        assert!(p.iter().any(|e| matches!(e,
            SessionEventPayload::JevVisibility { graded_by, call_id: Some(c), level, .. }
                if graded_by == "rebuild" && c == "t1" && level == "hide")));
        assert!(
            h.iter().all(|m| m.tool_call_id.is_none()),
            "the hidden pair is gone"
        );
        assert!(h
            .iter()
            .any(|m| m.content.contains("· hidden] recall(\"c1\")")));
    }

    #[tokio::test]
    async fn a_failed_regrade_keeps_the_history() {
        let (url, _) = answering(answer("hide", "1"), Duration::from_millis(3000)).await;
        let (w, db) = writer();
        let events = one_turn(NOW - 600_000, "jev", &hundred_lines("a"));
        let h = run(&jev_cfg(&url, 200), events, w).await;
        assert!(
            h.iter().any(|m| m.tool_call_id.as_deref() == Some("t1")),
            "kept as before"
        );
        assert!(logged(&db).iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { decision, .. } if decision == "keep_fallback")));
    }

    #[tokio::test]
    async fn sensitive_and_fallback_chunks_are_never_sent() {
        let (url, bodies) = answering(answer("hide", "1"), Duration::ZERO).await;
        let (w, _db) = writer();
        let events = one_turn(NOW - 600_000, "sensitive", &hundred_lines("SECRET"));
        run(&jev_cfg(&url, 2000), events, w).await;
        assert!(!bodies.lock().unwrap().join("\n").contains("SECRET"));
    }

    #[tokio::test]
    async fn h_uses_jev_plus_one_or_the_default() {
        // P25 at level index 1 ("2" steps) → H = 3 requests.
        let (url, _) = answering(answer("hide", "1"), Duration::ZERO).await;
        let (w, db) = writer();
        run(
            &jev_cfg(&url, 2000),
            one_turn(NOW - 600_000, "jev", &hundred_lines("a")),
            w,
        )
        .await;
        assert!(logged(&db)
            .iter()
            .any(|e| matches!(e, SessionEventPayload::ContextRebuild { h: 3, .. })));

        let (url, _) = answering(answer("hide", "1"), Duration::from_millis(3000)).await;
        let (w, db) = writer();
        run(
            &jev_cfg(&url, 200),
            one_turn(NOW - 600_000, "jev", &hundred_lines("a")),
            w,
        )
        .await;
        assert!(logged(&db).iter().any(
            |e| matches!(e, SessionEventPayload::ContextRebuild { h, .. } if *h == DEFAULT_H)
        ));
    }

    async fn polluted(
        level: &str,
        current_tokens: u64,
    ) -> (Option<Vec<Message>>, Vec<SessionEventPayload>) {
        let (url, _) = answering(answer(level, "1"), Duration::ZERO).await;
        let (w, db) = writer();
        let j = jev_cfg(&url, 2000);
        let env = RebuildEnv {
            forced: None,
            jev: &j,
            wire: ProviderType::Anthropic,
            events: one_turn(NOW - 30_000, "jev", &hundred_lines("a")),
            query: "only the summary matters now",
            writer: Some(w),
            stats: None,
            opts: HistoryOpts {
                max_messages: 50,
                max_bytes: 32_000,
                ..HistoryOpts::default()
            },
            now_ms: NOW,
        };
        let out = mid_turn_rebuild(&env, 3, current_tokens, "polluted").await;
        (out, logged(&db))
    }

    #[tokio::test]
    async fn a_polluted_rebuild_respects_rho() {
        // Everything hidden: a much smaller history → rebuild.
        let (out, ev) = polluted("hide", 0).await;
        assert!(out.is_some());
        assert!(ev.iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { reason, decision, .. } if reason == "polluted" && decision == "rebuild")));
        // Everything kept full: rebuilding re-writes the whole prefix, far
        // more than keep·(1+ρ) on a warm cache → keep.
        let (out, ev) = polluted("full", 0).await;
        assert!(out.is_none());
        assert!(ev.iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { reason, decision, .. } if reason == "polluted" && decision == "keep")));
    }

    #[test]
    fn log_history_wins_only_when_it_has_turns() {
        let text = vec![Message::user("from the caller")];
        let log = vec![Message::user("from the log")];
        assert_eq!(
            prefer_log(Some(log.clone()), text.clone())[0].content,
            "from the log"
        );
        assert_eq!(
            prefer_log(Some(vec![]), text.clone())[0].content,
            "from the caller"
        );
        assert_eq!(prefer_log(None, text)[0].content, "from the caller");
    }

    #[test]
    fn the_rebuild_point_is_off_by_default() {
        assert!(!rebuild_point_on(&crate::config::AgentConfig::default()));
    }

    #[tokio::test]
    async fn a_regrade_that_keeps_logs_no_new_grades() {
        // Re-graded "full" again: nothing saved, so keep — and the rejected
        // grades must not become the next turn's.
        let (url, _) = answering(answer("full", "1"), Duration::ZERO).await;
        let (w, db) = writer();
        run(
            &jev_cfg(&url, 2000),
            one_turn(NOW - 600_000, "jev", &hundred_lines("a")),
            w,
        )
        .await;
        let p = logged(&db);
        assert!(
            p.iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { decision, .. } if decision == "keep")),
            "{p:?}"
        );
        assert!(
            !p.iter().any(|e| matches!(e,
            SessionEventPayload::JevVisibility { graded_by, .. } if graded_by == "rebuild")),
            "{p:?}"
        );
    }

    #[tokio::test]
    async fn nothing_to_regrade_asks_jev_nothing() {
        let (url, bodies) = answering(answer("hide", "1"), Duration::ZERO).await;
        let (w, _db) = writer();
        let h = run(
            &jev_cfg(&url, 2000),
            one_turn(NOW - 600_000, "sensitive", &hundred_lines("a")),
            w,
        )
        .await;
        assert!(!h.is_empty());
        assert_eq!(bodies.lock().unwrap().len(), 0, "not even the H question");
    }

    #[tokio::test]
    async fn a_slow_jev_is_cut_off_at_the_turn_start_budget() {
        // Every answer arrives within its own timeout, but 40 chunks take
        // five rounds: past the turn-start budget (3 timeouts) → keep.
        let (url, _) = answering(answer("hide", "1"), Duration::from_millis(150)).await;
        let (w, db) = writer();
        let started = Instant::now();
        let h = run(
            &jev_cfg(&url, 200),
            many_chunks(40, NOW - 600_000, None, "a"),
            w,
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "{:?}",
            started.elapsed()
        );
        assert!(!h.is_empty());
        let p = logged(&db);
        assert!(p.iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { decision, .. } if decision == "keep_fallback")));
        assert!(!p.iter().any(|e| matches!(e,
            SessionEventPayload::JevVisibility { graded_by, .. } if graded_by == "rebuild")));
    }

    #[tokio::test]
    async fn a_chunk_from_a_page_now_sensitive_is_not_regraded() {
        let (url, bodies) = answering(answer("hide", "1"), Duration::ZERO).await;
        let (w, _db) = writer();
        let mut j = jev_cfg(&url, 2000);
        j.sensitive_domains = vec!["bank.example".into()];
        run(
            &j,
            many_chunks(
                1,
                NOW - 600_000,
                Some("https://bank.example/statement"),
                "SECRET",
            ),
            w,
        )
        .await;
        assert!(!bodies.lock().unwrap().join("\n").contains("SECRET"));
    }

    #[tokio::test]
    async fn a_polluted_rebuild_counts_the_current_turn_it_rewrites() {
        // Hiding everything pays when only the history is priced, not when
        // a large current turn has to be written again too.
        let (out, ev) = polluted("hide", 200_000).await;
        assert!(out.is_none(), "{ev:?}");
    }

    #[test]
    fn only_the_previous_turns_history_is_cached() {
        let before = vec![Message::user("q1"), Message::assistant("a1")];
        let mut now = before.clone();
        now.push(Message::user("q2"));
        now.push(Message::assistant("a2"));
        assert_eq!(cached_prefix(&before, &now), tokens(&before));
        // The front changed: nothing after the system prompt is cached.
        let mut moved = now.clone();
        moved[0] = Message::user("q1 condensed");
        assert_eq!(cached_prefix(&before, &moved), 0);
        // The new history end is too far from the cached one.
        let mut far = before.clone();
        for i in 0..30 {
            far.push(Message::assistant(format!("a{i}")));
        }
        assert_eq!(cached_prefix(&before, &far), 0);
    }

    #[test]
    fn table_turns_name_another_souls_answer() {
        use nevoflux_storage::{ContentType, Message as Row, MessageRole as R};
        let row = |role, content: &str, persona: Option<&str>| Row {
            id: format!("m-{content}"),
            session_id: "s1".to_string(),
            role,
            content: content.to_string(),
            content_type: ContentType::Text,
            created_at: 0,
            metadata: persona.map(|p| {
                let mut md = std::collections::HashMap::new();
                md.insert(crate::server::PERSONA_METADATA_KEY.to_string(), json!(p));
                md
            }),
        };
        let rows = vec![
            row(R::User, "first", None),
            row(R::Assistant, "mine", Some("me")),
            row(R::User, "second", None),
            row(R::Assistant, "theirs", Some("research")),
        ];
        let t = table_turns(&rows, Some("me"), &|s| format!("Name:{s}"));
        assert_eq!(
            t[0],
            TableTurn {
                user: "first".into(),
                other_speaker: None
            }
        );
        assert_eq!(t[1].other_speaker.as_deref(), Some("Name:research"));
        assert!(table_turns(&rows, None, &|s| s.to_string())
            .iter()
            .all(|t| t.other_speaker.is_none()));
    }

    #[test]
    fn thinking_providers_get_text_only_history() {
        let mut cfg = crate::config::AgentConfig::default();
        cfg.llm.provider = Some("deepseek".into());
        assert!(needs_reasoning_back(&cfg));
        cfg.llm.provider = Some("anthropic".into());
        assert!(!needs_reasoning_back(&cfg));
    }

    #[tokio::test]
    async fn a_tool_change_rebuild_prices_the_cache_as_rewritten() {
        // With "polluted", a 200k current turn makes hiding not worth it
        // (a_polluted_rebuild_counts_the_current_turn_it_rewrites); with
        // "tool_change" the cache is rewritten anyway, so it is.
        let (url, _) = answering(answer("hide", "1"), Duration::ZERO).await;
        let (w, db) = writer();
        let j = jev_cfg(&url, 2000);
        let env = RebuildEnv {
            jev: &j,
            wire: ProviderType::Anthropic,
            events: one_turn(NOW - 30_000, "jev", &hundred_lines("a")),
            query: "only the summary matters now",
            writer: Some(w),
            stats: None,
            opts: HistoryOpts {
                max_messages: 50,
                max_bytes: 32_000,
                ..HistoryOpts::default()
            },
            now_ms: NOW,
            forced: None,
        };
        assert!(mid_turn_rebuild(&env, 3, 200_000, "tool_change")
            .await
            .is_some());
        assert!(logged(&db).iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { reason, decision, .. }
                if reason == "tool_change" && decision == "rebuild")));
    }
}
