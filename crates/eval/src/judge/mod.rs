use crate::{EvalResult, Task, TaskResult};
use async_trait::async_trait;

pub mod pricing;
pub mod programmatic;
pub mod structured;
pub mod webjudge;
pub use webjudge::WebJudge;

// Stubs for subsequent PRs:
// pub mod privacy;      // outbound traffic auditor

/// Verdict produced by a judge for a single task execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Verdict {
    pub correct: bool,
    pub score: f64, // 0.0 – 1.0; for binary judges this is 0.0 or 1.0.
    pub explanation: String,
    /// Optional cost incurred by the judge itself (e.g. LLM-as-judge calls).
    pub judge_cost_usd: f64,
}

#[async_trait]
pub trait Judge: Send + Sync {
    fn name(&self) -> &str;

    /// Decide whether `result` correctly answers `task`.
    async fn judge(&self, task: &Task, result: &TaskResult) -> EvalResult<Verdict>;
}

pub fn registry() -> Vec<Box<dyn Judge>> {
    vec![
        Box::new(programmatic::ProgrammaticJudge::new()),
        Box::new(structured::StructuredJudge::new()),
        Box::new(webjudge::WebJudge::new()),
    ]
}

pub fn find(name: &str) -> Option<Box<dyn Judge>> {
    registry().into_iter().find(|j| j.name() == name)
}
