//! Shared QA-fixture parser used by both BrowseComp adapters.
//!
//! The two BrowseComp adapters (English + Chinese) load identical
//! `FixtureFile { version, source, tasks: Vec<{id, question, answer}> }`
//! shapes. This module factors out the common parse + filter logic so
//! adapter modules only carry the bits that actually differ (category
//! name, prompt suffix, tools_config, etc.).

use crate::{Assertion, EvalError, EvalResult, NevoFluxMode, Task};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct QaFixtureFile {
    #[allow(dead_code)]
    pub version: String,
    #[allow(dead_code)]
    pub source: String,
    pub tasks: Vec<QaFixtureTask>,
}

#[derive(Debug, Deserialize)]
pub struct QaFixtureTask {
    pub id: String,
    pub question: String,
    pub answer: String,
}

/// Load a QA-fixture JSON file and project each row into a `Task`.
///
/// * `category` — Task::category value (e.g. "browsecomp", "browsecomp-zh").
/// * `prompt_suffix` — appended to each task's question (e.g.
///   "Reply with just the short answer (1-5 words).").
/// * `filter` — optional substring filter on Task::id (matches the
///   existing `Benchmark::load_tasks` CLI filter semantics).
pub async fn load_qa_fixture(
    path: &Path,
    category: &str,
    prompt_suffix: &str,
    filter: Option<&str>,
) -> EvalResult<Vec<Task>> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(EvalError::Io)?;
    let file: QaFixtureFile = serde_json::from_str(&raw).map_err(|e| EvalError::TaskParse {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    let mut tasks = Vec::with_capacity(file.tasks.len());
    for ft in file.tasks {
        if let Some(f) = filter {
            if !ft.id.contains(f) {
                continue;
            }
        }
        tasks.push(Task {
            id: ft.id,
            category: category.into(),
            mode: NevoFluxMode::Agent,
            prompt: format!(
                "{question}\n\n{suffix}",
                question = ft.question,
                suffix = prompt_suffix
            ),
            setup: vec![],
            reference: Some(ft.answer.clone()),
            assertions: vec![Assertion::ContainsAny {
                targets: vec![ft.answer],
            }],
            requires_browser: false,
            metadata: Default::default(),
            supports_platform: vec![],
        });
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_qa_fixture_and_applies_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.json");
        let raw = r#"
{
  "version": "v",
  "source": "s",
  "tasks": [
    { "id": "x-001", "question": "Q1", "answer": "A1" },
    { "id": "y-002", "question": "Q2", "answer": "A2" }
  ]
}
"#;
        tokio::fs::write(&path, raw).await.unwrap();
        let tasks = load_qa_fixture(&path, "cat", "SUFFIX", None).await.unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].category, "cat");
        assert!(tasks[0].prompt.contains("Q1"));
        assert!(tasks[0].prompt.contains("SUFFIX"));
        assert_eq!(tasks[0].reference, Some("A1".into()));
    }

    #[tokio::test]
    async fn filter_substring_narrows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.json");
        let raw = r#"
{
  "version": "v",
  "source": "s",
  "tasks": [
    { "id": "alpha", "question": "?", "answer": "A" },
    { "id": "beta",  "question": "?", "answer": "B" }
  ]
}
"#;
        tokio::fs::write(&path, raw).await.unwrap();
        let tasks = load_qa_fixture(&path, "cat", "", Some("alph"))
            .await
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "alpha");
    }
}
