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
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
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
        Self {
            config,
            client: None,
        }
    }

    pub fn with_client(
        config: RunnerConfig,
        client: crate::daemon_client::DaemonHttpClient,
    ) -> Self {
        Self {
            config,
            client: Some(client),
        }
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

        // Derive an HTTP client from the browser lock when the caller did not supply one
        // explicitly.  DaemonOnlyBrowser writes a daemon.lock that contains the HTTP address
        // and bearer token; we read it here so that Runner::new (the default path) actually
        // reaches the daemon instead of erroring with "runner has no http client".
        //
        // Runner::with_client stays for Phase 3+ (External/Release modes have their own
        // discovery paths and pass in a pre-built client).
        let derived_client: Option<crate::daemon_client::DaemonHttpClient> =
            self.client.clone().or_else(|| {
                browser
                    .lock()
                    .map(crate::daemon_client::DaemonHttpClient::from_lock)
            });

        if derived_client.is_none() {
            return Err(crate::EvalError::DaemonConnection(
                "no HTTP client available — runner has no pre-built client and the \
                 browser handle does not expose a daemon lock (non-daemon-only modes \
                 are Phase 3/4 stubs)"
                    .into(),
            ));
        }

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
        let mut total_token_cost = 0.0f64;
        let mut total_judge_cost = 0.0f64;

        // Compute effective parallelism: bounded by config and benchmark's own cap.
        let effective_parallelism = self
            .config
            .parallelism
            .min(benchmark.max_parallelism(&self.config.browser_mode))
            .max(1);
        tracing::info!(parallelism = effective_parallelism, "effective concurrency");

        let semaphore = Arc::new(Semaphore::new(effective_parallelism));

        // Partition tasks into skipped (no browser) and to-run.
        let mut to_run: Vec<Task> = Vec::with_capacity(tasks.len());
        for task in tasks.iter() {
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
                        observed_events: vec![],
                    },
                    verdict: None,
                });
            } else {
                to_run.push(task.clone());
            }
        }

        // Dispatch tasks into a FuturesUnordered pool, interleaving dispatch and collection
        // to avoid deadlock when parallelism < to_run.len().
        //
        // Pattern: pre-fill up to effective_parallelism slots, then drain-one / refill-one.
        // This ensures pending futures are always being polled (via the drain loop) while
        // new permits are being acquired, so tasks in flight can complete and release permits.
        let mut pending: FuturesUnordered<
            std::pin::Pin<
                Box<dyn std::future::Future<Output = (Task, EvalResult<TaskResult>)> + Send>,
            >,
        > = FuturesUnordered::new();

        let timeout_secs = self.config.task_timeout_secs;
        // Use the derived client (pre-built or lock-derived) for all per-task futures.
        let client = derived_client;

        let mut task_iter = to_run.into_iter();

        // Pre-fill up to effective_parallelism slots (or until tasks exhausted).
        for _ in 0..effective_parallelism {
            if let Some(task) = task_iter.next() {
                let permit = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| EvalError::Other(format!("semaphore: {e}")))?;
                let client_for_task = client.clone();
                info!(id = %task.id, "queuing task");
                pending.push(Box::pin(execute_task_owned(
                    task,
                    benchmark,
                    browser.as_ref(),
                    client_for_task,
                    timeout_secs,
                    permit,
                )));
            }
        }

        // Drain one at a time; refill from task_iter after each completion.
        // Collect results as they complete (order is non-deterministic; outcomes are appended
        // to the vec as they arrive — the RunSummary consumer sorts by task.id if needed).
        while let Some((task, exec_result)) = pending.next().await {
            let result = match exec_result {
                Ok(r) => r,
                Err(e) => {
                    error!(id = %task.id, error = %e, "task execution failed");
                    TaskResult {
                        task_id: task.id.clone(),
                        status: TaskStatus::Failed,
                        final_answer: None,
                        latency_ms: 0,
                        token_cost: None,
                        error: Some(e.to_string()),
                        trace_ids: vec![],
                        observed_events: vec![],
                    }
                }
            };

            latencies.push(result.latency_ms);
            if let Some(ref c) = result.token_cost {
                total_token_cost += c.usd;
            }
            if matches!(result.status, TaskStatus::Timeout) {
                timeouts += 1;
            }

            // Judge runs synchronously here (in the collector loop) so it can borrow `judge`
            // from the outer scope without needing to move it into each per-task future.
            let verdict = match judge.judge(&task, &result).await {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(id = %task.id, error = %e, "judge failed");
                    None
                }
            };
            if let Some(ref v) = verdict {
                total_judge_cost += v.judge_cost_usd;
            }

            outcomes.push(TaskOutcome {
                task,
                result,
                verdict,
            });

            // Refill the drained slot from the remaining task iterator.
            if let Some(next_task) = task_iter.next() {
                let permit = semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| EvalError::Other(format!("semaphore: {e}")))?;
                let client_for_task = client.clone();
                info!(id = %next_task.id, "queuing task");
                pending.push(Box::pin(execute_task_owned(
                    next_task,
                    benchmark,
                    browser.as_ref(),
                    client_for_task,
                    timeout_secs,
                    permit,
                )));
            }
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
    /// Thin wrapper kept for direct callers / tests that bypass `run`.
    #[allow(dead_code)]
    pub(crate) async fn execute_task(
        &self,
        task: &Task,
        benchmark: &dyn Benchmark,
        _browser: &dyn BrowserHandle,
    ) -> EvalResult<TaskResult> {
        execute_task_impl(
            task,
            benchmark,
            self.client.as_ref(),
            self.config.task_timeout_secs,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Free functions — own all inputs so they can be stored in FuturesUnordered
// ---------------------------------------------------------------------------

/// Wrapper that takes full ownership of `task` and a semaphore permit.
/// The permit is dropped when the future resolves, releasing the concurrency slot.
async fn execute_task_owned<'b>(
    task: Task,
    benchmark: &'b dyn Benchmark,
    _browser: &'b dyn BrowserHandle,
    client: Option<crate::daemon_client::DaemonHttpClient>,
    task_timeout_secs: u64,
    _permit: tokio::sync::OwnedSemaphorePermit,
) -> (Task, EvalResult<TaskResult>) {
    let result = execute_task_impl(&task, benchmark, client.as_ref(), task_timeout_secs).await;
    (task, result)
}

/// Core execution logic — borrows `task` and `benchmark`, owns `client` reference.
///
/// Flow:
///   1. Create a fresh daemon session for the task's mode
///   2. Apply setup steps (inject messages, seed memory, grant permissions)
///   3. Open SSE event stream before submitting the prompt (avoids race)
///   4. Submit the prompt (with optional benchmark-supplied suffix)
///   5. Consume events, evaluating `TerminationStrategy` after each event
///   6. Clean up the session (DELETE)
///   7. Extract `final_answer` via `AnswerExtractor` and build `TaskResult`
async fn execute_task_impl(
    task: &Task,
    benchmark: &dyn Benchmark,
    client: Option<&crate::daemon_client::DaemonHttpClient>,
    task_timeout_secs: u64,
) -> EvalResult<TaskResult> {
    use crate::daemon_client::http::{
        CreateSessionRequest, SetupRequest, SetupStep as ClientSetupStep, SubmitMessageRequest,
    };
    use crate::daemon_client::sse::stream_events;
    use crate::termination::{DaemonEvent, TerminationDecision};
    use futures::StreamExt;

    let client =
        client.ok_or_else(|| EvalError::DaemonConnection("runner has no http client".into()))?;

    let started = Instant::now();

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
                crate::SetupStep::InjectMessage {
                    session,
                    role,
                    content,
                } => ClientSetupStep::InjectMessage {
                    session: if session.is_empty() {
                        None
                    } else {
                        Some(session.clone())
                    },
                    role: role.clone(),
                    content: content.clone(),
                },
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
                timeout_secs: Some(task_timeout_secs),
                tools_config: Some(
                    serde_json::to_value(benchmark.tools_config())
                        .unwrap_or(serde_json::Value::Null),
                ),
            },
        )
        .await
        .map_err(|e| EvalError::DaemonError(format!("submit: {e}")))?;

    // (5) Consume events until termination strategy fires or task_timeout elapses
    let strategy = benchmark.termination_strategy();
    let extractor = benchmark.answer_extractor();
    let task_timeout = Duration::from_secs(task_timeout_secs);
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

    // Fetch traces and extract daemon_event names + trace ids
    let mut observed_events: Vec<String> = Vec::new();
    let mut trace_ids: Vec<String> = Vec::new();
    match client.get_traces(&sid).await {
        Ok(body) => {
            let entries = crate::daemon_client::traces::parse_jsonl(&body);
            trace_ids = entries.iter().map(|e| e.id.to_string()).collect();
            observed_events = crate::daemon_client::traces::event_names(&entries);
        }
        Err(e) => {
            tracing::warn!(session_id = %sid, error = %e, "get_traces failed (continuing)");
        }
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
        trace_ids,
        observed_events,
    })
}
