use crate::{
    browser::BrowserLaunchMode,
    termination::{AnswerExtractor, TerminationStrategy},
    EvalResult, Task,
};
use async_trait::async_trait;
use nevoflux_protocol::subagent::ToolsConfig;

pub mod browsecomp;
pub mod nevoflux_suite;

// Stubs to be implemented in subsequent PRs:
// pub mod browsecomp_zh;
// pub mod online_mind2web;
// pub mod webarena;
// pub mod webvoyager;

/// A benchmark loads tasks from a source (filesystem / submodule / remote).
///
/// Implementors must be cheap to construct; heavy I/O happens in [`load_tasks`].
#[async_trait]
pub trait Benchmark: Send + Sync {
    /// Stable identifier (matches CLI flag, e.g. "browsecomp", "nevoflux-suite").
    fn name(&self) -> &str;

    /// Human-readable description for reports.
    fn description(&self) -> &str;

    /// Whether this benchmark needs network access at run time.
    fn requires_network(&self) -> bool;

    /// Load all tasks. Use `filter` to support `--task-glob` CLI option.
    async fn load_tasks(&self, filter: Option<&str>) -> EvalResult<Vec<Task>>;

    /// Default judge name for this benchmark (e.g. "programmatic", "webjudge").
    /// Runner uses this if user does not override with `--judge`.
    fn default_judge(&self) -> &str;

    /// When to stop watching the SSE stream. See spec §6.2.3.
    /// Default `NaturalStop` covers most benchmarks.
    fn termination_strategy(&self) -> TerminationStrategy {
        TerminationStrategy::NaturalStop
    }

    /// How to extract `final_answer` from the event stream.
    /// Default `LastAssistantMessage` concatenates all Token events.
    fn answer_extractor(&self) -> AnswerExtractor {
        AnswerExtractor::LastAssistantMessage
    }

    /// Append this string to each task's prompt before submission. The
    /// report header will note the modification (see reporter.rs).
    /// BrowseComp uses this to inject `<ANSWER>` tag instructions.
    fn prompt_suffix(&self) -> Option<&str> {
        None
    }

    /// Per-benchmark max concurrent tasks. Runner uses
    /// `min(config.parallelism, benchmark.max_parallelism(mode))`.
    fn max_parallelism(&self, mode: &BrowserLaunchMode) -> usize {
        match mode {
            BrowserLaunchMode::DaemonOnly { .. } => 8,
            _ => 1, // real-browser benchmarks are conservative by default
        }
    }

    /// Tools to expose to the agent for tasks in this benchmark.
    /// Default `None` — most benchmarks don't need tools. Browser-based
    /// benchmarks (Online-Mind2Web, etc.) override to enable navigation,
    /// click, type, etc.
    fn tools_config(&self) -> ToolsConfig {
        ToolsConfig::None
    }
}

/// Registry — maps benchmark name → instance. Add new benchmarks here.
pub fn registry() -> Vec<Box<dyn Benchmark>> {
    vec![
        Box::new(browsecomp::BrowseComp::new()),
        Box::new(nevoflux_suite::NevoFluxSuite::new()),
        // Box::new(browsecomp_zh::BrowseCompZh::new()),
        // Box::new(online_mind2web::OnlineMind2Web::new()),
    ]
}

pub fn find(name: &str) -> Option<Box<dyn Benchmark>> {
    registry().into_iter().find(|b| b.name() == name)
}
