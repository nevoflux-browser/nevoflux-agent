//! NevoFlux self-suite — loads YAML tasks from `eval/nevoflux-suite/<category>/*.yaml`.
//!
//! Categories (see spec §4.1): canvas-sdk, memory-recall, mcp-bidir,
//! mode-authz, privacy-audit.

use crate::{benchmarks::Benchmark, EvalError, EvalResult, Task};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

pub struct NevoFluxSuite {
    root: PathBuf,
}

impl NevoFluxSuite {
    pub fn new() -> Self {
        Self {
            root: PathBuf::from("eval/nevoflux-suite"),
        }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
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
        "NevoFlux self-suite — canvas-sdk, memory-recall, mode-authz, privacy-audit, mcp-bidir"
    }
    fn requires_network(&self) -> bool {
        false
    }
    fn default_judge(&self) -> &str {
        "structured"
    }

    async fn load_tasks(&self, filter: Option<&str>) -> EvalResult<Vec<Task>> {
        load_tasks_from(&self.root, filter).await
    }
}

async fn load_tasks_from(root: &Path, filter: Option<&str>) -> EvalResult<Vec<Task>> {
    if !root.exists() {
        return Err(EvalError::Other(format!(
            "nevoflux-suite root not found at {} (run from repo root, or set --suite-root)",
            root.display()
        )));
    }
    let mut paths: Vec<PathBuf> = vec![];
    collect_yaml(root, &mut paths)?;
    paths.sort();

    let mut tasks = Vec::new();
    for path in paths {
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(EvalError::Io)?;
        let task: Task = serde_yaml::from_str(&text).map_err(|e| EvalError::TaskParse {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        if let Some(f) = filter {
            if !task.id.contains(f) && !task.category.contains(f) {
                continue;
            }
        }
        tasks.push(task);
    }
    Ok(tasks)
}

fn collect_yaml(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_yaml(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_scaffold_yaml() {
        let suite = NevoFluxSuite::new();
        let tasks = suite.load_tasks(None).await;
        // We may not be running from repo root; tolerate either result.
        match tasks {
            Ok(ts) => assert!(!ts.is_empty(), "expected scaffold tasks to load"),
            Err(EvalError::Other(msg)) if msg.contains("nevoflux-suite root not found") => {
                // Acceptable in unit-test isolation; integration smoke covers it.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn loads_explicit_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = tmp.path().join("synthetic");
        tokio::fs::create_dir_all(&cat).await.unwrap();
        let yaml = r#"
id: synthetic-001
category: synthetic
mode: chat
prompt: "tell me about cats"
requires_browser: false
assertions:
  - type: contains_any
    targets: ["cat"]
"#;
        tokio::fs::write(cat.join("ok.yaml"), yaml).await.unwrap();

        let suite = NevoFluxSuite::with_root(tmp.path().to_path_buf());
        let tasks = suite.load_tasks(None).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "synthetic-001");
    }

    #[tokio::test]
    async fn filter_narrows_to_matching_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = tmp.path().join("c");
        tokio::fs::create_dir_all(&cat).await.unwrap();
        tokio::fs::write(
            cat.join("a.yaml"),
            r#"
id: alpha
category: c
mode: chat
prompt: "p"
requires_browser: false
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            cat.join("b.yaml"),
            r#"
id: beta
category: c
mode: chat
prompt: "p"
requires_browser: false
"#,
        )
        .await
        .unwrap();
        let suite = NevoFluxSuite::with_root(tmp.path().to_path_buf());
        let tasks = suite.load_tasks(Some("alph")).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "alpha");
    }
}
