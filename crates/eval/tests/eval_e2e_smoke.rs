//! Phase 2 end-to-end smoke. Requires the release daemon binary built at
//! `target/release/nevoflux-agent`.
//!
//! Flow:
//!   1. Build a RunnerConfig pointing at the real daemon binary (DaemonOnly mode)
//!   2. Let Runner::run spawn the daemon subprocess, wait for its lock, and wire
//!      up DaemonHttpClient internally
//!   3. Run a synthetic 1-task benchmark through the full runner pipeline
//!   4. Assert RunSummary shape and that the runner completed without crashing
//!
//! The test is skipped (with a printed message) if the binary is missing.

use async_trait::async_trait;
use nevoflux_eval::{
    benchmarks::Benchmark,
    browser::BrowserLaunchMode,
    judge::programmatic::ProgrammaticJudge,
    runner::{Runner, RunnerConfig},
    Assertion, NevoFluxMode, SignalGrade, Task,
};
use std::path::PathBuf;

struct OneTaskBench;

#[async_trait]
impl Benchmark for OneTaskBench {
    fn name(&self) -> &str {
        "smoke"
    }

    fn description(&self) -> &str {
        "synthetic 1-task smoke"
    }

    fn requires_network(&self) -> bool {
        false
    }

    fn default_judge(&self) -> &str {
        "programmatic"
    }

    async fn load_tasks(&self, _filter: Option<&str>) -> nevoflux_eval::EvalResult<Vec<Task>> {
        Ok(vec![Task {
            id: "smoke-001".into(),
            category: "smoke".into(),
            mode: NevoFluxMode::Chat,
            prompt: "say hi".into(),
            setup: vec![],
            reference: None,
            // ContainsAny with empty string always matches any (including empty) answer.
            assertions: vec![Assertion::ContainsAny {
                targets: vec!["".into()],
            }],
            requires_browser: false,
            metadata: Default::default(),
        }])
    }
}

fn daemon_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap() // crates/
        .parent()
        .unwrap() // workspace root
        .join("target/release/nevoflux-agent")
}

#[tokio::test]
async fn eval_runs_against_real_daemon() {
    // Enable mock LLM mode for this test. The forwarded env var causes the
    // spawned daemon (built with --features eval-mock-llm) to use its local
    // mock HTTP server. Without this, every task would time out waiting for
    // a real API call.
    std::env::set_var("NEVOFLUX_EVAL_LLM_MODE", "mock");
    struct EnvVarGuard(&'static str);
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }
    let _guard = EnvVarGuard("NEVOFLUX_EVAL_LLM_MODE");

    let daemon = daemon_path();
    if !daemon.exists() {
        eprintln!(
            "skipping: daemon binary not built at {}. \
             Run `cargo build --release --bin nevoflux-agent`.",
            daemon.display()
        );
        return;
    }

    let state_dir = tempfile::tempdir().unwrap();

    let config = RunnerConfig {
        daemon_addr: "".into(),
        task_timeout_secs: 15,
        parallelism: 1,
        task_filter: None,
        limit: None,
        browser_mode: BrowserLaunchMode::DaemonOnly {
            daemon_binary: daemon,
            state_dir: state_dir.path().to_path_buf(),
        },
    };
    let runner = Runner::new(config);

    let bench = OneTaskBench;
    let judge = ProgrammaticJudge::new();
    let summary = runner.run(&bench, &judge).await.expect("runner completed");

    assert_eq!(summary.total, 1, "expected 1 task total");
    assert_eq!(
        summary.signal_grade,
        SignalGrade::Exploratory,
        "DaemonOnly always produces Exploratory grade"
    );

    assert_eq!(summary.per_task.len(), 1, "expected one TaskOutcome");

    // With mock LLM, the agent should reach Stop (not timeout).
    assert_eq!(
        summary.timeouts, 0,
        "with mock LLM, the agent should reach Stop, not timeout. Got timeouts={}",
        summary.timeouts
    );

    // The task should complete (Status::Completed or Status::Failed — both
    // indicate the agent ran, distinguishing from Timeout).
    let status = summary.per_task[0].result.status;
    assert!(
        matches!(
            status,
            nevoflux_eval::TaskStatus::Completed | nevoflux_eval::TaskStatus::Failed
        ),
        "expected Completed or Failed (agent ran), got {:?}",
        status
    );

    // `passed`, `failed`, and `timeouts` are NOT mutually exclusive counters:
    // `timeouts` counts tasks whose status was Timeout, while `passed`/`failed`
    // are verdict-based (judge result). A timed-out task can still be judged
    // passed if the extractor found a match. Assert the structural invariant
    // instead: total == skipped + (passed + failed), where failed absorbs
    // everything that is neither skipped nor judged-correct.
    assert_eq!(
        summary.total,
        summary.skipped + summary.passed + summary.failed,
        "total must equal skipped + passed + failed; \
         got total={} skipped={} passed={} failed={}",
        summary.total,
        summary.skipped,
        summary.passed,
        summary.failed,
    );
}
