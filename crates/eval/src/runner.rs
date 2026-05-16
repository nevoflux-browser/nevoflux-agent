//! Runner — drives a benchmark against a NevoFlux daemon and (optionally) browser.
//!
//! Flow per task:
//!   1. Check `task.requires_browser` against `BrowserLaunchMode`
//!      → if mismatch, mark `SkippedNoBrowser` and continue
//!   2. Apply `Task::setup` (inject prior session messages, seed memory, grant perms)
//!   3. Open a fresh session in the requested mode (Chat / Browser / Agent)
//!   4. Send `Task::prompt` and stream until completion or timeout
//!   5. Capture `final_answer`, `latency`, `token_cost`, `trace_ids`
//!   6. Hand off to the configured Judge for verdict
//!   7. Aggregate metrics; emit per-task line to JSONL trace

use crate::{
    benchmarks::Benchmark,
    browser::{self, BrowserHandle, BrowserLaunchMode},
    judge::{Judge, Verdict},
    EvalError, EvalResult, SignalGrade, Task, TaskResult, TaskStatus,
};
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub daemon_addr: String,
    pub task_timeout_secs: u64,
    pub parallelism: usize,
    pub task_filter: Option<String>,
    pub limit: Option<usize>,
    pub browser_mode: BrowserLaunchMode,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            daemon_addr: "127.0.0.1:19500".into(),
            task_timeout_secs: 300,
            parallelism: 1,
            task_filter: None,
            limit: None,
            browser_mode: BrowserLaunchMode::DaemonOnly,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunSummary {
    pub benchmark: String,
    pub judge: String,
    /// Authoritative iff browser came from a published release binary.
    pub signal_grade: SignalGrade,
    pub browser_version: String,
    /// Total tasks loaded (before any skipping).
    pub total: usize,
    /// Tasks that ran AND were judged correct.
    pub passed: usize,
    /// Tasks that ran but were judged incorrect or errored.
    pub failed: usize,
    /// Tasks skipped because they require a browser but DaemonOnly was selected.
    pub skipped: usize,
    pub timeouts: usize,
    pub mean_latency_ms: f64,
    pub p99_latency_ms: u64,
    pub total_token_cost_usd: f64,
    pub total_judge_cost_usd: f64,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub per_task: Vec<TaskOutcome>,
}

impl RunSummary {
    /// Accuracy excludes skipped tasks from the denominator.
    pub fn effective_total(&self) -> usize {
        self.total.saturating_sub(self.skipped)
    }

    pub fn accuracy(&self) -> f64 {
        let denom = self.effective_total();
        if denom == 0 {
            0.0
        } else {
            self.passed as f64 / denom as f64
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskOutcome {
    pub task: Task,
    pub result: TaskResult,
    /// `None` when task was skipped (no judging performed).
    pub verdict: Option<Verdict>,
}

pub struct Runner {
    config: RunnerConfig,
}

impl Runner {
    pub fn new(config: RunnerConfig) -> Self {
        Self { config }
    }

    pub async fn run(
        &self,
        benchmark: &dyn Benchmark,
        judge: &dyn Judge,
    ) -> EvalResult<RunSummary> {
        let started_at = chrono::Utc::now();
        let signal_grade = self.config.browser_mode.signal_grade();

        // Bring up browser (no-op for DaemonOnly).
        let browser = browser::launch(&self.config.browser_mode).await?;
        browser.ensure_ready().await?;
        let browser_version = browser.version_string();

        info!(
            benchmark = benchmark.name(),
            judge = judge.name(),
            browser = %browser_version,
            grade = ?signal_grade,
            "starting eval"
        );

        let mut tasks = benchmark
            .load_tasks(self.config.task_filter.as_deref())
            .await?;
        if let Some(limit) = self.config.limit {
            tasks.truncate(limit);
        }
        info!(loaded = tasks.len(), "tasks loaded");

        let mut outcomes = Vec::with_capacity(tasks.len());
        let mut latencies = Vec::with_capacity(tasks.len());
        let mut timeouts = 0usize;
        let mut skipped = 0usize;
        let mut total_token_cost = 0.0;
        let mut total_judge_cost = 0.0;

        for (i, task) in tasks.iter().enumerate() {
            // Browser availability check.
            if task.requires_browser && !browser.is_real_browser() {
                info!(id = %task.id, "skipping: task requires browser, runner is DaemonOnly");
                skipped += 1;
                outcomes.push(TaskOutcome {
                    task: task.clone(),
                    result: TaskResult {
                        task_id: task.id.clone(),
                        status: TaskStatus::SkippedNoBrowser,
                        final_answer: None,
                        latency_ms: 0,
                        token_cost: None,
                        error: None,
                        trace_ids: vec![],
                    },
                    verdict: None,
                });
                continue;
            }

            info!(
                progress = format!("{}/{}", i + 1, tasks.len()),
                id = %task.id,
                "running task"
            );

            let started = Instant::now();
            let exec_result = timeout(
                Duration::from_secs(self.config.task_timeout_secs),
                self.execute_task(task, browser.as_ref()),
            )
            .await;

            let result = match exec_result {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    error!(id = %task.id, error = %e, "task execution failed");
                    TaskResult {
                        task_id: task.id.clone(),
                        status: TaskStatus::Failed,
                        final_answer: None,
                        latency_ms: started.elapsed().as_millis() as u64,
                        token_cost: None,
                        error: Some(e.to_string()),
                        trace_ids: vec![],
                    }
                }
                Err(_) => {
                    timeouts += 1;
                    warn!(id = %task.id, "task timed out");
                    TaskResult {
                        task_id: task.id.clone(),
                        status: TaskStatus::Timeout,
                        final_answer: None,
                        latency_ms: (self.config.task_timeout_secs * 1000) as u64,
                        token_cost: None,
                        error: Some(format!(
                            "timeout after {}s",
                            self.config.task_timeout_secs
                        )),
                        trace_ids: vec![],
                    }
                }
            };

            latencies.push(result.latency_ms);
            if let Some(ref c) = result.token_cost {
                total_token_cost += c.usd;
            }

            let verdict = judge.judge(task, &result).await?;
            total_judge_cost += verdict.judge_cost_usd;

            outcomes.push(TaskOutcome {
                task: task.clone(),
                result,
                verdict: Some(verdict),
            });
        }

        if let Err(e) = browser.shutdown().await {
            warn!(error = %e, "browser shutdown returned error");
        }

        latencies.sort_unstable();
        let mean_latency_ms = if latencies.is_empty() {
            0.0
        } else {
            latencies.iter().sum::<u64>() as f64 / latencies.len() as f64
        };
        let p99_latency_ms = if latencies.is_empty() {
            0
        } else {
            let idx = ((latencies.len() as f64) * 0.99).ceil() as usize - 1;
            latencies[idx.min(latencies.len() - 1)]
        };

        let passed = outcomes
            .iter()
            .filter(|o| o.verdict.as_ref().map(|v| v.correct).unwrap_or(false))
            .count();
        let total = outcomes.len();
        let failed = total - passed - skipped;

        Ok(RunSummary {
            benchmark: benchmark.name().to_string(),
            judge: judge.name().to_string(),
            signal_grade,
            browser_version,
            total,
            passed,
            failed,
            skipped,
            timeouts,
            mean_latency_ms,
            p99_latency_ms,
            total_token_cost_usd: total_token_cost,
            total_judge_cost_usd: total_judge_cost,
            started_at,
            finished_at: chrono::Utc::now(),
            per_task: outcomes,
        })
    }

    /// Execute a single task — STUB.
    ///
    /// Concrete implementation must:
    ///   1. Use `nevoflux_daemon_client::DaemonClient::connect(&self.config.daemon_addr)`
    ///   2. Apply `task.setup` steps (CreateSession, AppendMessage, GrantPermission)
    ///   3. Open session in `task.mode` and dispatch the prompt
    ///   4. If `task.requires_browser`, route page actions through `browser`
    ///   5. Stream events until `Stop`; collect text into `final_answer`
    ///   6. Read the `traces` SQLite table for trace IDs
    ///   7. Compute token cost from LLM provider response metadata
    async fn execute_task(
        &self,
        task: &Task,
        _browser: &dyn BrowserHandle,
    ) -> EvalResult<TaskResult> {
        let _ = task;
        Err(EvalError::DaemonConnection(
            "Runner::execute_task not yet wired to DaemonClient — see TODO".into(),
        ))
    }
}
