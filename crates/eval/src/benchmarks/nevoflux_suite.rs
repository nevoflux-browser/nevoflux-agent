//! NevoFlux self-suite — YAML-driven tasks for capabilities not covered by
//! external benchmarks (Canvas SDK, memory recall, MCP bidir, mode authz, privacy).
//!
//! Tasks live in `eval/nevoflux-suite/<category>/*.yaml`. Adding a task = adding
//! a YAML file + opening a PR. No Rust changes needed.

use super::Benchmark;
use crate::{EvalError, EvalResult, Task};
use async_trait::async_trait;
use glob::glob;
use std::path::PathBuf;
use tracing::{debug, warn};

const DEFAULT_ROOT: &str = "eval/nevoflux-suite";

pub struct NevoFluxSuite {
    root: PathBuf,
}

impl NevoFluxSuite {
    pub fn new() -> Self {
        Self {
            root: PathBuf::from(
                std::env::var("NEVOFLUX_SUITE_ROOT").unwrap_or_else(|_| DEFAULT_ROOT.to_string()),
            ),
        }
    }
}

impl Default for NevoFluxSuite {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Benchmark for NevoFluxSuite {
    fn name(&self) -> &str {
        "nevoflux-suite"
    }

    fn description(&self) -> &str {
        "NevoFlux self-suite — Canvas SDK, memory recall, MCP bidir, mode authz, privacy"
    }

    fn requires_network(&self) -> bool {
        // Most subcategories run offline; privacy-audit tests outbound traffic
        // monitoring and may need a local mock server.
        false
    }

    fn default_judge(&self) -> &str {
        "structured" // Uses Assertion-based judging.
    }

    async fn load_tasks(&self, filter: Option<&str>) -> EvalResult<Vec<Task>> {
        let pattern = self.root.join("**/*.yaml");
        let pattern_str = pattern.to_string_lossy().to_string();

        let mut tasks = Vec::new();
        for entry in glob(&pattern_str).map_err(|e| EvalError::Other(e.to_string()))? {
            let path = match entry {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "glob entry error; skipping");
                    continue;
                }
            };

            let content = tokio::fs::read_to_string(&path).await?;
            let task: Task = match serde_yaml::from_str(&content) {
                Ok(t) => t,
                Err(e) => {
                    return Err(EvalError::TaskParse {
                        path: path.to_string_lossy().to_string(),
                        reason: e.to_string(),
                    })
                }
            };

            if let Some(f) = filter {
                if !task.id.contains(f) && !task.category.contains(f) {
                    continue;
                }
            }

            tasks.push(task);
        }

        debug!(loaded = tasks.len(), root = ?self.root, "NevoFlux suite tasks loaded");
        Ok(tasks)
    }
}
