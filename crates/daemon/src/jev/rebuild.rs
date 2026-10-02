//! Rebuild (spec §5.6.4, §5.7): at turn start, keep the history as graded
//! before or re-grade its chunks against the new query, whichever the
//! economics favour; a Jev failure keeps (§5.8).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use nevoflux_builtin_wasm::Message;
use nevoflux_llm::ProviderType;
use nevoflux_protocol::session_event::{SessionEvent, SessionEventPayload};

use super::client;
use super::economics::{
    cache_rate, decide, keep_cost, rebuild_cost, remaining_requests, warm, Decision,
};
use super::history::{derive_history, grades, window, Grade, HistoryOpts};
use super::oracle::{DecisionOracle, JevOracle, OracleContext, Verdict};
use super::visibility::{combine, parts, pseudo_lines, questions, Level, SMALL};
use crate::session_events::SessionEventWriter;
use crate::turn_stats::{estimate_tokens, TurnStats};

/// Chunks re-graded concurrently.
const REGRADE_CONCURRENCY: usize = 8;
/// The query as sent in the H question.
const QUERY_CHARS: usize = 1_000;

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

/// Re-grade the window's Jev-graded chunks against `query` (spec §5.6.4:
/// full text, groups under 64k, in parallel). Chunks graded `sensitive` or
/// `fallback` were never sent and are not now. Each new grade is logged as
/// `jev/visibility{graded_by:"rebuild"}`. `Err` on any Jev fallback.
pub async fn regrade(
    env: &RebuildEnv<'_>,
    query: &str,
) -> Result<(HashMap<String, Grade>, u32, f64), String> {
    let old = grades(&env.events);
    let results: HashMap<&str, &str> = env
        .events
        .iter()
        .filter_map(|e| match &e.payload {
            SessionEventPayload::ToolResult { id, content, .. } => {
                Some((id.as_str(), content.as_str()))
            }
            _ => None,
        })
        .collect();
    let todo: Vec<(String, Grade, String)> = window(&env.events, u32::MAX, &old)
        .into_iter()
        .filter(|(_, g)| g.graded_by == "jev" || g.graded_by == "rebuild")
        .filter_map(|(call, g)| {
            let content = results.get(call.as_str())?;
            (content.len() > SMALL).then(|| (call, g, content.to_string()))
        })
        .collect();
    if todo.is_empty() {
        return Ok((HashMap::new(), 0, 0.0));
    }
    let oracle = oracle(env).ok_or("jev not configured")?;
    let timeout = Duration::from_millis(env.jev.timeout_ms);
    let graded: Vec<Result<(String, Grade, f64, u64, u64), String>> = stream::iter(todo)
        .map(|(call, g, content)| {
            let oracle = &oracle;
            async move {
                let started = Instant::now();
                let lines = pseudo_lines(&content);
                let (ps, _) = parts(&lines, query);
                let ctx = OracleContext::no_page("rebuild", timeout);
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
                    chunk: g.chunk.clone(),
                    level: grade.level,
                    kept: if grade.level == Level::Long {
                        grade.kept
                    } else {
                        Vec::new()
                    },
                    graded_by: "rebuild".to_string(),
                };
                Ok((
                    call,
                    new,
                    spent,
                    content.len() as u64,
                    started.elapsed().as_millis() as u64,
                ))
            }
        })
        .buffer_unordered(REGRADE_CONCURRENCY)
        .collect()
        .await;
    let mut out = HashMap::new();
    let mut spent = 0.0;
    let mut logs = Vec::new();
    for r in graded {
        let (call, g, cost, bytes, ms) = r?;
        spent += cost;
        logs.push((call.clone(), g.clone(), bytes, ms));
        out.insert(call, g);
    }
    if let Some(w) = &env.writer {
        let tools: HashMap<&str, &str> = env
            .events
            .iter()
            .filter_map(|e| match &e.payload {
                SessionEventPayload::ToolCall { id, name, .. } => {
                    Some((id.as_str(), name.as_str()))
                }
                _ => None,
            })
            .collect();
        for (call, g, bytes, ms) in logs {
            w.append(SessionEventPayload::JevVisibility {
                id: g.chunk.clone(),
                tool: tools.get(call.as_str()).unwrap_or(&"").to_string(),
                bytes,
                level: g.level.as_str().to_string(),
                graded_by: g.graded_by.clone(),
                kept_lines: g.kept.iter().map(|r| r.len() as u64).sum(),
                elapsed_ms: ms,
                call_id: Some(call),
                kept: g
                    .kept
                    .iter()
                    .map(|r| [r.start as u64, r.end as u64])
                    .collect(),
            });
        }
    }
    let n = out.len() as u32;
    Ok((out, n, spent))
}

fn last_request_ms(events: &[SessionEvent]) -> Option<i64> {
    events
        .iter()
        .rev()
        .find(|e| matches!(e.payload, SessionEventPayload::AssistantMessage { .. }))
        .map(|e| e.ts)
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

/// Turn start with Jev on: the earlier turns to send, re-graded when the
/// economics favour it (forced by an expired cache), kept otherwise.
pub async fn history_for_turn(env: &RebuildEnv<'_>) -> Vec<Message> {
    let old = grades(&env.events);
    let keep = derive_history(&env.events, u32::MAX, &old, &env.opts);
    if keep.is_empty() {
        return keep;
    }
    let p = tokens(&keep);
    let h = remaining_requests(ask_h(env).await);
    let rate = cache_rate(env.wire, &env.jev.cache);
    let is_warm = warm(last_request_ms(&env.events), env.now_ms, rate);
    let keep_c = keep_cost(p as f64, h, rate, is_warm);
    let reason = if is_warm {
        "cost_formula"
    } else {
        "ttl_expired"
    };
    if is_warm {
        // The best a rebuild could do: every window chunk shown short.
        let mut floor: HashMap<String, Grade> = old.clone();
        for (call, g) in window(&env.events, u32::MAX, &old) {
            floor.insert(
                call,
                Grade {
                    level: Level::Short,
                    kept: Vec::new(),
                    ..g
                },
            );
        }
        let a_min = tokens(&derive_history(&env.events, u32::MAX, &floor, &env.opts));
        if decide(keep_c, rebuild_cost(a_min as f64, h, rate, 0.0), false) == Decision::Keep {
            return keep;
        }
    }
    match regrade(env, env.query).await {
        Err(_) => {
            log_rebuild(env, reason, "keep_fallback", keep_c, keep_c, h, p, p, 0);
            keep
        }
        Ok((new, n, jev_tokens)) => {
            let mut merged = old;
            merged.extend(new);
            let rebuilt = derive_history(&env.events, u32::MAX, &merged, &env.opts);
            let a = tokens(&rebuilt);
            let rebuild_c = rebuild_cost(a as f64, h, rate, jev_tokens);
            match decide(keep_c, rebuild_c, false) {
                Decision::Rebuild => {
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

/// Mid-turn, the earlier turns polluted (spec §5.7): re-grade them against
/// the current query and rebuild when that costs at most ρ more than
/// keeping. The cache is warm mid-turn. `env.events` must hold the earlier
/// turns only. `None` keeps them (also on a Jev failure, §5.8).
pub async fn polluted_rebuild(env: &RebuildEnv<'_>, h: u32) -> Option<Vec<Message>> {
    let old = grades(&env.events);
    let keep = derive_history(&env.events, u32::MAX, &old, &env.opts);
    if keep.is_empty() {
        return None;
    }
    let p = tokens(&keep);
    let rate = cache_rate(env.wire, &env.jev.cache);
    let keep_c = keep_cost(p as f64, h, rate, true);
    match regrade(env, env.query).await {
        Err(_) => {
            log_rebuild(env, "polluted", "keep_fallback", keep_c, keep_c, h, p, p, 0);
            None
        }
        Ok((new, n, jev_tokens)) => {
            let mut merged = old;
            merged.extend(new);
            let rebuilt = derive_history(&env.events, u32::MAX, &merged, &env.opts);
            let a = tokens(&rebuilt);
            let rebuild_c = rebuild_cost(a as f64, h, rate, jev_tokens);
            match decide(keep_c, rebuild_c, true) {
                Decision::Rebuild => {
                    log_rebuild(env, "polluted", "rebuild", keep_c, rebuild_c, h, p, a, n);
                    Some(rebuilt)
                }
                Decision::Keep => {
                    log_rebuild(env, "polluted", "keep", keep_c, rebuild_c, h, p, p, n);
                    None
                }
            }
        }
    }
}

/// Earlier turns from the session log, kept or rebuilt for `query` — the
/// Jev history path shared by chat and tasks. `None` when the log cannot be
/// read or the provider has no wire.
pub async fn history_from_log(
    cfg: &crate::config::AgentConfig,
    database: &Arc<nevoflux_storage::Database>,
    session_id: &str,
    query: &str,
    max_messages: usize,
) -> Option<Vec<Message>> {
    let events = nevoflux_storage::repositories::SessionEventRepository::new(database)
        .list(session_id)
        .ok()?;
    let wire = cfg
        .llm
        .active_provider()
        .and_then(|p| cfg.llm.resolve_wire(p))?;
    let writer = Arc::new(SessionEventWriter::new(
        database.clone(),
        session_id.to_string(),
    ));
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let env = RebuildEnv {
        jev: &cfg.jev,
        wire,
        events,
        query,
        writer: Some(writer),
        stats: None,
        opts: HistoryOpts {
            max_messages,
            max_bytes: 32_000,
        },
        now_ms,
    };
    Some(history_for_turn(&env).await)
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
            jev: j,
            wire: ProviderType::Anthropic,
            events,
            query: "what changed in the log?",
            writer: Some(w),
            stats: None,
            opts: HistoryOpts {
                max_messages: 50,
                max_bytes: 32_000,
            },
            now_ms: NOW,
        };
        history_for_turn(&env).await
    }

    #[tokio::test]
    async fn a_warm_cache_with_a_cheap_keep_does_not_regrade() {
        let (url, bodies) = answering(answer("hide", "1"), Duration::ZERO).await;
        let (w, db) = writer();
        let events = one_turn(NOW - 60_000, "jev", &hundred_lines("a"));
        let expected = derive_history(
            &events,
            u32::MAX,
            &grades(&events),
            &HistoryOpts {
                max_messages: 50,
                max_bytes: 32_000,
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

    async fn polluted(level: &str) -> (Option<Vec<Message>>, Vec<SessionEventPayload>) {
        let (url, _) = answering(answer(level, "1"), Duration::ZERO).await;
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
            },
            now_ms: NOW,
        };
        let out = polluted_rebuild(&env, 3).await;
        (out, logged(&db))
    }

    #[tokio::test]
    async fn a_polluted_rebuild_respects_rho() {
        // Everything hidden: a much smaller history → rebuild.
        let (out, ev) = polluted("hide").await;
        assert!(out.is_some());
        assert!(ev.iter().any(|e| matches!(e,
            SessionEventPayload::ContextRebuild { reason, decision, .. } if reason == "polluted" && decision == "rebuild")));
        // Everything kept full: rebuilding re-writes the whole prefix, far
        // more than keep·(1+ρ) on a warm cache → keep.
        let (out, ev) = polluted("full").await;
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
}
