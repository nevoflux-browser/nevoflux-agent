//! Online-Mind2Web data-dir loader.
//!
//! The upstream repo has per-task directories under `data/example/<task_id>/`
//! containing `result.json` files.  Phase 3d extracts just the task
//! metadata fields we need (url + instruction + evaluation_criteria) and
//! builds a Browser-mode `Task`.  Malformed `result.json` files are
//! skipped with a warn-level log (better than failing the whole load).
//!
//! Caller passes a path to the `data/` directory (or any directory whose
//! immediate children are task directories) after cloning the upstream
//! repo.  See `eval/README-DATASETS.md` for the fetch procedure.

use crate::{EvalError, EvalResult, NevoFluxMode, Task};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ResultJson {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    confirmed_task: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    evaluation_criteria: Option<String>,
}

pub fn load(data_dir: &Path) -> EvalResult<Vec<Task>> {
    if !data_dir.is_dir() {
        return Err(EvalError::Other(format!(
            "om2w data dir not found at {}",
            data_dir.display()
        )));
    }
    let mut tasks = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(data_dir)
        .map_err(EvalError::Io)?
        .collect::<Result<_, _>>()
        .map_err(EvalError::Io)?;
    // Deterministic order across filesystems.
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let task_dir = entry.path();
        if !task_dir.is_dir() {
            continue;
        }
        let result_path = task_dir.join("result.json");
        if !result_path.is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(&result_path).map_err(EvalError::Io)?;
        let r: ResultJson = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = ?result_path, error = %e, "skipping malformed result.json");
                continue;
            }
        };
        let id = r.task_id.unwrap_or_else(|| {
            task_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });
        let url = r.url.unwrap_or_default();
        let instruction = r.confirmed_task.or(r.description).unwrap_or_default();
        let evaluation_criteria = r.evaluation_criteria.unwrap_or_else(|| {
            "Agent completed the task successfully per Online-Mind2Web criteria.".into()
        });
        let mut metadata = serde_json::Map::new();
        metadata.insert("url".into(), serde_json::Value::String(url.clone()));
        metadata.insert(
            "evaluation_criteria".into(),
            serde_json::Value::String(evaluation_criteria),
        );
        tasks.push(Task {
            id,
            category: "online-mind2web".into(),
            mode: NevoFluxMode::Browser,
            prompt: format!("Open this URL in the browser: {url}\n\nThen do: {instruction}"),
            setup: vec![],
            reference: None,
            assertions: vec![],
            requires_browser: true,
            metadata,
            supports_platform: vec![],
        });
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_data_dir_with_result_json() {
        let tmp = tempfile::tempdir().unwrap();
        let task_dir = tmp.path().join("task-abc");
        std::fs::create_dir_all(&task_dir).unwrap();
        let result_json = r#"
{
  "task_id": "task-abc",
  "url": "https://example.com",
  "confirmed_task": "click the button",
  "evaluation_criteria": "button click confirmed"
}
"#;
        std::fs::write(task_dir.join("result.json"), result_json).unwrap();
        let tasks = load(tmp.path()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "task-abc");
        assert!(tasks[0].prompt.contains("click the button"));
        assert_eq!(
            tasks[0].metadata.get("url").and_then(|v| v.as_str()),
            Some("https://example.com")
        );
    }

    #[test]
    fn skips_malformed_result_json() {
        let tmp = tempfile::tempdir().unwrap();
        let task_dir = tmp.path().join("task-bad");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("result.json"), "not json").unwrap();
        let tasks = load(tmp.path()).unwrap();
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn returns_error_when_data_dir_missing() {
        let p = std::path::PathBuf::from("/does-not-exist-zzzz");
        assert!(matches!(load(&p), Err(EvalError::Other(_))));
    }
}
