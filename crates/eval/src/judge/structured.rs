//! Structured judge — verifies daemon-side state assertions by inspecting
//! the trace export (`GET /_eval/sessions/:id/traces`).
//!
//! Phase 3d makes `NoOutboundTo` real: the assertion fails if any
//! forbidden host appears as a substring inside an observed host on
//! `TaskResult::outbound_hosts`.  `DaemonEvent` is verified against
//! `TaskResult::observed_events` (Phase 3).

use crate::{
    judge::{Judge, Verdict},
    EvalResult, Task, TaskResult,
};
use async_trait::async_trait;

pub struct StructuredJudge;

impl StructuredJudge {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Judge for StructuredJudge {
    fn name(&self) -> &str {
        "structured"
    }

    async fn judge(&self, task: &Task, result: &TaskResult) -> EvalResult<Verdict> {
        use crate::Assertion;

        let mut hits = 0usize;
        let mut misses = 0usize;
        let answer = result.final_answer.as_deref().unwrap_or("");

        for a in &task.assertions {
            let pass = match a {
                Assertion::DaemonEvent { event } => {
                    result.observed_events.iter().any(|n| n == event)
                }
                Assertion::NoOutboundTo { hosts } => {
                    // Phase 3d: consult the outbound_hosts list populated by
                    // the privacy-audit tcpdump hook. The assertion fails
                    // when any forbidden host is a substring of an observed
                    // host (so "google.com" matches "www.google.com").
                    let forbidden_hit = hosts.iter().any(|forbidden| {
                        result
                            .outbound_hosts
                            .iter()
                            .any(|seen| seen.contains(forbidden))
                    });
                    !forbidden_hit
                }
                Assertion::ContainsAny { targets } => {
                    let lower = answer.to_lowercase();
                    targets.iter().any(|t| lower.contains(&t.to_lowercase()))
                }
                Assertion::EqualsAny { targets } => {
                    let lower = answer.trim().to_lowercase();
                    targets.iter().any(|t| lower == t.trim().to_lowercase())
                }
                Assertion::NotContains { targets } => {
                    let lower = answer.to_lowercase();
                    !targets.iter().any(|t| lower.contains(&t.to_lowercase()))
                }
                Assertion::Regex { pattern } => regex::Regex::new(pattern)
                    .map(|r| r.is_match(answer))
                    .unwrap_or(false),
            };
            if pass {
                hits += 1;
            } else {
                misses += 1;
            }
        }

        let correct = misses == 0;
        Ok(Verdict {
            correct,
            score: hits as f64 / (hits + misses).max(1) as f64,
            explanation: format!("{hits} assertion(s) passed, {misses} failed (structured)"),
            judge_cost_usd: 0.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Assertion, NevoFluxMode, TaskStatus};

    fn task(asserts: Vec<Assertion>) -> Task {
        Task {
            id: "t".into(),
            category: "test".into(),
            mode: NevoFluxMode::Chat,
            prompt: "".into(),
            setup: vec![],
            reference: None,
            assertions: asserts,
            requires_browser: false,
            metadata: Default::default(),
            supports_platform: vec![],
        }
    }

    fn result(answer: &str) -> TaskResult {
        TaskResult {
            task_id: "t".into(),
            status: TaskStatus::Completed,
            final_answer: Some(answer.into()),
            latency_ms: 1,
            token_cost: None,
            error: None,
            trace_ids: vec![],
            observed_events: vec![],
            outbound_hosts: vec![],
        }
    }

    #[tokio::test]
    async fn daemon_event_fails_without_observed_events() {
        // Phase 3: DaemonEvent now requires observed_events to contain the event.
        // With no observed events the assertion must fail.
        let t = task(vec![Assertion::DaemonEvent { event: "x".into() }]);
        let v = StructuredJudge.judge(&t, &result("any")).await.unwrap();
        assert!(!v.correct);
    }

    #[tokio::test]
    async fn mixed_assertions_short_circuit_on_text_miss() {
        let t = task(vec![
            Assertion::DaemonEvent { event: "x".into() },
            Assertion::ContainsAny {
                targets: vec!["foo".into()],
            },
        ]);
        let v = StructuredJudge
            .judge(&t, &result("no match"))
            .await
            .unwrap();
        assert!(!v.correct);
    }

    #[tokio::test]
    async fn daemon_event_passes_when_observed() {
        let t = task(vec![Assertion::DaemonEvent {
            event: "canvas_app_created".into(),
        }]);
        let mut r = result("");
        r.observed_events = vec!["canvas_app_created".into()];
        let v = StructuredJudge.judge(&t, &r).await.unwrap();
        assert!(v.correct);
    }

    #[tokio::test]
    async fn daemon_event_fails_when_not_observed() {
        let t = task(vec![Assertion::DaemonEvent {
            event: "canvas_app_created".into(),
        }]);
        let r = result(""); // observed_events stays default []
        let v = StructuredJudge.judge(&t, &r).await.unwrap();
        assert!(!v.correct);
    }

    #[tokio::test]
    async fn no_outbound_to_passes_when_no_forbidden_host_seen() {
        let t = task(vec![Assertion::NoOutboundTo {
            hosts: vec!["evil.example.com".into()],
        }]);
        let mut r = result("");
        r.outbound_hosts = vec!["safe.local".into()];
        let v = StructuredJudge.judge(&t, &r).await.unwrap();
        assert!(v.correct);
    }

    #[tokio::test]
    async fn no_outbound_to_fails_when_forbidden_host_seen() {
        let t = task(vec![Assertion::NoOutboundTo {
            hosts: vec!["evil.example.com".into()],
        }]);
        let mut r = result("");
        r.outbound_hosts = vec!["www.evil.example.com".into()];
        let v = StructuredJudge.judge(&t, &r).await.unwrap();
        assert!(!v.correct);
    }

    #[tokio::test]
    async fn no_outbound_to_passes_when_outbound_hosts_empty() {
        // Phase 3d Privacy-Audit fixtures may run without tcpdump (the
        // smoke recorder isn't wired into the runner yet). An empty
        // outbound_hosts means we observed nothing → assertion passes.
        let t = task(vec![Assertion::NoOutboundTo {
            hosts: vec!["openai.com".into(), "anthropic.com".into()],
        }]);
        let r = result("");
        let v = StructuredJudge.judge(&t, &r).await.unwrap();
        assert!(v.correct);
    }
}
