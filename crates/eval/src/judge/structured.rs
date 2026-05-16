//! Structured judge — verifies daemon-side state assertions by inspecting
//! the trace export (`GET /_eval/sessions/:id/traces`).
//!
//! For Phase 2 this judge checks text assertions fully. `DaemonEvent` and
//! `NoOutboundTo` are best-effort (pass without verification) until Phase 3
//! threads observed daemon events / tcpdump results into TaskResult.

use crate::{judge::{Judge, Verdict}, EvalResult, Task, TaskResult};
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
                Assertion::DaemonEvent { event: _ } => {
                    // Phase 2: best-effort pass. Real verification lands in
                    // Phase 3 once Runner attaches observed daemon events to
                    // TaskResult.
                    true
                }
                Assertion::NoOutboundTo { .. } => {
                    // Phase 3 (tcpdump).
                    true
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
                Assertion::Regex { pattern } => {
                    regex::Regex::new(pattern)
                        .map(|r| r.is_match(answer))
                        .unwrap_or(false)
                }
            };
            if pass { hits += 1; } else { misses += 1; }
        }

        let correct = misses == 0;
        Ok(Verdict {
            correct,
            score: hits as f64 / (hits + misses).max(1) as f64,
            explanation: format!(
                "{hits} assertion(s) passed, {misses} failed (structured)"
            ),
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
        }
    }

    #[tokio::test]
    async fn daemon_event_phase2_best_effort_passes() {
        let t = task(vec![Assertion::DaemonEvent { event: "x".into() }]);
        let v = StructuredJudge.judge(&t, &result("any")).await.unwrap();
        assert!(v.correct);
    }

    #[tokio::test]
    async fn mixed_assertions_short_circuit_on_text_miss() {
        let t = task(vec![
            Assertion::DaemonEvent { event: "x".into() },
            Assertion::ContainsAny { targets: vec!["foo".into()] },
        ]);
        let v = StructuredJudge.judge(&t, &result("no match")).await.unwrap();
        assert!(!v.correct);
    }
}
