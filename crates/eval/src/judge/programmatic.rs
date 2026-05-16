//! Programmatic judge — equals / contains / regex matching against the
//! task's `reference` answer. Cheap, no LLM, deterministic.

use crate::{
    judge::{Judge, Verdict},
    EvalResult, Task, TaskResult,
};
use async_trait::async_trait;
use regex::Regex;

pub struct ProgrammaticJudge;

impl ProgrammaticJudge {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Judge for ProgrammaticJudge {
    fn name(&self) -> &str {
        "programmatic"
    }

    async fn judge(&self, task: &Task, result: &TaskResult) -> EvalResult<Verdict> {
        let answer = result.final_answer.as_deref().unwrap_or("");

        // Try assertions first (more structured).
        let mut hits = 0;
        let mut misses = 0;
        for a in &task.assertions {
            let pass = check_assertion(a, answer, result);
            if pass {
                hits += 1;
            } else {
                misses += 1;
            }
        }

        // Fall back to reference if no assertions.
        if task.assertions.is_empty() {
            if let Some(reference) = &task.reference {
                let ans = answer.trim().to_lowercase();
                let r = reference.trim().to_lowercase();
                let correct = ans == r;
                return Ok(Verdict {
                    correct,
                    score: if correct { 1.0 } else { 0.0 },
                    explanation: if correct {
                        "exact match against reference".into()
                    } else {
                        format!("got `{ans}`, expected `{r}`")
                    },
                    judge_cost_usd: 0.0,
                });
            }
            return Ok(Verdict {
                correct: false,
                score: 0.0,
                explanation: "no assertions and no reference".into(),
                judge_cost_usd: 0.0,
            });
        }

        let correct = misses == 0;
        Ok(Verdict {
            correct,
            score: hits as f64 / (hits + misses).max(1) as f64,
            explanation: format!("{hits} assertion(s) passed, {misses} failed"),
            judge_cost_usd: 0.0,
        })
    }
}

fn check_assertion(a: &crate::Assertion, answer: &str, _result: &TaskResult) -> bool {
    use crate::Assertion;
    match a {
        Assertion::EqualsAny { targets } => {
            let lower = answer.trim().to_lowercase();
            targets.iter().any(|t| lower == t.trim().to_lowercase())
        }
        Assertion::ContainsAny { targets } => {
            let lower = answer.to_lowercase();
            targets.iter().any(|t| lower.contains(&t.to_lowercase()))
        }
        Assertion::NotContains { targets } => {
            let lower = answer.to_lowercase();
            !targets.iter().any(|t| lower.contains(&t.to_lowercase()))
        }
        Assertion::Regex { pattern } => Regex::new(pattern)
            .map(|r| r.is_match(answer))
            .unwrap_or(false),
        // Structured assertions (DaemonEvent / NoOutboundTo) are handled
        // by StructuredJudge — programmatic skips them silently.
        Assertion::DaemonEvent { .. } | Assertion::NoOutboundTo { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Assertion, NevoFluxMode, TaskStatus};

    fn task(asserts: Vec<Assertion>, reference: Option<&str>) -> Task {
        Task {
            id: "t".into(),
            category: "test".into(),
            mode: NevoFluxMode::Chat,
            prompt: "".into(),
            setup: vec![],
            reference: reference.map(String::from),
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
    async fn reference_exact_match() {
        let t = task(vec![], Some("42"));
        let v = ProgrammaticJudge.judge(&t, &result("42")).await.unwrap();
        assert!(v.correct);
    }

    #[tokio::test]
    async fn contains_any_passes() {
        let t = task(
            vec![Assertion::ContainsAny {
                targets: vec!["foo".into()],
            }],
            None,
        );
        let v = ProgrammaticJudge
            .judge(&t, &result("contains FOO inside"))
            .await
            .unwrap();
        assert!(v.correct);
    }

    #[tokio::test]
    async fn not_contains_fails_on_hit() {
        let t = task(
            vec![Assertion::NotContains {
                targets: vec!["secret".into()],
            }],
            None,
        );
        let v = ProgrammaticJudge
            .judge(&t, &result("contains SECRET"))
            .await
            .unwrap();
        assert!(!v.correct);
    }

    #[tokio::test]
    async fn regex_match() {
        let t = task(
            vec![Assertion::Regex {
                pattern: r"\d+".into(),
            }],
            None,
        );
        let v = ProgrammaticJudge
            .judge(&t, &result("answer is 42"))
            .await
            .unwrap();
        assert!(v.correct);
    }
}
