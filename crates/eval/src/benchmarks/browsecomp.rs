//! BrowseComp adapter (OpenAI, 1,266 tasks).
//!
//! Source: git submodule at `eval/benchmarks/browsecomp/` pointing to
//! github.com/openai/simple-evals. Tasks are JSONL with short verifiable answers.
//!
//! Default judge: `programmatic` (case-insensitive string equals).

use super::Benchmark;
use crate::{Assertion, EvalError, EvalResult, NevoFluxMode, Task};
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncBufReadExt;
use tracing::{debug, warn};

const DEFAULT_DATA_PATH: &str = "eval/benchmarks/browsecomp/data/browsecomp.jsonl";

pub struct BrowseComp {
    data_path: PathBuf,
}

impl BrowseComp {
    pub fn new() -> Self {
        Self {
            data_path: PathBuf::from(
                std::env::var("BROWSECOMP_DATA").unwrap_or_else(|_| DEFAULT_DATA_PATH.to_string()),
            ),
        }
    }
}

impl Default for BrowseComp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Benchmark for BrowseComp {
    fn name(&self) -> &str {
        "browsecomp"
    }

    fn description(&self) -> &str {
        "OpenAI BrowseComp — 1,266 hard information-retrieval tasks (English web)"
    }

    fn requires_network(&self) -> bool {
        true
    }

    fn default_judge(&self) -> &str {
        "programmatic"
    }

    async fn load_tasks(&self, filter: Option<&str>) -> EvalResult<Vec<Task>> {
        if !self.data_path.exists() {
            return Err(EvalError::BenchmarkNotFound {
                name: format!(
                    "browsecomp data missing at {:?}; did you `git submodule update --init`?",
                    self.data_path
                ),
            });
        }

        let file = fs::File::open(&self.data_path).await?;
        let reader = tokio::io::BufReader::new(file);
        let mut lines = reader.lines();

        let mut tasks = Vec::new();
        let mut idx = 0usize;
        while let Some(line) = lines.next_line().await? {
            idx += 1;
            if line.trim().is_empty() {
                continue;
            }
            let raw: BrowseCompRaw = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    warn!(line = idx, error = %e, "skipping malformed BrowseComp row");
                    continue;
                }
            };

            let task_id = format!("browsecomp-{:04}", idx);

            // Apply filter glob (simple substring for now; upgrade to glob crate if needed).
            if let Some(f) = filter {
                if !task_id.contains(f) && !raw.problem.contains(f) {
                    continue;
                }
            }

            tasks.push(Task {
                id: task_id,
                category: "browsecomp".into(),
                mode: NevoFluxMode::Agent,
                prompt: raw.problem,
                setup: vec![],
                reference: Some(raw.answer.clone()),
                requires_browser: false,
                assertions: vec![Assertion::EqualsAny {
                    targets: vec![raw.answer],
                }],
                metadata: serde_json::Map::new(),
            });
        }

        debug!(loaded = tasks.len(), "BrowseComp tasks loaded");
        Ok(tasks)
    }
}

#[derive(Debug, serde::Deserialize)]
struct BrowseCompRaw {
    problem: String,
    answer: String,
    // BrowseComp also includes `topic` and `source_url` — keep simple for now.
}
