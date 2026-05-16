//! Online-Mind2Web benchmark adapter (Phase 3b minimal slice).
//!
//! For Phase 3b this loads a hand-written 3-task fixture from
//! `eval/benchmarks/online-mind2web-fixture.json`. Phase 3c will swap the
//! source for `eval/benchmarks/Online-Mind2Web/` via git submodule.
//!
//! The wire shape:
//! ```json
//! { "version": "...", "source": "...", "tasks": [{"id", "url",
//!   "instruction", "evaluation_criteria"}] }
//! ```

use crate::{benchmarks::Benchmark, EvalError, EvalResult, NevoFluxMode, Task};
use async_trait::async_trait;
use nevoflux_protocol::subagent::ToolsConfig;
use serde::Deserialize;
use std::path::PathBuf;

pub struct OnlineMind2Web {
    fixture_path: PathBuf,
}

impl OnlineMind2Web {
    pub fn new() -> Self {
        Self {
            fixture_path: PathBuf::from("eval/benchmarks/online-mind2web-fixture.json"),
        }
    }

    pub fn with_fixture(path: PathBuf) -> Self {
        Self { fixture_path: path }
    }
}

impl Default for OnlineMind2Web {
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
    url: String,
    instruction: String,
    evaluation_criteria: String,
}

#[async_trait]
impl Benchmark for OnlineMind2Web {
    fn name(&self) -> &str {
        "online-mind2web"
    }

    fn description(&self) -> &str {
        "Online-Mind2Web — 300 real-website tasks (Phase 3b: 3-task fixture; Phase 3c: full submodule)"
    }

    fn requires_network(&self) -> bool {
        true
    }

    fn default_judge(&self) -> &str {
        "webjudge"
    }

    fn tools_config(&self) -> ToolsConfig {
        // Enable browser tools so the agent can navigate. `ToolsConfig`
        // variants in `nevoflux_protocol::subagent` are `None` (no tools)
        // and `Allow(Vec<String>)` (allowlist with wildcard support).
        // `browser_*` matches all browser-prefixed tools.
        ToolsConfig::Allow(vec!["browser_*".to_string()])
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
            let mut metadata = serde_json::Map::new();
            metadata.insert("url".into(), serde_json::Value::String(ft.url.clone()));
            metadata.insert(
                "evaluation_criteria".into(),
                serde_json::Value::String(ft.evaluation_criteria),
            );
            tasks.push(Task {
                id: ft.id,
                category: "online-mind2web".into(),
                mode: NevoFluxMode::Browser,
                prompt: format!(
                    "Open this URL in the browser: {}\n\nThen do: {}",
                    ft.url, ft.instruction
                ),
                setup: vec![],
                reference: None,
                assertions: vec![],
                requires_browser: true,
                metadata,
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
    {
      "id": "om2w-test-001",
      "url": "https://example.com/",
      "instruction": "do a thing",
      "evaluation_criteria": "agent did the thing"
    }
  ]
}
"#;
        tokio::fs::write(&path, raw).await.unwrap();
        let bench = OnlineMind2Web::with_fixture(path);
        let tasks = bench.load_tasks(None).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "om2w-test-001");
        assert!(tasks[0].requires_browser);
        assert_eq!(tasks[0].mode, NevoFluxMode::Browser);
        assert!(tasks[0].prompt.contains("https://example.com/"));
        assert_eq!(
            tasks[0]
                .metadata
                .get("evaluation_criteria")
                .and_then(|v| v.as_str()),
            Some("agent did the thing")
        );
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
    {"id": "alpha", "url": "u", "instruction": "i", "evaluation_criteria": "e"},
    {"id": "beta", "url": "u", "instruction": "i", "evaluation_criteria": "e"}
  ]
}
"#;
        tokio::fs::write(&path, raw).await.unwrap();
        let bench = OnlineMind2Web::with_fixture(path);
        let tasks = bench.load_tasks(Some("alph")).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "alpha");
    }
}
