//! The decision oracle (spec §5.1 `DecisionOracle`, v1 `JevOracle`). Every
//! failure becomes a fallback the caller handles with its local rule; it is
//! logged as `jev/fallback`, never raised.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nevoflux_protocol::session_event::SessionEventPayload;

use super::client::{JevClient, JevError};
use super::privacy::{scope_for, Scope};
use super::wire::{JevResponse, Question};
use crate::session_events::SessionEventWriter;
use crate::turn_stats::TurnStats;

/// What the state describes, so the oracle can apply §5.8. There is no
/// public field to forget: every call site has to pick one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageScope {
    /// Content of the page at this URL.
    Page(String),
    /// Page content whose URL is not known (MCP tools, a lost tab): metadata only.
    UnknownPage,
    /// No page content at all (local files, shell output, the user's own text).
    NoPage,
}

pub struct OracleContext {
    /// The decision point asking (`tools`, `visibility`, …), for the log.
    point: &'static str,
    page: PageScope,
    timeout: Duration,
}

impl OracleContext {
    /// The state carries content of the page at `url`.
    pub fn page(point: &'static str, url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            point,
            page: PageScope::Page(url.into()),
            timeout,
        }
    }

    /// The state carries page content from an unknown URL: metadata only.
    pub fn unknown_page(point: &'static str, timeout: Duration) -> Self {
        Self {
            point,
            page: PageScope::UnknownPage,
            timeout,
        }
    }

    /// The state carries no page content.
    pub fn no_page(point: &'static str, timeout: Duration) -> Self {
        Self {
            point,
            page: PageScope::NoPage,
            timeout,
        }
    }

    pub fn point(&self) -> &'static str {
        self.point
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Full only for no page or a known, non-sensitive page; an empty URL
    /// counts as unknown.
    pub fn scope(&self, sensitive_domains: &[String]) -> Scope {
        match &self.page {
            PageScope::NoPage => Scope::Full,
            PageScope::UnknownPage => Scope::MetadataOnly,
            PageScope::Page(u) if u.trim().is_empty() => Scope::MetadataOnly,
            PageScope::Page(u) => scope_for(u, sensitive_domains),
        }
    }
}

pub enum Verdict {
    Answered(JevResponse),
    /// The caller applies its local rule.
    Fallback {
        reason: String,
    },
}

#[async_trait]
pub trait DecisionOracle: Send + Sync {
    async fn ask(
        &self,
        ctx: &OracleContext,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Verdict;
}

const METADATA_KEYS: &[&str] = &["domain", "title", "element_count", "query", "step"];

/// The state with only metadata keys (sensitive sites, spec §5.8).
pub fn metadata_only(state: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let Some(obj) = state.as_object() {
        for k in METADATA_KEYS {
            if let Some(v) = obj.get(*k) {
                out.insert((*k).to_string(), v.clone());
            }
        }
    }
    serde_json::Value::Object(out)
}

pub struct JevOracle {
    client: JevClient,
    sensitive_domains: Vec<String>,
    events: Option<Arc<SessionEventWriter>>,
    stats: Option<Arc<TurnStats>>,
}

impl JevOracle {
    pub fn new(
        client: JevClient,
        sensitive_domains: Vec<String>,
        events: Option<Arc<SessionEventWriter>>,
        stats: Option<Arc<TurnStats>>,
    ) -> Self {
        Self {
            client,
            sensitive_domains,
            events,
            stats,
        }
    }

    fn fall_back(&self, point: &str, reason: String, started: Instant) -> Verdict {
        if let Some(w) = &self.events {
            w.append(SessionEventPayload::JevFallback {
                point: point.to_string(),
                reason: reason.clone(),
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
        Verdict::Fallback { reason }
    }
}

#[async_trait]
impl DecisionOracle for JevOracle {
    async fn ask(
        &self,
        ctx: &OracleContext,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Verdict {
        let started = Instant::now();
        let scoped = match ctx.scope(&self.sensitive_domains) {
            Scope::MetadataOnly => metadata_only(&state),
            Scope::Full => state,
        };
        match self.client.ask(scoped, questions, ctx.timeout()).await {
            Ok(r) => {
                if let Some(s) = &self.stats {
                    s.record_jev(r.usage.input_tokens, r.usage.output_tokens);
                }
                Verdict::Answered(r)
            }
            Err(e) => {
                let reason = match e {
                    JevError::Timeout => "timeout".to_string(),
                    JevError::Refused(_) => "refused".to_string(),
                    JevError::NotConfigured => "not_configured".to_string(),
                    JevError::Http { status } => format!("http_{status}"),
                    JevError::Transport(_) => "transport".to_string(),
                    JevError::Decode(_) => "decode".to_string(),
                };
                self.fall_back(ctx.point(), reason, started)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::test_support::{answering, one_noul};

    fn ctx(point: &'static str, tab: Option<&str>, ms: u64) -> OracleContext {
        let t = Duration::from_millis(ms);
        match tab {
            Some(u) => OracleContext::page(point, u, t),
            None => OracleContext::no_page(point, t),
        }
    }

    #[test]
    fn scope_is_metadata_only_unless_the_page_is_known_and_ordinary() {
        let t = Duration::from_millis(800);
        assert_eq!(OracleContext::no_page("x", t).scope(&[]), Scope::Full);
        assert_eq!(
            OracleContext::page("x", "https://en.wikipedia.org/", t).scope(&[]),
            Scope::Full
        );
        assert_eq!(
            OracleContext::page("x", "https://www.paypal.com/", t).scope(&[]),
            Scope::MetadataOnly
        );
        assert_eq!(
            OracleContext::page("x", "", t).scope(&[]),
            Scope::MetadataOnly
        );
        assert_eq!(
            OracleContext::unknown_page("x", t).scope(&[]),
            Scope::MetadataOnly
        );
    }

    #[tokio::test]
    async fn an_unknown_page_sends_metadata_only() {
        let (url, bodies) = answering(
            serde_json::json!({"answers": {}, "usage": {}}),
            Duration::ZERO,
        )
        .await;
        let oracle = JevOracle::new(JevClient::new(&url, "k", "jev-latest"), vec![], None, None);
        let state = serde_json::json!({"domain": "d", "page_text": "SECRET"});
        oracle
            .ask(
                &OracleContext::unknown_page("visibility", Duration::from_secs(2)),
                state,
                one_noul(),
            )
            .await;
        let sent = bodies.lock().unwrap().join(
            "
",
        );
        assert!(!sent.contains("SECRET"), "{sent}");
    }

    fn writer() -> (Arc<SessionEventWriter>, Arc<nevoflux_storage::Database>) {
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        (
            Arc::new(SessionEventWriter::new(db.clone(), "s1".into())),
            db,
        )
    }

    #[tokio::test]
    async fn an_answer_is_returned_and_billed() {
        let (url, _) = answering(
            serde_json::json!({"answers": {"x": {"noul": 0.7}}, "usage": {"input_tokens": 30, "output_tokens": 2}}),
            Duration::ZERO,
        )
        .await;
        let stats = TurnStats::new();
        let oracle = JevOracle::new(
            JevClient::new(&url, "k", "jev-latest"),
            vec![],
            None,
            Some(stats.clone()),
        );
        let v = oracle
            .ask(
                &ctx("rebuild", None, 2000),
                serde_json::json!({}),
                one_noul(),
            )
            .await;
        assert!(matches!(v, Verdict::Answered(ref r) if r.noul("x") == Some(0.7)));
        stats.record(crate::turn_stats::CallStats {
            reported_input: Some(1),
            ..Default::default()
        });
        let j = stats.snapshot().unwrap().jev.unwrap();
        assert_eq!((j.input, j.output, j.calls), (30, 2, 1));
    }

    #[tokio::test]
    async fn a_timeout_falls_back_and_is_logged() {
        let (url, _) = answering(
            serde_json::json!({"answers": {}}),
            Duration::from_millis(400),
        )
        .await;
        let (w, db) = writer();
        let oracle = JevOracle::new(
            JevClient::new(&url, "k", "jev-latest"),
            vec![],
            Some(w),
            None,
        );
        let v = oracle
            .ask(
                &ctx("visibility", None, 50),
                serde_json::json!({}),
                one_noul(),
            )
            .await;
        assert!(matches!(v, Verdict::Fallback { ref reason } if reason == "timeout"));
        let events = nevoflux_storage::repositories::SessionEventRepository::new(&db)
            .list("s1")
            .unwrap();
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            SessionEventPayload::JevFallback { point, reason, .. } if point == "visibility" && reason == "timeout"
        )));
    }

    #[tokio::test]
    async fn a_sensitive_tab_sends_metadata_only() {
        let (url, bodies) = answering(
            serde_json::json!({"answers": {}, "usage": {}}),
            Duration::ZERO,
        )
        .await;
        let oracle = JevOracle::new(JevClient::new(&url, "k", "jev-latest"), vec![], None, None);
        let state = serde_json::json!({"domain": "paypal.com", "page_text": "SECRET balance 1234"});
        oracle
            .ask(
                &ctx("visibility", Some("https://www.paypal.com/x"), 2000),
                state,
                one_noul(),
            )
            .await;
        let sent = bodies.lock().unwrap().join("\n");
        assert!(!sent.contains("SECRET"), "{sent}");
        assert!(sent.contains("paypal.com"));
    }

    #[test]
    fn metadata_only_keeps_only_the_allowed_keys() {
        let s = serde_json::json!({"domain":"d","title":"t","element_count":3,"query":"q","step":2,"page_text":"x","new_result":{"text":"y"}});
        assert_eq!(
            metadata_only(&s),
            serde_json::json!({"domain":"d","title":"t","element_count":3,"query":"q","step":2})
        );
    }
}
