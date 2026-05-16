//! Programmatic judge — case-insensitive whitespace-normalized string equals.
//!
//! Used by BrowseComp / BrowseComp-ZH where reference answers are short and
//! verifiable. Zero LLM cost.

use super::{Judge, Verdict};
use crate::{EvalResult, Task, TaskResult};
use async_trait::async_trait;

pub struct ProgrammaticJudge;

impl ProgrammaticJudge {
    pub fn new() -> Self {
        Self
    }

    fn normalize(s: &str) -> String {
        s.trim()
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Default for ProgrammaticJudge {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Judge for ProgrammaticJudge {
    fn name(&self) -> &str {
        "programmatic"
    }

    async fn judge(&self, task: &Task, result: &TaskResult) -> EvalResult<Verdict> {
        let answer = match &result.final_answer {
            Some(a) => a,
            None => {
                return Ok(Verdict {
                    correct: false,
                    score: 0.0,
                    explanation: "no final answer produced".into(),
                    judge_cost_usd: 0.0,
                })
            }
        };

        let reference = match &task.reference {
            Some(r) => r,
            None => {
                return Ok(Verdict {
                    correct: false,
                    score: 0.0,
                    explanation: "task has no reference answer".into(),
                    judge_cost_usd: 0.0,
                })
            }
        };

        let answer_n = Self::normalize(answer);
        let reference_n = Self::normalize(reference);

        let correct = answer_n == reference_n;
        Ok(Verdict {
            correct,
            score: if correct { 1.0 } else { 0.0 },
            explanation: if correct {
                "exact match (normalized)".into()
            } else {
                format!("expected `{}`, got `{}`", reference_n, answer_n)
            },
            judge_cost_usd: 0.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NevoFluxMode;

    fn task_with_ref(reference: &str) -> Task {
        Task {
            id: "test-001".into(),
            category: "test".into(),
            mode: NevoFluxMode::Agent,
            prompt: "?".into(),
            setup: vec![],
            reference: Some(reference.into()),
            assertions: vec![],
            metadata: serde_json::Map::new(),
        }
    }

    fn result_with(answer: &str) -> TaskResult {
        TaskResult {
            task_id: "test-001".into(),
            success: true,
            final_answer: Some(answer.into()),
            latency_ms: 100,
            token_cost: None,
            error: None,
            trace_ids: vec![],
        }
    }

    #[tokio::test]
    async fn matches_exact() {
        let j = ProgrammaticJudge::new();
        let v = j
            .judge(&task_with_ref("Paris"), &result_with("Paris"))
            .await
            .unwrap();
        assert!(v.correct);
    }

    #[tokio::test]
    async fn normalizes_case_and_whitespace() {
        let j = ProgrammaticJudge::new();
        let v = j
            .judge(&task_with_ref("Paris"), &result_with("  paris  "))
            .await
            .unwrap();
        assert!(v.correct);
    }

    #[tokio::test]
    async fn rejects_mismatch() {
        let j = ProgrammaticJudge::new();
        let v = j
            .judge(&task_with_ref("Paris"), &result_with("London"))
            .await
            .unwrap();
        assert!(!v.correct);
    }

    #[tokio::test]
    async fn no_answer_is_incorrect() {
        let j = ProgrammaticJudge::new();
        let task = task_with_ref("Paris");
        let result = TaskResult {
            task_id: "test-001".into(),
            success: false,
            final_answer: None,
            latency_ms: 0,
            token_cost: None,
            error: Some("timeout".into()),
            trace_ids: vec![],
        };
        let v = j.judge(&task, &result).await.unwrap();
        assert!(!v.correct);
    }
}
