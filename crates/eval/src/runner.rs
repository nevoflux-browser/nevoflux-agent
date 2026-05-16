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
            browser_mode: BrowserLaunchMode::DaemonOnly {
                daemon_binary: std::path::PathBuf::from("target/release/nevoflux-agent"),
                state_dir: std::path::PathBuf::from(".eval-state"),
            },
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
    pub(crate) config: RunnerConfig,
    pub(crate) client: Option<crate::daemon_client::DaemonHttpClient>,
}

impl Runner {
    pub fn new(config: RunnerConfig) -> Self {
        Self { config, client: None }
    }

    pub fn with_client(config: RunnerConfig, client: crate::daemon_client::DaemonHttpClient) -> Self {
        Self { config, client: Some(client) }
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
                Duration::from_secs(self.config.task_timeout_secs + 30),
                self.execute_task(task, benchmark, browser.as_ref()),
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

    /// Execute a single task against the daemon via HTTP + SSE.
    ///
    /// Flow:
    ///   1. Create a fresh daemon session for the task's mode
    ///   2. Apply setup steps (inject messages, seed memory, grant permissions)
    ///   3. Open SSE event stream before submitting the prompt (avoids race)
    ///   4. Submit the prompt (with optional benchmark-supplied suffix)
    ///   5. Consume events, evaluating `TerminationStrategy` after each event
    ///   6. Clean up the session (DELETE)
    ///   7. Extract `final_answer` via `AnswerExtractor` and build `TaskResult`
    async fn execute_task(
        &self,
        task: &Task,
        benchmark: &dyn Benchmark,
        _browser: &dyn BrowserHandle,
    ) -> EvalResult<TaskResult> {
        use crate::daemon_client::http::{
            CreateSessionRequest, SetupRequest, SetupStep as ClientSetupStep,
            SubmitMessageRequest,
        };
        use crate::daemon_client::sse::stream_events;
        use crate::termination::{DaemonEvent, TerminationDecision};
        use futures::StreamExt;

        let client = self
            .client
            .as_ref()
            .ok_or_else(|| EvalError::DaemonConnection("runner has no http client".into()))?;

        let started = std::time::Instant::now();

        // (1) Create session
        let mode_str = match task.mode {
            crate::NevoFluxMode::Chat => "chat",
            crate::NevoFluxMode::Browser => "browser",
            crate::NevoFluxMode::Agent => "agent",
        };
        let create = client
            .create_session(&CreateSessionRequest {
                mode: mode_str.to_string(),
                llm_backend: None,
                mock_browser: None,
                eval_run_id: None,
            })
            .await
            .map_err(|e| EvalError::DaemonError(format!("create_session: {e}")))?;
        let sid = create.session_id;

        // (2) Apply setup steps
        if !task.setup.is_empty() {
            let steps: Vec<ClientSetupStep> = task
                .setup
                .iter()
                .map(|s| match s {
                    crate::SetupStep::InjectMessage { role, content, .. } => {
                        ClientSetupStep::InjectMessage {
                            role: role.clone(),
                            content: content.clone(),
                        }
                    }
                    crate::SetupStep::SeedMemory { content } => ClientSetupStep::SeedMemory {
                        key: "memory".into(),
                        value: content.clone(),
                    },
                    crate::SetupStep::GrantPermission { resource, action } => {
                        ClientSetupStep::GrantPermission {
                            tool: format!("{resource}:{action}"),
                        }
                    }
                })
                .collect();
            let _ = client
                .setup_session(&sid, &SetupRequest { steps })
                .await
                .map_err(|e| EvalError::DaemonError(format!("setup: {e}")))?;
        }

        // (3) Open SSE stream FIRST so we don't race the submit
        let resp = client
            .open_events(&sid)
            .await
            .map_err(|e| EvalError::DaemonError(format!("open_events: {e}")))?;
        let mut events = stream_events(resp);

        // (4) Submit prompt (with optional suffix from benchmark)
        let prompt = match benchmark.prompt_suffix() {
            Some(suffix) => format!("{}{}", task.prompt, suffix),
            None => task.prompt.clone(),
        };
        let _ = client
            .submit_message(
                &sid,
                &SubmitMessageRequest {
                    prompt,
                    timeout_secs: Some(self.config.task_timeout_secs),
                },
            )
            .await
            .map_err(|e| EvalError::DaemonError(format!("submit: {e}")))?;

        // (5) Consume events until termination strategy fires or task_timeout elapses
        let strategy = benchmark.termination_strategy();
        let extractor = benchmark.answer_extractor();
        let task_timeout = Duration::from_secs(self.config.task_timeout_secs);
        let mut collected: Vec<DaemonEvent> = Vec::new();
        let mut termination_reason: Option<String> = None;

        let consume = async {
            loop {
                match events.next().await {
                    Some(Ok(evt)) => {
                        collected.push(evt);
                        match strategy.evaluate(&collected) {
                            TerminationDecision::Continue => continue,
                            TerminationDecision::Stop => break,
                            TerminationDecision::StopWithError(msg) => {
                                termination_reason = Some(msg);
                                break;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        termination_reason = Some(format!("sse error: {e}"));
                        break;
                    }
                    None => {
                        termination_reason = Some("sse stream ended unexpectedly".into());
                        break;
                    }
                }
            }
            Ok::<(), EvalError>(())
        };

        let timed = tokio::time::timeout(task_timeout, consume).await;
        let timed_out = timed.is_err();

        // (6) Cleanup: DELETE session (best-effort)
        if let Err(e) = client.delete_session(&sid).await {
            tracing::warn!(session_id = %sid, error = %e, "delete_session failed (continuing)");
        }

        // (7) Build result
        let status = if timed_out {
            TaskStatus::Timeout
        } else if termination_reason.is_some() {
            TaskStatus::Failed
        } else {
            TaskStatus::Completed
        };

        let final_answer = extractor.extract(&collected);

        Ok(TaskResult {
            task_id: task.id.clone(),
            status,
            final_answer,
            latency_ms: started.elapsed().as_millis() as u64,
            token_cost: None, // Phase 3: parse from tool_result events
            error: termination_reason,
            trace_ids: vec![], // Phase 3: read traces endpoint and extract IDs
        })
    }
}
