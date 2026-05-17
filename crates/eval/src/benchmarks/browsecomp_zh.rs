//! BrowseComp-ZH benchmark adapter (Phase 3c minimal slice).
//!
//! For Phase 3c this loads a hand-written 5-task Chinese-language fixture
//! from `eval/benchmarks/browsecomp-zh-fixture.json`. Phase 3d/4 will swap
//! the source for the HuggingFace `Phantom-AI/BrowseComp-ZH` parquet
//! (see `eval/README-DATASETS.md`).

use crate::{benchmarks::Benchmark, EvalResult, Task};
use async_trait::async_trait;
use nevoflux_protocol::subagent::ToolsConfig;
use std::path::PathBuf;

pub struct BrowseCompZh {
    fixture_path: PathBuf,
}

impl BrowseCompZh {
    pub fn new() -> Self {
        Self {
            fixture_path: PathBuf::from("eval/benchmarks/browsecomp-zh-fixture.json"),
        }
    }

    pub fn with_fixture(path: PathBuf) -> Self {
        Self { fixture_path: path }
    }
}

impl Default for BrowseCompZh {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Benchmark for BrowseCompZh {
    fn name(&self) -> &str {
        "browsecomp-zh"
    }

    fn description(&self) -> &str {
        "BrowseComp-ZH — Chinese-web hard short-answer (Phase 3c: 5-task fixture; Phase 3d/4: HuggingFace parquet)"
    }

    fn requires_network(&self) -> bool {
        true
    }

    fn default_judge(&self) -> &str {
        "programmatic"
    }

    fn tools_config(&self) -> ToolsConfig {
        ToolsConfig::Allow(vec!["browser_*".to_string(), "web_search".to_string()])
    }

    async fn load_tasks(&self, filter: Option<&str>) -> EvalResult<Vec<Task>> {
        // Phase 3d: NEVOFLUX_BC_ZH_DATA_PATH override routes to a JSONL
        // pre-converted from the HuggingFace parquet (the Rust parquet
        // reader is deferred to Phase 4).  Filter glob is still honoured.
        if let Some(real_path) = std::env::var_os("NEVOFLUX_BC_ZH_DATA_PATH") {
            let p = std::path::PathBuf::from(real_path);
            let mut tasks = crate::datasets::jsonl::load(
                &p,
                "browsecomp-zh",
                "请用中文简短作答（不超过 10 个字）。",
            )?;
            if let Some(f) = filter {
                tasks.retain(|t| t.id.contains(f));
            }
            return Ok(tasks);
        }
        crate::benchmarks::fixture::load_qa_fixture(
            &self.fixture_path,
            "browsecomp-zh",
            "请用中文简短作答（不超过 10 个字）。",
            filter,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_chinese_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fixture.json");
        let raw = r#"
{
  "version": "test-1",
  "source": "test",
  "tasks": [
    { "id": "bc-zh-test-001", "question": "1+1=?", "answer": "2" }
  ]
}
"#;
        tokio::fs::write(&path, raw).await.unwrap();
        let bench = BrowseCompZh::with_fixture(path);
        let tasks = bench.load_tasks(None).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "bc-zh-test-001");
        assert!(tasks[0].prompt.contains("请用中文"));
    }
}
