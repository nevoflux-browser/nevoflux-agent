//! BrowseComp benchmark adapter (Phase 3c minimal slice).
//!
//! For Phase 3c this loads a hand-written 5-task fixture from
//! `eval/benchmarks/browsecomp-fixture.json`. Phase 3d/4 will swap the
//! source for the decrypted openai/simple-evals browse_comp_test_set.csv
//! (XOR-encrypted to prevent training contamination — see
//! `eval/README-DATASETS.md`).

use crate::{benchmarks::Benchmark, EvalResult, Task};
use async_trait::async_trait;
use nevoflux_protocol::subagent::ToolsConfig;
use std::path::PathBuf;

pub struct BrowseComp {
    fixture_path: PathBuf,
}

impl BrowseComp {
    pub fn new() -> Self {
        Self {
            fixture_path: PathBuf::from("eval/benchmarks/browsecomp-fixture.json"),
        }
    }

    pub fn with_fixture(path: PathBuf) -> Self {
        Self { fixture_path: path }
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
        "BrowseComp — hard short-answer web research (Phase 3c: 5-task fixture; Phase 3d/4: full decrypted upstream)"
    }

    fn requires_network(&self) -> bool {
        true
    }

    fn default_judge(&self) -> &str {
        "programmatic"
    }

    fn tools_config(&self) -> ToolsConfig {
        // BrowseComp is designed for browsing; default exposes browser_*
        // and web_search tools.  But in daemon-only browser mode those
        // tools cannot succeed, and the agent typically hangs waiting on
        // tool responses → 100% task timeout.  Setting
        // NEVOFLUX_BC_NO_TOOLS=1 forces pure-knowledge mode (no tool
        // surface), matching the openai/simple-evals 'no-browse' baseline.
        if std::env::var("NEVOFLUX_BC_NO_TOOLS").is_ok() {
            return ToolsConfig::None;
        }
        ToolsConfig::Allow(vec!["browser_*".to_string(), "web_search".to_string()])
    }

    async fn load_tasks(&self, filter: Option<&str>) -> EvalResult<Vec<Task>> {
        // Phase 3d: NEVOFLUX_BC_DATA_PATH override switches to the real
        // upstream XOR-encrypted CSV (see eval/README-DATASETS.md and
        // `just eval-fetch-bc`).  Fixture path used when unset.
        if let Some(real_path) = std::env::var_os("NEVOFLUX_BC_DATA_PATH") {
            let p = std::path::PathBuf::from(real_path);
            let mut tasks = crate::datasets::browsecomp_csv::load(&p)?;
            if let Some(f) = filter {
                tasks.retain(|t| t.id.contains(f));
            }
            return Ok(tasks);
        }
        crate::benchmarks::fixture::load_qa_fixture(
            &self.fixture_path,
            "browsecomp",
            "Reply with just the short answer (1-5 words).",
            filter,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NevoFluxMode;

    #[tokio::test]
    async fn loads_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fixture.json");
        let raw = r#"
{
  "version": "test-1",
  "source": "test",
  "tasks": [
    { "id": "bc-test-001", "question": "What is 1+1?", "answer": "2" }
  ]
}
"#;
        tokio::fs::write(&path, raw).await.unwrap();
        let bench = BrowseComp::with_fixture(path);
        let tasks = bench.load_tasks(None).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "bc-test-001");
        assert_eq!(tasks[0].reference, Some("2".into()));
        assert!(tasks[0].prompt.contains("What is 1+1?"));
        assert_eq!(tasks[0].mode, NevoFluxMode::Agent);
    }

    #[tokio::test]
    async fn filter_narrows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fixture.json");
        let raw = r#"
{
  "version": "t",
  "source": "t",
  "tasks": [
    { "id": "alpha", "question": "?", "answer": "A" },
    { "id": "beta", "question": "?", "answer": "B" }
  ]
}
"#;
        tokio::fs::write(&path, raw).await.unwrap();
        let bench = BrowseComp::with_fixture(path);
        let tasks = bench.load_tasks(Some("alph")).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "alpha");
    }

    #[tokio::test]
    async fn shipped_fixture_parses_with_default_path() {
        // Walks up from CARGO_MANIFEST_DIR (crates/eval) to the workspace
        // root, then loads the checked-in fixture.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace = std::path::Path::new(manifest_dir)
            .ancestors()
            .nth(2)
            .unwrap();
        let path = workspace.join("eval/benchmarks/browsecomp-fixture.json");
        let bench = BrowseComp::with_fixture(path);
        let tasks = bench.load_tasks(None).await.unwrap();
        assert_eq!(tasks.len(), 5, "fixture should ship 5 tasks");
        assert!(
            tasks[0].prompt.contains("Nobel Prize"),
            "Phase 4 fixture should contain multi-hop Nobel Prize question, got: {}",
            tasks[0].prompt
        );
    }
}
