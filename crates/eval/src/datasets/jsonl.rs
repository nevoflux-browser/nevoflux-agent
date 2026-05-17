//! Generic JSONL loader for benchmark datasets that were pre-converted
//! from upstream (e.g. HuggingFace parquet via `datasets.to_json(...)`).
//!
//! Each line is a JSON object with at minimum `question` and `answer`
//! fields. Optional `id` field — auto-generated as `{category}-NNNN`
//! when absent.  Blank lines are skipped.

use crate::{Assertion, EvalError, EvalResult, NevoFluxMode, Task};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct JsonlRow {
    #[serde(default)]
    id: Option<String>,
    question: String,
    answer: String,
}

pub fn load(path: &Path, category: &str, prompt_suffix: &str) -> EvalResult<Vec<Task>> {
    let body = std::fs::read_to_string(path).map_err(EvalError::Io)?;
    let mut tasks = Vec::new();
    let mut emitted = 0usize;
    for (idx, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: JsonlRow = serde_json::from_str(line).map_err(|e| EvalError::TaskParse {
            path: path.display().to_string(),
            reason: format!("line {idx}: {e}"),
        })?;
        emitted += 1;
        let id = row.id.unwrap_or_else(|| format!("{category}-{emitted:04}"));
        tasks.push(Task {
            id,
            category: category.into(),
            mode: NevoFluxMode::Agent,
            prompt: format!(
                "{question}\n\n{suffix}",
                question = row.question,
                suffix = prompt_suffix
            ),
            setup: vec![],
            reference: Some(row.answer.clone()),
            assertions: vec![Assertion::ContainsAny {
                targets: vec![row.answer],
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

    #[test]
    fn loads_jsonl_with_question_answer() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let body = "{\"id\":\"r1\",\"question\":\"Q1\",\"answer\":\"A1\"}\n{\"question\":\"Q2\",\"answer\":\"A2\"}\n";
        std::fs::write(tmp.path(), body).unwrap();
        let tasks = load(tmp.path(), "test-cat", "请回答").unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "r1");
        assert_eq!(tasks[1].id, "test-cat-0002");
        assert!(tasks[0].prompt.contains("请回答"));
    }

    #[test]
    fn loads_jsonl_skips_blank_lines() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let body = "\n{\"question\":\"Q\",\"answer\":\"A\"}\n\n";
        std::fs::write(tmp.path(), body).unwrap();
        let tasks = load(tmp.path(), "cat", "").unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn malformed_line_returns_error() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "not json").unwrap();
        let err = load(tmp.path(), "cat", "").unwrap_err();
        assert!(matches!(err, EvalError::TaskParse { .. }));
    }
}
