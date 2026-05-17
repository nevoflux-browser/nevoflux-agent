//! BrowseComp benchmark adapter (Phase 3c minimal slice).
//!
//! For Phase 3c this loads a hand-written 5-task fixture from
//! `eval/benchmarks/browsecomp-fixture.json`. Phase 3d/4 will swap the
//! source for the decrypted openai/simple-evals browse_comp_test_set.csv
//! (XOR-encrypted to prevent training contamination — see
//! `eval/README-DATASETS.md`).

use crate::{benchmarks::Benchmark, Assertion, EvalError, EvalResult, NevoFluxMode, Task};
use async_trait::async_trait;
use nevoflux_protocol::subagent::ToolsConfig;
use serde::Deserialize;
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

#[derive(Debug, Deserialize)]
struct FixtureFile {
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    source: String,
    tasks: Vec<FixtureTask>,
}

#[derive(Debug, Deserialize)]
struct FixtureTask {
    id: String,
    question: String,
    answer: String,
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
        ToolsConfig::Allow(vec!["browser_*".to_string(), "web_search".to_string()])
    }

    async fn load_tasks(&self, filter: Option<&str>) -> EvalResult<Vec<Task>> {
        let raw = tokio::fs::read_to_string(&self.fixture_path)
            .await
            .map_err(EvalError::Io)?;
        let file: FixtureFile = serde_json::from_str(&raw).map_err(|e| EvalError::TaskParse {
            path: self.fixture_path.display().to_string(),
            reason: e.to_string(),
        })?;
        let mut tasks = Vec::new();
        for ft in file.tasks {
            if let Some(f) = filter {
                if !ft.id.contains(f) {
                    continue;
                }
            }
            tasks.push(Task {
                id: ft.id,
                category: "browsecomp".into(),
                mode: NevoFluxMode::Agent,
                prompt: format!(
                    "{question}\n\nReply with just the short answer (1-5 words).",
                    question = ft.question
                ),
                setup: vec![],
                reference: Some(ft.answer.clone()),
                assertions: vec![Assertion::ContainsAny {
                    targets: vec![ft.answer.clone()],
                }],
                requires_browser: false,
                metadata: Default::default(),
                supports_platform: vec![],
            });
        }
        Ok(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
