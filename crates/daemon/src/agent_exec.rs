//! Shared unattended agent-execution kernel.
//!
//! [`run_agent_once`] performs a single, non-interactive agent turn: it
//! constructs a production [`crate::agent_host::DaemonHostFunctions`] from a
//! [`HostServices`] snapshot, resolves the mode's tool catalog into an
//! allowlist (minus forbidden names / prefixes), and runs
//! [`nevoflux_builtin_wasm::Agent::run`] on a blocking task.
//!
//! It was extracted verbatim from the `/loop` `IterationExecutor` production
//! path (`loops::executor`) so that other unattended surfaces (schedules,
//! goals) reuse the exact same host wiring and behavior. The loop executor is
//! now a thin caller of this kernel; behavior on the loops path is preserved.
//!
//! The kernel is only entered on the *production* path — the loop executor's
//! stub path (no [`HostServices`]) never reaches here.

use crate::wasm::services::HostServices;
use nevoflux_builtin_wasm::{AgentInput, Message, MessageRole};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Forward-compatibility token budget shared across an unattended run.
///
/// **Not yet wired** into [`crate::agent_host::DaemonHostFunctions`]: a later
/// task threads it into the host so per-call usage accrues here and the agent
/// loop can stop when [`Self::exceeded`] flips. For now [`run_agent_once`]
/// only *holds* the handle (via [`AgentExecRequest::token_budget`]) so callers
/// can begin constructing budgets without a second refactor — the value is
/// intentionally not read during execution.
#[derive(Debug)]
pub struct TokenBudget {
    /// Hard ceiling in tokens.
    pub limit: u64,
    /// Tokens spent so far.
    pub spent: AtomicU64,
}

impl TokenBudget {
    /// Create a shared budget with the given token ceiling.
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            spent: AtomicU64::new(0),
        })
    }

    /// Add `n` tokens to the running total.
    pub fn add(&self, n: u64) {
        self.spent.fetch_add(n, Ordering::Relaxed);
    }

    /// True once spend has reached or exceeded the limit.
    pub fn exceeded(&self) -> bool {
        self.spent.load(Ordering::Relaxed) >= self.limit
    }
}

/// Request describing one unattended agent turn.
pub struct AgentExecRequest {
    /// Session the run belongs to (drives artifact scoping + proxy borrow).
    pub session_id: String,
    /// Agent mode; picks the base tool catalog via `get_tools_for_mode`.
    pub mode: nevoflux_builtin_wasm::AgentMode,
    /// Fully-formed user message (callers do any templating themselves).
    pub user_message: String,
    /// Exact tool names to strip from the mode catalog. Loops pass their
    /// `is_forbidden_in_iteration` set (`loop_create`, `ask_user`).
    pub forbidden_tools: Vec<String>,
    /// Tool-name prefixes to strip from the mode catalog (e.g. `browser_`).
    /// Empty for loops today; a later task uses it to drop `browser_*` /
    /// `computer_*` for surfaces that must run without a browser.
    pub forbidden_prefixes: Vec<String>,
    /// Marks `services.is_iteration` (permission auto-approve + iteration
    /// gate). Loops pass `true`.
    pub unattended: bool,
    /// Sets `services.iteration_loop_id` (loop iterations only).
    pub iteration_loop_id: Option<String>,
    /// Borrow the session's most recently active sidebar proxy for `browser_*`
    /// routing. Ignored when [`Self::bound_browser`] is `Some`. Loops pass
    /// `true`.
    pub borrow_proxy: bool,
    /// Route `browser_*` tools to an explicitly-bound browser instead of a
    /// borrowed sidebar proxy. When `Some`, takes precedence over
    /// [`Self::borrow_proxy`]. Loops pass `None` (a later task supplies it).
    pub bound_browser: Option<crate::registry::BrowserEntry>,
    /// Prior conversation as `(role, text)` pairs; converted into
    /// [`AgentInput::history`]. Loops pass empty.
    pub history: Vec<(String, String)>,
    /// Forward-compat token budget; held but **not** yet wired into the host.
    /// See [`TokenBudget`].
    pub token_budget: Option<Arc<TokenBudget>>,
}

/// Result of a single unattended agent turn.
pub struct AgentExecOutcome {
    /// Final assistant text.
    pub text: String,
    /// json array `[{"name": ..., "ok": true}]` — the same trace shape the
    /// loop executor persists to `loop_iterations.summary_json`.
    pub trace: serde_json::Value,
}

/// Filter a mode's full tool catalog down to the run allowlist by removing any
/// name that exactly matches an entry in `names` or begins with any entry in
/// `prefixes`. Pure, so it is unit-tested without constructing an `Agent`.
pub fn filter_allowlist(all: Vec<String>, names: &[String], prefixes: &[String]) -> Vec<String> {
    all.into_iter()
        .filter(|n| !names.iter().any(|f| f == n))
        .filter(|n| !prefixes.iter().any(|p| n.starts_with(p.as_str())))
        .collect()
}

/// Convert `(role, text)` history pairs into agent [`Message`]s. Unknown roles
/// fall back to [`MessageRole::User`].
fn history_to_messages(history: Vec<(String, String)>) -> Vec<Message> {
    history
        .into_iter()
        .map(|(role, text)| {
            let role = match role.to_ascii_lowercase().as_str() {
                "system" => MessageRole::System,
                "assistant" => MessageRole::Assistant,
                "tool" => MessageRole::Tool,
                _ => MessageRole::User,
            };
            Message {
                role,
                content: text,
                tool_call_id: None,
                tool_calls: Vec::new(),
                attachments: Vec::new(),
                reasoning: None,
            }
        })
        .collect()
}

/// Run a single unattended agent turn.
///
/// Requires `services.agent_config` and `services.runtime_handle` (both set at
/// daemon boot); returns `Err` with the same diagnostic strings the loop
/// executor used to surface when either is missing. Any agent-run failure or
/// task panic is returned as `Err(message)`; success yields the final text and
/// the tool-call trace.
pub async fn run_agent_once(
    services: &HostServices,
    req: AgentExecRequest,
) -> Result<AgentExecOutcome, String> {
    // `token_budget` is intentionally *not* read yet: a later task wires it
    // into `DaemonHostFunctions` so per-call usage accrues against it. Holding
    // it in the request keeps callers forward-compatible without a re-plumb.
    let _ = &req.token_budget;

    let agent_config = services
        .agent_config
        .as_ref()
        .cloned()
        .ok_or_else(|| "HostServices has no agent_config — bug at server boot".to_string())?;
    let runtime_handle = services
        .runtime_handle
        .as_ref()
        .cloned()
        .ok_or_else(|| "HostServices has no runtime_handle — bug at server boot".to_string())?;

    let mut services_for_run = services.clone();
    services_for_run.session_id = req.session_id.clone();
    // Mark this clone as unattended so permission handlers in
    // `wasm::mcp_tool_executor` and `agent_host` short-circuit dialogs (there
    // is no interactive sidebar the user explicitly authorized — auto-approve
    // keeps the interaction silent; the caller's tool allowlist is the gate).
    services_for_run.is_iteration = req.unattended;
    services_for_run.iteration_loop_id = req.iteration_loop_id.clone();

    // Browser routing. An explicitly-bound browser wins (headless model);
    // otherwise optionally borrow the session's most recently active sidebar so
    // `browser_*` tools dispatched from inside the run can reach a content
    // script. Without either, the run has `proxy_id=""` and the daemon's writer
    // lookup drops the request (see `server.rs::No writer for proxy ""`).
    if let Some(entry) = req.bound_browser.as_ref() {
        services_for_run = services_for_run.with_bound_browser(entry);
    } else if req.borrow_proxy {
        if let Some(tracker) = services_for_run.session_proxy_tracker.as_ref() {
            if let Some(entry) = tracker.latest(&req.session_id) {
                tracing::info!(
                    iteration_loop_id = ?req.iteration_loop_id,
                    session_id = %req.session_id,
                    borrowed_proxy = %entry.proxy_id,
                    "unattended run borrowed sidebar proxy"
                );
                services_for_run.proxy_id = entry.proxy_id;
                services_for_run.client_identity = entry.client_identity;
            } else {
                tracing::warn!(
                    iteration_loop_id = ?req.iteration_loop_id,
                    session_id = %req.session_id,
                    "unattended run could not borrow a sidebar proxy — browser_* tools will fail"
                );
            }
        }
    }

    // Back-fill loop_manager handle: the stored services snapshot may have
    // `loop_manager: None` (chicken-and-egg at construction time), but the run
    // may call `loop_scratchpad_{set,get}` via the MCP tool surface, which
    // dispatches through `mcp_tool_executor` and needs `services.loop_manager`.
    // Resolve via the process-global OnceLock set by server.rs at startup.
    if services_for_run.loop_manager.is_none() {
        if let Some(mgr) = crate::loops::CURRENT_LOOP_MANAGER.get() {
            services_for_run.loop_manager = Some(mgr.clone());
        }
    }
    // Reset interrupt flag so a stray prior cancel doesn't poison this run.
    services_for_run.reset_interrupt();

    let host = crate::agent_host::DaemonHostFunctions::new(agent_config, runtime_handle)
        .with_services(services_for_run)
        .with_session_id(req.session_id.clone());

    let agent = nevoflux_builtin_wasm::Agent::new(host);
    // Build the run allowlist from the mode's canonical tool catalog, then
    // strip forbidden names + prefixes. Passing as `ToolsConfig::Allow(...)`
    // enforces the filter even where `Agent::run`'s mode-default would include
    // an otherwise-forbidden tool.
    let allowlist = filter_allowlist(
        agent
            .get_tools_for_mode(req.mode)
            .into_iter()
            .map(|t| t.name)
            .collect(),
        &req.forbidden_tools,
        &req.forbidden_prefixes,
    );

    let input = AgentInput {
        session_id: req.session_id.clone(),
        mode: req.mode,
        user_message: req.user_message,
        history: history_to_messages(req.history),
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

    // `Agent::run` is synchronous; the host functions block on the runtime
    // handle stashed at construction time for any async LLM calls. Wrapping in
    // `spawn_blocking` keeps us from hogging the dispatcher's executor thread.
    let outcome: Result<nevoflux_builtin_wasm::AgentOutput, nevoflux_builtin_wasm::HostError> =
        tokio::task::spawn_blocking(move || agent.run(&input))
            .await
            .unwrap_or_else(|e| {
                Err(nevoflux_builtin_wasm::HostError {
                    code: 500,
                    message: format!("agent task panicked: {e}"),
                })
            });

    match outcome {
        Ok(out) => {
            let trace = serde_json::Value::Array(
                out.tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "name": tc.name,
                            "ok": true,
                        })
                    })
                    .collect(),
            );
            Ok(AgentExecOutcome {
                text: out.text,
                trace,
            })
        }
        Err(e) => Err(e.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TokenBudget arithmetic ----------------------------------------

    #[test]
    fn token_budget_starts_empty() {
        let b = TokenBudget::new(100);
        assert_eq!(b.limit, 100);
        assert!(!b.exceeded());
    }

    #[test]
    fn token_budget_accumulates() {
        let b = TokenBudget::new(100);
        b.add(30);
        b.add(50);
        assert!(!b.exceeded());
        assert_eq!(b.spent.load(Ordering::Relaxed), 80);
    }

    #[test]
    fn token_budget_exceeded_at_limit_boundary() {
        let b = TokenBudget::new(100);
        b.add(100);
        // `>=`: hitting the ceiling exactly counts as exceeded.
        assert!(b.exceeded());
    }

    #[test]
    fn token_budget_exceeded_over_limit() {
        let b = TokenBudget::new(10);
        b.add(7);
        assert!(!b.exceeded());
        b.add(5);
        assert!(b.exceeded());
        assert_eq!(b.spent.load(Ordering::Relaxed), 12);
    }

    #[test]
    fn token_budget_zero_limit_is_immediately_exceeded() {
        let b = TokenBudget::new(0);
        assert!(b.exceeded());
    }

    #[test]
    fn token_budget_shared_across_clones() {
        let b = TokenBudget::new(100);
        let b2 = Arc::clone(&b);
        b.add(60);
        b2.add(60);
        // Both handles point at the same atomic.
        assert_eq!(b.spent.load(Ordering::Relaxed), 120);
        assert!(b2.exceeded());
    }

    // ---- filter_allowlist ----------------------------------------------

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn filter_allowlist_no_filters_returns_all() {
        let all = names(&["read", "write", "browser_query"]);
        let out = filter_allowlist(all.clone(), &[], &[]);
        assert_eq!(out, all);
    }

    #[test]
    fn filter_allowlist_removes_exact_names() {
        let all = names(&["read", "loop_create", "ask_user", "write"]);
        let out = filter_allowlist(all, &names(&["loop_create", "ask_user"]), &[]);
        assert_eq!(out, names(&["read", "write"]));
    }

    #[test]
    fn filter_allowlist_removes_by_prefix() {
        let all = names(&["read", "browser_query", "browser_click", "computer_shot"]);
        let out = filter_allowlist(all, &[], &names(&["browser_"]));
        assert_eq!(out, names(&["read", "computer_shot"]));
    }

    #[test]
    fn filter_allowlist_removes_by_names_and_prefixes() {
        let all = names(&[
            "read",
            "write",
            "loop_create",
            "browser_query",
            "computer_shot",
        ]);
        let out = filter_allowlist(
            all,
            &names(&["loop_create"]),
            &names(&["browser_", "computer_"]),
        );
        assert_eq!(out, names(&["read", "write"]));
    }

    #[test]
    fn filter_allowlist_unknown_forbidden_name_is_noop() {
        let all = names(&["read", "write"]);
        let out = filter_allowlist(all.clone(), &names(&["does_not_exist"]), &[]);
        assert_eq!(out, all);
    }

    #[test]
    fn filter_allowlist_preserves_order() {
        let all = names(&["c", "a", "b", "drop_me", "d"]);
        let out = filter_allowlist(all, &names(&["drop_me"]), &[]);
        assert_eq!(out, names(&["c", "a", "b", "d"]));
    }

    #[test]
    fn filter_allowlist_empty_prefix_string_matches_everything() {
        // An empty prefix is a starts_with("") match on every name — documents
        // that callers must not pass "" unless they intend to strip all tools.
        let all = names(&["read", "write"]);
        let out = filter_allowlist(all, &[], &names(&[""]));
        assert!(out.is_empty());
    }

    // ---- history_to_messages -------------------------------------------

    #[test]
    fn history_maps_roles() {
        let hist = vec![
            ("system".to_string(), "sys".to_string()),
            ("user".to_string(), "u".to_string()),
            ("assistant".to_string(), "a".to_string()),
            ("tool".to_string(), "t".to_string()),
            ("weird".to_string(), "fallback".to_string()),
        ];
        let msgs = history_to_messages(hist);
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0].role, MessageRole::System);
        assert_eq!(msgs[0].content, "sys");
        assert_eq!(msgs[1].role, MessageRole::User);
        assert_eq!(msgs[2].role, MessageRole::Assistant);
        assert_eq!(msgs[3].role, MessageRole::Tool);
        // Unknown role falls back to User.
        assert_eq!(msgs[4].role, MessageRole::User);
        assert_eq!(msgs[4].content, "fallback");
    }

    #[test]
    fn history_role_matching_is_case_insensitive() {
        let hist = vec![("ASSISTANT".to_string(), "hi".to_string())];
        let msgs = history_to_messages(hist);
        assert_eq!(msgs[0].role, MessageRole::Assistant);
    }

    #[test]
    fn history_empty_yields_empty() {
        assert!(history_to_messages(vec![]).is_empty());
    }
}
