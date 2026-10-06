//! Permissions with Jev on (spec §5.7, J14): Jev can require a confirmation
//! for a call the gate would let through. It never loosens.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use nevoflux_llm::ProviderType;
use nevoflux_protocol::session_event::{SessionEvent, SessionEventPayload};
use nevoflux_protocol::{classify_tool, RiskBucket};

use super::client;
use super::history::nested;
use super::oracle::{DecisionOracle, JevOracle, OracleContext, Verdict};
use super::wire::Question;
use crate::session_events::SessionEventWriter;
use crate::turn_stats::TurnStats;

/// Jev's risk at or above this flags a call (the tool `LOAD_P` value; the
/// spec says "above the threshold" without one).
pub const RISK_P: f64 = 0.7;
/// The request and the arguments, as sent.
const TEXT_CHARS: usize = 1_000;
/// Where a page snapshot starts in a stored user message.
const SNAPSHOT_MARK: &str = "\n\nCurrent page state:";

/// Jev, its permissions point, egress and a provider that is not on-device:
/// the conditions for asking Jev at a permission gate. ACP is included —
/// J14 is the one point ACP gets (spec §3.2).
pub fn permissions_point_on(cfg: &crate::config::AgentConfig) -> bool {
    let jev = &cfg.jev;
    let Some(provider) = cfg.llm.active_provider() else {
        return false;
    };
    jev.is_usable()
        && jev.points.permissions
        && cfg.llm.resolve_wire(provider) != Some(ProviderType::Local)
        && client::egress_allowed(crate::local::latch::is_on(), &jev.endpoint)
}

/// The deterministic policy stage: on with the permissions point, and
/// needing no network, key or egress.
pub fn policy_on(cfg: &crate::config::AgentConfig) -> bool {
    cfg.jev.enabled && cfg.jev.points.permissions
}

/// Jev is asked only about a call the gate would let through without
/// asking, and never about a read-only one: its only power is "ask".
pub fn should_assess(tool: &str, would_auto: bool) -> bool {
    would_auto && classify_tool(tool) != RiskBucket::R
}

/// The one question: could this call do harm the user did not ask for?
pub fn questions(tool: &str) -> BTreeMap<String, Question> {
    let mut q = BTreeMap::new();
    q.insert(
        "risk".to_string(),
        Question::Noul {
            instructions: format!(
                "Could running the tool `{tool}` with these arguments do harm the user did not \
                 ask for — delete or leak data, spend money, contact people, or change the \
                 system — given the user's request?"
            ),
            when_true: "This action could do harm the user did not ask for.".into(),
            when_false: "This action is what the user asked for, or harmless.".into(),
        },
    );
    q
}

/// What Jev sees: the request, the tool, its arguments and the page's
/// domain — nothing of the page itself.
pub fn state(
    query: &str,
    tool: &str,
    args_summary: &str,
    domain: Option<&str>,
) -> serde_json::Value {
    let mut s = serde_json::json!({
        "query": query.chars().take(TEXT_CHARS).collect::<String>(),
        "tool": tool,
        "arguments": args_summary.chars().take(TEXT_CHARS).collect::<String>(),
    });
    if let Some(d) = domain {
        s["domain"] = serde_json::Value::String(d.to_string());
    }
    s
}

/// The turn's request: the last top-level user message, without its page
/// snapshot.
pub fn turn_query(events: &[SessionEvent]) -> String {
    let inner = nested(events);
    events
        .iter()
        .zip(inner)
        .rev()
        .find_map(|(e, nested)| match &e.payload {
            SessionEventPayload::UserMessage { content, .. } if !nested => Some(
                content
                    .split(SNAPSHOT_MARK)
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(TEXT_CHARS)
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// What the model is told when a flagged call cannot be confirmed.
pub fn unattended_message(tool: &str) -> String {
    format!(
        "`{tool}` needs the user's confirmation (flagged as risky), and nobody can confirm in \
         this run. Continue without it, or report what you would have done."
    )
}

/// Jev's risk for a call, or `None` (point off, fallback, no answer).
#[allow(clippy::too_many_arguments)]
pub async fn assess(
    cfg: &crate::config::AgentConfig,
    db: &Arc<nevoflux_storage::Database>,
    session_id: &str,
    stats: Option<Arc<TurnStats>>,
    tool: &str,
    args_summary: &str,
    page_url: Option<&str>,
) -> Option<f64> {
    if !permissions_point_on(cfg) {
        return None;
    }
    let events = nevoflux_storage::repositories::SessionEventRepository::new(db)
        .list(session_id)
        .unwrap_or_default();
    let query = turn_query(&events);
    let writer = Arc::new(SessionEventWriter::new(db.clone(), session_id.to_string()));
    let oracle = JevOracle::new(
        client::shared(&cfg.jev).ok()?,
        cfg.jev.sensitive_domains.clone(),
        Some(writer),
        stats,
    );
    let timeout = Duration::from_millis(cfg.jev.timeout_ms);
    let domain = page_url
        .and_then(|u| reqwest::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_string));
    let ctx = match page_url {
        Some(u) => OracleContext::page("permissions", u, timeout),
        None => OracleContext::no_page("permissions", timeout),
    };
    match oracle
        .ask(
            &ctx,
            state(&query, tool, args_summary, domain.as_deref()),
            questions(tool),
        )
        .await
    {
        Verdict::Answered(r) => r.noul("risk"),
        Verdict::Fallback { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::test_support::answering;
    use nevoflux_protocol::session_event::SessionEvent;
    use serde_json::json;
    use std::time::Duration;

    fn cfg(url: &str) -> crate::config::AgentConfig {
        let mut c = crate::config::AgentConfig::default();
        c.llm.provider = Some("anthropic".into());
        c.jev.enabled = true;
        c.jev.endpoint = url.to_string();
        c.jev.api_key = "k".into();
        c.jev.timeout_ms = 2000;
        c.jev.points.permissions = true;
        c
    }

    #[test]
    fn the_points_are_off_by_default_and_acp_is_covered() {
        let off = crate::config::AgentConfig::default();
        assert!(!permissions_point_on(&off) && !policy_on(&off));
        let mut c = cfg("http://127.0.0.1:1");
        assert!(permissions_point_on(&c) && policy_on(&c));
        c.llm.provider = Some("claude-code".into());
        assert!(permissions_point_on(&c), "ACP gets J14 (spec §3.2)");
        c.llm.provider = Some("local".into());
        assert!(!permissions_point_on(&c), "on-device sends nothing");
        assert!(policy_on(&c), "the deterministic policy needs no network");
    }

    #[test]
    fn only_calls_the_gate_would_let_through_are_assessed_and_never_reads() {
        assert!(should_assess("run_command", true));
        assert!(
            !should_assess("run_command", false),
            "the user is asked anyway"
        );
        assert!(!should_assess("browser_get_markdown", true), "read-only");
    }

    #[test]
    fn the_state_carries_the_request_the_call_and_the_domain_only() {
        let s = state(
            "tidy my downloads",
            "run_command",
            &"x".repeat(5000),
            Some("example.com"),
        );
        assert_eq!(s["query"], "tidy my downloads");
        assert_eq!(s["tool"], "run_command");
        assert_eq!(s["arguments"].as_str().unwrap().chars().count(), 1000);
        assert_eq!(s["domain"], "example.com");
        let q = questions("run_command");
        assert_eq!(q.len(), 1);
        assert!(serde_json::to_string(&q["risk"])
            .unwrap()
            .contains("run_command"));
    }

    #[test]
    fn the_turn_query_is_the_last_request_without_its_page_snapshot() {
        let ev: Vec<SessionEvent> = [
            json!({"type": "turn/start", "turn": 1}),
            json!({"type": "user/message", "content": "first", "origin": "user"}),
            json!({"type": "turn/start", "turn": 2}),
            json!({"type": "user/message", "content": "clean up\n\nCurrent page state:\nSECRET", "origin": "user"}),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, v)| SessionEvent {
            seq: i as i64 + 1,
            ts: 0,
            payload: serde_json::from_value(v).unwrap(),
        })
        .collect();
        assert_eq!(turn_query(&ev), "clean up");
    }

    #[tokio::test]
    async fn assess_reads_the_risk_and_falls_back_to_none() {
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let (url, _) = answering(
            json!({"answers": {"risk": {"noul": 0.9}}, "usage": {"input_tokens": 1, "output_tokens": 1}}),
            Duration::ZERO,
        )
        .await;
        assert_eq!(
            assess(
                &cfg(&url),
                &db,
                "s1",
                None,
                "run_command",
                "rm -rf ~/x",
                None
            )
            .await,
            Some(0.9)
        );
        let (slow, _) = answering(
            json!({"answers": {"risk": {"noul": 0.9}}}),
            Duration::from_millis(3000),
        )
        .await;
        let mut c = cfg(&slow);
        c.jev.timeout_ms = 100;
        assert_eq!(
            assess(&c, &db, "s1", None, "run_command", "rm -rf ~/x", None).await,
            None
        );
    }

    #[test]
    fn the_unattended_message_says_what_to_do() {
        let m = unattended_message("run_command");
        assert!(m.contains("run_command") && m.contains("nobody can confirm"));
    }
}
