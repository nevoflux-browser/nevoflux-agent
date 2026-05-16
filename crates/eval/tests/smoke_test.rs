//! Smoke tests — verify pipeline pieces work end-to-end without a real daemon.

use nevoflux_eval::{
    benchmarks::Benchmark,
    browser::BrowserLaunchMode,
    judge::{structured::StructuredJudge, Judge},
    Assertion, NevoFluxMode, SignalGrade, Task, TaskResult, TaskStatus,
};
use tempfile::TempDir;

#[tokio::test]
async fn structured_judge_passes_when_all_assertions_match() {
    let task = Task {
        id: "smoke-001".into(),
        category: "test".into(),
        mode: NevoFluxMode::Agent,
        prompt: "What is 1+1?".into(),
        setup: vec![],
        reference: None,
        assertions: vec![Assertion::ContainsAny {
            targets: vec!["2".into(), "two".into()],
        }],
        requires_browser: false,
        metadata: serde_json::Map::new(),
    };

    let result = TaskResult {
        task_id: "smoke-001".into(),
        status: TaskStatus::Completed,
        final_answer: Some("The answer is 2.".into()),
        latency_ms: 100,
        token_cost: None,
        error: None,
        trace_ids: vec![],
    };

    let judge = StructuredJudge::new();
    let verdict = judge.judge(&task, &result).await.unwrap();
    assert!(verdict.correct, "expected pass: {}", verdict.explanation);
}

#[tokio::test]
async fn yaml_loader_round_trip_with_requires_browser() {
    let dir = TempDir::new().unwrap();
    let category_dir = dir.path().join("canvas-sdk");
    std::fs::create_dir_all(&category_dir).unwrap();

    let yaml = r#"
id: canvas-sdk-smoke
category: canvas_sdk
mode: agent
prompt: "Test prompt"
requires_browser: true
assertions:
  - type: contains_any
    targets: ["foo", "bar"]
"#;
    std::fs::write(category_dir.join("test.yaml"), yaml).unwrap();

    std::env::set_var(
        "NEVOFLUX_SUITE_ROOT",
        dir.path().to_string_lossy().to_string(),
    );

    let suite = nevoflux_eval::benchmarks::nevoflux_suite::NevoFluxSuite::new();
    let tasks = suite.load_tasks(None).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, "canvas-sdk-smoke");
    assert!(tasks[0].requires_browser);
}

#[tokio::test]
async fn browser_mode_grade_routing() {
    assert_eq!(BrowserLaunchMode::DaemonOnly.signal_grade(), SignalGrade::Exploratory);
    assert_eq!(
        BrowserLaunchMode::ExternalDevInstance {
            endpoint: "http://localhost:5959".into()
        }
        .signal_grade(),
        SignalGrade::Exploratory
    );
    assert_eq!(
        BrowserLaunchMode::ReleaseBinary {
            version: "v0.3.2".into(),
            cache_dir: ".cache".into()
        }
        .signal_grade(),
        SignalGrade::Authoritative
    );
}

#[tokio::test]
async fn browser_mode_supports_check() {
    assert!(!BrowserLaunchMode::DaemonOnly.supports_browser_tasks());
    assert!(BrowserLaunchMode::ExternalDevInstance {
        endpoint: "x".into()
    }
    .supports_browser_tasks());
    assert!(BrowserLaunchMode::ReleaseBinary {
        version: "v".into(),
        cache_dir: ".".into()
    }
    .supports_browser_tasks());
}

#[tokio::test]
async fn registry_finds_known_benchmarks() {
    assert!(nevoflux_eval::benchmarks::find("browsecomp").is_some());
    assert!(nevoflux_eval::benchmarks::find("nevoflux-suite").is_some());
    assert!(nevoflux_eval::benchmarks::find("does-not-exist").is_none());
}

#[tokio::test]
async fn judge_registry_finds_known_judges() {
    assert!(nevoflux_eval::judge::find("programmatic").is_some());
    assert!(nevoflux_eval::judge::find("structured").is_some());
}

#[tokio::test]
async fn task_status_helpers() {
    assert!(TaskStatus::SkippedNoBrowser.is_skipped());
    assert!(!TaskStatus::Completed.is_skipped());
    assert!(TaskStatus::Completed.ran_to_completion());
    assert!(!TaskStatus::Timeout.ran_to_completion());
}
