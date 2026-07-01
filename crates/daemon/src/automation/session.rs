//! Automation session orchestration (P3): the taint-gated retry loop that
//! drives one task to a terminal outcome.
//!
//! The per-attempt execution (clone profile → spawn browser → run the agent
//! loop with the policy allowlist + taint tracking → drain) is injected as a
//! closure, so this control flow — where the retry/caps logic bugs live — is
//! unit-tested without a browser. The injected leaf is stubbed in tests and
//! real in production.

use crate::agent_host::DaemonHostFunctions;
use crate::automation::policy::Policy;
use crate::automation::retry_decision;
use crate::http::types::TaskStatus;
use crate::registry::BrowserEntry;
use crate::wasm::services::HostServices;
use std::future::Future;

/// Result of one attempt at a task.
#[derive(Debug, Clone)]
pub struct AttemptOutcome {
    /// Whether the attempt completed the task.
    pub success: bool,
    /// Whether a mutating tool was dispatched this attempt (see [`crate::automation::taint`]).
    pub tainted: bool,
    /// Agent output, if any.
    pub output: Option<String>,
    /// Error detail, if failed.
    pub error: Option<String>,
}

/// Terminal result of a task after retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOutcome {
    /// Final status.
    pub status: TaskStatus,
    /// Number of attempts made (1 + retries).
    pub attempts: u32,
    /// Final output, if any.
    pub output: Option<String>,
    /// Final error, if failed.
    pub error: Option<String>,
}

/// Drive a task with taint-gated retry (≤3, untainted-only; `idempotent`
/// overrides taint, `no_retry` disables — see [`retry_decision`]).
///
/// `run_attempt(attempt_number)` executes ONE fresh attempt. Each retry is a
/// fresh attempt (the production leaf clones a new profile + spawns a new
/// browser), so a partially-completed attempt never resumes.
pub async fn run_with_retry<F, Fut>(policy: &Policy, mut run_attempt: F) -> SessionOutcome
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = AttemptOutcome>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let outcome = run_attempt(attempt).await;
        if outcome.success {
            return SessionOutcome {
                status: TaskStatus::Succeeded,
                attempts: attempt,
                output: outcome.output,
                error: None,
            };
        }
        if retry_decision(attempt, outcome.tainted, policy) {
            continue;
        }
        return SessionOutcome {
            status: TaskStatus::Failed,
            attempts: attempt,
            output: outcome.output,
            error: outcome.error,
        };
    }
}

/// Execute ONE task attempt against the bound browser — the production leaf of
/// the automation session (P3). Assembles the policy allowlist + browser binding
/// + non-interactive (`is_iteration`) execution into `Agent::run`, then
/// classifies the outcome for the retry gate ([`run_with_retry`]).
///
/// Taint semantics: setup failures (before the agent runs any tool — missing
/// config, etc.) are **untainted** (retryable). A failure *inside* the agent run
/// is conservatively **tainted** (not auto-retried), because `AgentOutput` alone
/// cannot prove no mutating tool ran before the error. On success, taint is
/// derived from the tools actually called.
///
/// End-to-end behavior (the agent driving a live browser) is verified only
/// against a real browser — this is the code that phase gate exercises. The
/// setup-failure classification is unit-tested here.
pub async fn execute_task_attempt(
    services_template: HostServices,
    browser: &BrowserEntry,
    policy: &Policy,
    task: &str,
    mode: nevoflux_builtin_wasm::AgentMode,
    session_id: String,
) -> AttemptOutcome {
    let Some(agent_config) = services_template.agent_config.clone() else {
        return AttemptOutcome {
            success: false,
            tainted: false,
            output: None,
            error: Some("no agent_config on services".into()),
        };
    };
    let Some(runtime_handle) = services_template.runtime_handle.clone() else {
        return AttemptOutcome {
            success: false,
            tainted: false,
            output: None,
            error: Some("no runtime_handle on services".into()),
        };
    };

    // Bind browser routing + non-interactive gating (mirrors IterationExecutor).
    let mut services = services_template.with_bound_browser(browser);
    services.is_iteration = true;
    services.session_id = session_id.clone();

    let host = DaemonHostFunctions::new(agent_config, runtime_handle)
        .with_services(services)
        .with_session_id(session_id.clone());
    let agent = nevoflux_builtin_wasm::Agent::new(host);

    let mode_tools: Vec<String> = agent
        .get_tools_for_mode(mode)
        .into_iter()
        .map(|t| t.name)
        .collect();
    let allowlist = policy.tool_allowlist(&mode_tools);

    let input = nevoflux_builtin_wasm::AgentInput {
        session_id,
        mode,
        user_message: task.to_string(),
        history: vec![],
        attachments: vec![],
        local_files: vec![],
        custom_system_prompt: None,
        tab_id: None,
        tab_ids: vec![],
        skill_context: None,
        available_models: vec![],
        mcp_servers: vec![],
        soul_context: None,
        tools_config: Some(nevoflux_protocol::subagent::ToolsConfig::Allow(allowlist)),
        os_platform: Some(std::env::consts::OS.to_string()),
    };

    // `Agent::run` is synchronous (host fns block on the stashed runtime handle
    // for async LLM calls); wrap in spawn_blocking to not hog the executor.
    let outcome = tokio::task::spawn_blocking(move || agent.run(&input)).await;
    match outcome {
        Ok(Ok(out)) => {
            let tainted = out
                .tool_calls
                .iter()
                .any(|tc| crate::automation::taint::is_mutating_tool(&tc.name));
            AttemptOutcome {
                success: true,
                tainted,
                output: Some(out.text),
                error: None,
            }
        }
        Ok(Err(e)) => AttemptOutcome {
            success: false,
            tainted: true,
            output: None,
            error: Some(e.message),
        },
        Err(e) => AttemptOutcome {
            success: false,
            tainted: true,
            output: None,
            error: Some(format!("agent task panicked: {e}")),
        },
    }
}

/// Everything the per-task orchestration needs, threaded from the daemon.
pub struct AutomationDeps {
    /// Clones per-task profiles.
    pub profile_mgr: crate::profile::ProfileManager,
    /// Base-profile name to clone (login state + tenant brain).
    pub profile: String,
    /// The available-browser registry (resolves the bound browser).
    pub registry: std::sync::Arc<crate::registry::BrowserRegistry>,
    /// Services template (carries agent_config, runtime_handle, browser_sender).
    pub services_template: HostServices,
    /// Path to the nevoflux browser binary.
    pub browser_bin: std::path::PathBuf,
    /// X11 display for the browser (e.g. `:99`), if any.
    pub display: Option<String>,
    /// Agent mode for the task.
    pub mode: nevoflux_builtin_wasm::AgentMode,
    /// Per-task workspace dir (drain target for result + debug bundle, P6/Q12).
    pub workspace: std::path::PathBuf,
}

/// Run a full task: taint-gated retry over fresh attempts, each of which clones
/// a profile → spawns a browser → resolves the binding → runs the agent leaf →
/// cleans up. Composes the individually-tested pieces (ProfileManager,
/// browser_launch, BrowserRegistry, execute_task_attempt, run_with_retry).
/// End-to-end behavior is verified against a live browser (phase gate).
pub async fn execute_full_task(
    deps: &AutomationDeps,
    policy: &Policy,
    task: &str,
) -> SessionOutcome {
    let outcome = run_with_retry(policy, |attempt| async move {
        let clone = match deps.profile_mgr.clone_base(&deps.profile) {
            Ok(c) => c,
            Err(e) => {
                return AttemptOutcome {
                    success: false,
                    tainted: false,
                    output: None,
                    error: Some(format!("profile clone failed: {e}")),
                }
            }
        };
        let _ = deps.profile_mgr.inject_automation_pref(&clone);

        let cfg = crate::browser_launch::BrowserLaunchConfig {
            browser_bin: deps.browser_bin.clone(),
            profile_dir: clone.clone(),
            display: deps.display.clone(),
            register_timeout: std::time::Duration::from_secs(60),
        };
        let result = match crate::browser_launch::spawn_and_supervise(cfg, deps.registry.clone())
            .await
        {
            Err(e) => AttemptOutcome {
                success: false,
                tainted: false, // browser never started ⇒ untainted (retryable)
                output: None,
                error: Some(format!("browser launch failed: {e}")),
            },
            Ok(mut handle) => {
                let outcome = match deps.registry.single() {
                    Ok(browser) => {
                        execute_task_attempt(
                            deps.services_template.clone(),
                            &browser,
                            policy,
                            task,
                            deps.mode,
                            format!("automation-{attempt}"),
                        )
                        .await
                    }
                    Err(e) => AttemptOutcome {
                        success: false,
                        tainted: false,
                        output: None,
                        error: Some(format!("binding failed: {e}")),
                    },
                };
                // Tear down the browser for this attempt (fresh browser per retry).
                let _ = handle.child.start_kill();
                outcome
            }
        };
        deps.profile_mgr.cleanup(&clone);
        result
    })
    .await;

    // P6/Q12 drain: write the task result to the workspace (best-effort, incl.
    // on failure) so it survives sandbox teardown. Per-step screenshots require
    // a tool-dispatch hook (follow-up); the result + a bundle manifest land here.
    let _ = std::fs::create_dir_all(&deps.workspace);
    let result_json = serde_json::json!({
        "status": format!("{:?}", outcome.status),
        "attempts": outcome.attempts,
        "output": outcome.output,
        "error": outcome.error,
    });
    let _ = std::fs::write(
        deps.workspace.join("result.json"),
        serde_json::to_string_pretty(&result_json).unwrap_or_default(),
    );
    let _ = crate::automation::bundle::DebugBundle::new().write_to(&deps.workspace);

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(tainted: bool) -> AttemptOutcome {
        AttemptOutcome {
            success: false,
            tainted,
            output: None,
            error: Some("boom".into()),
        }
    }

    fn ok() -> AttemptOutcome {
        AttemptOutcome {
            success: true,
            tainted: false,
            output: Some("done".into()),
            error: None,
        }
    }

    #[tokio::test]
    async fn untainted_retries_then_succeeds_on_third() {
        let out = run_with_retry(&Policy::browser_only(), |a| async move {
            if a < 3 {
                fail(false)
            } else {
                ok()
            }
        })
        .await;
        assert_eq!(out.status, TaskStatus::Succeeded);
        assert_eq!(out.attempts, 3);
        assert_eq!(out.output.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn untainted_gives_up_after_three_retries() {
        // Always fails untainted: attempts 1,2,3 retry; attempt 4 not retried.
        let out = run_with_retry(&Policy::browser_only(), |_a| async move { fail(false) }).await;
        assert_eq!(out.status, TaskStatus::Failed);
        assert_eq!(out.attempts, 4);
        assert_eq!(out.error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn tainted_failure_not_retried() {
        let out = run_with_retry(&Policy::browser_only(), |_a| async move { fail(true) }).await;
        assert_eq!(out.status, TaskStatus::Failed);
        assert_eq!(out.attempts, 1);
    }

    #[tokio::test]
    async fn idempotent_policy_retries_even_when_tainted() {
        let mut p = Policy::browser_only();
        p.idempotent = true;
        let out = run_with_retry(&p, |a| async move {
            if a < 2 {
                fail(true)
            } else {
                ok()
            }
        })
        .await;
        assert_eq!(out.status, TaskStatus::Succeeded);
        assert_eq!(out.attempts, 2);
    }

    #[tokio::test]
    async fn no_retry_policy_stops_immediately() {
        let mut p = Policy::browser_only();
        p.no_retry = true;
        let out = run_with_retry(&p, |_a| async move { fail(false) }).await;
        assert_eq!(out.status, TaskStatus::Failed);
        assert_eq!(out.attempts, 1);
    }

    #[tokio::test]
    async fn leaf_setup_failure_is_untainted_and_retryable() {
        use nevoflux_storage::Database;
        use std::sync::Arc;
        use std::time::Instant;
        // HostServices::new has no agent_config/runtime_handle, so the leaf
        // returns a setup failure BEFORE running the agent — untainted (so the
        // retry loop would retry it), never touching a browser.
        let db = Arc::new(Database::open_in_memory().expect("in-memory db"));
        let services = HostServices::new(db);
        let entry = BrowserEntry {
            proxy_id: "proxy-b1".into(),
            client_identity: b"proxy-b1".to_vec(),
            registered_at: Instant::now(),
            last_heartbeat: Instant::now(),
        };
        let out = execute_task_attempt(
            services,
            &entry,
            &Policy::browser_only(),
            "open example.com",
            nevoflux_builtin_wasm::AgentMode::Browser,
            "sess-1".into(),
        )
        .await;
        assert!(!out.success);
        assert!(!out.tainted, "setup failure must be untainted (retryable)");
        assert!(out.error.unwrap().contains("agent_config"));
    }
}
