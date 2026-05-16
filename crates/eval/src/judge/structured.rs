//! Structured judge — evaluates a task's `assertions` list against a [`TaskResult`].
//!
//! Used by NevoFlux self-suite (YAML-defined tasks). Composable: a task can mix
//! ContainsAny / NotContains / Regex / DaemonEvent / NoOutboundTo assertions and
//! the judge ANDs them together.

use super::{Judge, Verdict};
use crate::{Assertion, EvalResult, Task, TaskResult};
use async_trait::async_trait;
use regex::Regex;

pub struct StructuredJudge;

impl StructuredJudge {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StructuredJudge {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Judge for StructuredJudge {
    fn name(&self) -> &str {
        "structured"
    }

    async fn judge(&self, task: &Task, result: &TaskResult) -> EvalResult<Verdict> {
        if task.assertions.is_empty() {
            return Ok(Verdict {
                correct: false,
                score: 0.0,
                explanation: "task defines no assertions".into(),
                judge_cost_usd: 0.0,
            });
        }

        let answer = result.final_answer.as_deref().unwrap_or("");
        let answer_lower = answer.to_lowercase();

        let mut failures = Vec::new();

        for (idx, assertion) in task.assertions.iter().enumerate() {
            let ok = match assertion {
                Assertion::EqualsAny { targets } => targets
                    .iter()
                    .any(|t| t.trim().to_lowercase() == answer.trim().to_lowercase()),

                Assertion::ContainsAny { targets } => {
                    targets.iter().any(|t| answer_lower.contains(&t.to_lowercase()))
                }

                Assertion::NotContains { targets } => !targets
                    .iter()
                    .any(|t| answer_lower.contains(&t.to_lowercase())),

                Assertion::Regex { pattern } => {
                    let re = Regex::new(pattern)?;
                    re.is_match(answer)
                }

                // These two require daemon-side trace inspection. The runner
                // populates `result.trace_ids`; a full implementation queries
                // the traces SQLite table. For scaffold, we conservatively
                // mark them as unimplemented so users are aware.
                Assertion::DaemonEvent { event: _ } => {
                    failures.push(format!(
                        "assertion #{}: DaemonEvent assertions require traces inspection (TODO)",
                        idx
                    ));
                    continue;
                }
                Assertion::NoOutboundTo { hosts: _ } => {
                    failures.push(format!(
                        "assertion #{}: NoOutboundTo assertions require network audit hook (TODO)",
                        idx
                    ));
                    continue;
                }
            };

            if !ok {
                failures.push(format!("assertion #{} failed: {:?}", idx, assertion));
            }
        }

        let total = task.assertions.len();
        let passed = total - failures.len();
        let score = passed as f64 / total as f64;
        let correct = failures.is_empty();

        Ok(Verdict {
            correct,
            score,
            explanation: if correct {
                format!("all {} assertions passed", total)
            } else {
                failures.join("; ")
            },
            judge_cost_usd: 0.0,
        })
    }
}
