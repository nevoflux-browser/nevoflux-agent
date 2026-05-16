use crate::{EvalResult, Task};
use async_trait::async_trait;

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
