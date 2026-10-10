//! The agent channel's real tool backend (M2a §2).
//!
//! One per agent pairing. Every call takes the same steps, in this order, and
//! any of them can end it with a structured error (§5):
//!
//! 0. **the whitelist** — the dispatcher already refuses anything else; checked
//!    again here so a caller bug cannot widen the channel;
//! 1. **the tier** — a hard ceiling, never a question (decision A1);
//! 2. **the browser** — which desktop browser would answer, decided before
//!    anything is sent, so a missing browser fails at once rather than after
//!    a timeout;
//! 3. **the tab** — the agent works in tabs it opened itself unless it names
//!    one (decision A5), and those tabs open in the background;
//! 4. **the rules** — Jev and the site/pack pipeline, judged the way the MCP
//!    dispatcher judges, with any question put to the agent's client instead
//!    of the desktop sidebar;
//! 5. **the call** — through the same browser path the MCP dispatcher uses,
//!    bounded by [`EXECUTE_TIMEOUT`];
//! 6. **one audit line** — tool, outcome, duration; never arguments or
//!    results, which can hold what was typed.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nevoflux_mcp::rmcp::model::{CallToolResult, ContentBlock, Tool};
use nevoflux_mcp::rmcp::ErrorData;
use nevoflux_protocol::session_event::{SessionEventPayload, ToolOrigin};
use nevoflux_protocol::ExecutionTier;
use serde_json::{json, Value};

use super::agent_tabs::AgentTabs;
use super::mcp_tools::{
    is_allowed, AgentError, AgentToolBackend, Approval, Approver, REASON_APPROVAL_FAILED,
    REASON_CLIENT_CANNOT_APPROVE, REASON_DECLINED, REASON_RULE,
};
use super::pairing::{Pairing, PairingStore};
use crate::session_events::SessionEventWriter;
use crate::tool_pipeline::{AskPrompt, Judgement};
use crate::wasm::services::HostServices;

/// `ToolContext.origin` / the log's `origin` for every agent-channel call.
pub const AGENT_TOOL_ORIGIN: &str = "mcp:agent";

/// The longest a browser call may take before the agent is told it failed.
/// Above the extension's own 30 s and the upload path's 120 s, so a slow but
/// live browser still answers; a dead re-address would otherwise wait for the
/// ten-minute registry sweep.
pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(150);

/// Where an agent's new tab opens (spec §3.5, probe S1).
pub enum NewTabPlacement {
    /// `{new_tab: true, background: true}` — a background tab in the
    /// person's window (verdict A).
    Background,
    /// `{new_tab: true, window: "agent"}` — an unfocused window of the
    /// agent's own (verdict B).
    AgentWindow,
}

/// S1's verdict. Flipping it is the whole change if background tabs fail.
pub const NEW_TAB_PLACEMENT: NewTabPlacement = NewTabPlacement::Background;

/// Where a pairing's tier comes from.
pub trait TierSource: Send + Sync {
    fn tier(&self) -> ExecutionTier;
}

/// The tier stored in `agent-pairings.json`, read on every call so
/// `/agent-tier` takes effect on the agent's next call.
pub struct StoredTier {
    store: Arc<PairingStore>,
    channel_id: String,
}

impl StoredTier {
    pub fn new(store: Arc<PairingStore>, channel_id: &str) -> Self {
        Self {
            store,
            channel_id: channel_id.to_string(),
        }
    }
}

impl TierSource for StoredTier {
    /// Anything that goes wrong reads as `read-only`: a store that cannot
    /// be read must narrow an agent, never unlock it.
    fn tier(&self) -> ExecutionTier {
        match self.store.load() {
            Ok(all) => all
                .iter()
                .find(|p| p.control_channel_id == self.channel_id)
                .map(Pairing::agent_tier)
                .unwrap_or(ExecutionTier::ReadOnly),
            Err(e) => {
                tracing::warn!(target: "remote", "agent tier unreadable, treating as read-only: {e}");
                ExecutionTier::ReadOnly
            }
        }
    }
}

/// Decides what the rules say about one call (step 4).
#[async_trait]
pub trait Judge: Send + Sync {
    async fn judge(&self, name: &str, arguments: &Value, services: &HostServices) -> Judgement;
}

/// The daemon's own judgement (spec §2.2 step 4): Jev first — the same
/// `should_assess` / `assess` / `RISK_P` the native gate uses — then the
/// site/pack pipeline the MCP dispatcher runs. Every question is collected;
/// any refusal wins.
pub struct PipelineJudge {
    packs_dir: PathBuf,
}

impl PipelineJudge {
    /// Judge against the packs installed for this daemon.
    pub fn installed() -> Self {
        Self::at(crate::paths::resolve_from_daemon().packs_dir())
    }

    pub fn at(packs_dir: PathBuf) -> Self {
        Self { packs_dir }
    }
}

#[async_trait]
impl Judge for PipelineJudge {
    async fn judge(&self, name: &str, arguments: &Value, services: &HostServices) -> Judgement {
        let mut jev = Vec::new();
        // The tier already lets this call through without asking, which is
        // exactly the case Jev exists for (`would_auto = true`).
        if crate::jev::permission::should_assess(name, true) {
            if let Some(cfg) = services.agent_config.as_deref() {
                let risk = crate::jev::permission::assess(
                    cfg,
                    &services.database,
                    &services.session_id,
                    None,
                    name,
                    &arguments.to_string(),
                    None,
                )
                .await;
                if let Some(p) = risk.filter(|p| *p >= crate::jev::permission::RISK_P) {
                    SessionEventWriter::new(services.database.clone(), services.session_id.clone())
                        .append(SessionEventPayload::PermissionJev {
                            tool: name.to_string(),
                            p,
                            unattended: false,
                        });
                    jev.push(AskPrompt {
                        stage: "jev",
                        prompt: format!("Jev rates this action as risky ({p:.2})."),
                    });
                }
            }
        }
        let gate = crate::wasm::mcp_tool_executor::judge_mcp_call(
            name,
            arguments,
            services,
            &self.packs_dir,
        );
        match gate {
            Judgement::Deny(denial) => Judgement::Deny(denial),
            Judgement::Ask {
                mut prompts,
                arguments,
            } => {
                jev.append(&mut prompts);
                Judgement::Ask {
                    prompts: jev,
                    arguments,
                }
            }
            Judgement::Allow if !jev.is_empty() => Judgement::Ask {
                prompts: jev,
                arguments: None,
            },
            Judgement::Rewrite { arguments } if !jev.is_empty() => Judgement::Ask {
                prompts: jev,
                arguments: Some(arguments),
            },
            other => other,
        }
    }
}

/// The daemon's services for one call, or `None` before startup finished.
pub type ServicesFn = Arc<dyn Fn() -> Option<HostServices> + Send + Sync>;
/// The browser connection that would answer a call from `sender`.
pub type TargetFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The first eight ASCII-alphanumerics of a pairing's channel id: enough to
/// tell agents apart in a log and a session list, short enough to read.
pub fn short_id(channel_id: &str) -> String {
    channel_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect()
}

/// The daemon's fully wired services, published once startup is done.
///
/// Late on purpose: `ControlDeps` (and the agent channels restored from it)
/// are built before the asset server and the recorder exist, and an upload
/// without the asset server fails. Read per call, so a channel restored
/// before this is set answers `browser_unavailable` until it is.
static SERVICES: OnceLock<HostServices> = OnceLock::new();

/// Publish the services agent calls clone from. Called once from `server.rs`.
pub fn set_services_template(services: HostServices) {
    let _ = SERVICES.set(services);
}

fn services_template() -> Option<HostServices> {
    SERVICES.get().cloned()
}

/// Which tab a call runs in (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabPlan {
    /// As the agent sent it: it named a tab, or the tool needs none.
    AsGiven,
    /// In this tab of the agent's own.
    Owned(i64),
    /// In a tab opened for the agent now.
    OpenNew,
}

/// Whether `new_tab` asks for a new tab the way the extension would read it:
/// JavaScript truthiness, so `"true"` and `1` count as well as `true`. Read
/// any narrower and a truthy non-bool would slip past the plan and open a
/// foreground tab the agent does not own.
fn wants_new_tab(arguments: &Value) -> bool {
    match arguments.get("new_tab") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => false,
    }
}

/// A `tab_id`, when there is one, must be a positive integer: Firefox ids
/// are, and the extension reads a falsy one (`0`) as "the active tab".
fn check_tab_id(arguments: &Value) -> Result<(), AgentError> {
    match arguments.get("tab_id") {
        None | Some(Value::Null) => Ok(()),
        Some(v) if v.as_i64().is_some_and(|id| id > 0) => Ok(()),
        Some(_) => Err(AgentError::ToolFailed {
            detail: "tab_id must be a positive integer".to_string(),
        }),
    }
}

/// Decide the tab before anything runs.
pub fn plan_tab(
    tool: &str,
    arguments: &Value,
    primary: Option<i64>,
) -> Result<TabPlan, AgentError> {
    check_tab_id(arguments)?;
    if tool == "browser_navigate" && wants_new_tab(arguments) {
        return Ok(TabPlan::OpenNew);
    }
    if tool == "browser_get_tabs" || arguments.get("tab_id").and_then(Value::as_i64).is_some() {
        return Ok(TabPlan::AsGiven);
    }
    if tool == "browser_activate_tab" {
        // Without an id the extension would activate whatever is active.
        return Err(AgentError::ToolFailed {
            detail: "tab_id is required".to_string(),
        });
    }
    match (tool, primary) {
        (_, Some(id)) => Ok(TabPlan::Owned(id)),
        ("browser_navigate", None) => Ok(TabPlan::OpenNew),
        (_, None) => Err(AgentError::NoAgentTab),
    }
}

/// Rewrite the arguments to say what [`plan_tab`] decided. Placement keys
/// and `workspace_dir` are the daemon's to set, never the agent's: they are
/// dropped from whatever the agent sent first. So is `new_tab`, which only
/// [`TabPlan::OpenNew`] puts back, as a real `true`.
pub fn apply_plan(plan: TabPlan, arguments: &mut Value) {
    let Some(args) = arguments.as_object_mut() else {
        return;
    };
    args.remove("new_tab");
    args.remove("background");
    args.remove("window");
    // Uploads come from the default workspace only; a caller-chosen
    // workspace would make the whole disk uploadable.
    args.remove("workspace_dir");
    match plan {
        TabPlan::AsGiven => {}
        TabPlan::Owned(id) => {
            args.insert("tab_id".into(), json!(id));
        }
        TabPlan::OpenNew => {
            args.remove("tab_id");
            args.insert("new_tab".into(), json!(true));
            match NEW_TAB_PLACEMENT {
                NewTabPlacement::Background => {
                    args.insert("background".into(), json!(true));
                }
                NewTabPlacement::AgentWindow => {
                    args.insert("window".into(), json!("agent"));
                }
            }
        }
    }
}

/// Put a rule's rewrite through what [`apply_plan`] did to the agent's own
/// arguments: placement keys and `workspace_dir` dropped, `new_tab` set only
/// for [`TabPlan::OpenNew`], and any `tab_id` a positive integer. A rule may
/// still move the call to another tab: rules are the person's own config,
/// and the approval question names the tab that will run.
fn settle(plan: TabPlan, mut arguments: Value) -> Result<Value, AgentError> {
    if !arguments.is_object() {
        return Err(AgentError::ToolFailed {
            detail: "a rule rewrote the arguments into something that is not an object".to_string(),
        });
    }
    let again = if plan == TabPlan::OpenNew {
        TabPlan::OpenNew
    } else {
        TabPlan::AsGiven
    };
    apply_plan(again, &mut arguments);
    check_tab_id(&arguments)?;
    Ok(arguments)
}

/// Whether a browser failure means the tab no longer exists. The extension
/// says so as `3001: Tab not found`, `-1: Tab not found`, or Firefox's own
/// `Invalid tab ID: N`.
pub fn is_tab_gone(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    d.starts_with("3001:")
        || d.contains("tab not found")
        || d.contains("invalid tab id")
        || d.contains("no tab with id")
}

/// Add `owned: true|false` to each tab in a `browser_get_tabs` result.
pub fn annotate_owned(result: &str, owned: &[i64]) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(result) else {
        return result.to_string();
    };
    let tabs = if v.is_array() {
        v.as_array_mut()
    } else {
        v.get_mut("tabs").and_then(Value::as_array_mut)
    };
    if let Some(tabs) = tabs {
        for tab in tabs {
            let id = tab
                .get("id")
                .or_else(|| tab.get("tab_id"))
                .and_then(Value::as_i64);
            if let Some(obj) = tab.as_object_mut() {
                obj.insert(
                    "owned".into(),
                    json!(id.is_some_and(|i| owned.contains(&i))),
                );
            }
        }
    }
    v.to_string()
}

/// The question put to the person: the action as the desktop dialog words it
/// (`describe_tool_action`), the tab, and why it is being asked.
pub fn approval_message(
    tool: &str,
    arguments: &Value,
    target: &str,
    prompts: &[AskPrompt],
) -> String {
    let action = crate::wasm::mcp_tool_executor::describe_tool_action(tool, &arguments.to_string());
    let why: Vec<&str> = prompts.iter().map(|p| p.prompt.as_str()).collect();
    format!("{action}\nTab: {target}\nWhy: {}", why.join("\n"))
}

/// Runs an agent pairing's tool calls against the desktop browser.
pub struct DesktopBrowserBackend {
    channel_id: String,
    short: String,
    tier: Arc<dyn TierSource>,
    tabs: Arc<AgentTabs>,
    judge: Arc<dyn Judge>,
    services: ServicesFn,
    target: TargetFn,
}

impl DesktopBrowserBackend {
    pub fn new(
        channel_id: &str,
        tier: Arc<dyn TierSource>,
        tabs: Arc<AgentTabs>,
        judge: Arc<dyn Judge>,
        services: ServicesFn,
        target: TargetFn,
    ) -> Self {
        Self {
            channel_id: channel_id.to_string(),
            short: short_id(channel_id),
            tier,
            tabs,
            judge,
            services,
            target,
        }
    }

    /// The production backend for one agent pairing.
    pub fn for_pairing(pairing: &Pairing, store: Arc<PairingStore>) -> Self {
        let services: ServicesFn = Arc::new(services_template);
        let target: TargetFn = Arc::new(|sender: &str| {
            crate::registry::browser_target(sender).map(|(proxy, _)| proxy)
        });
        Self::new(
            &pairing.control_channel_id,
            Arc::new(StoredTier::new(store, &pairing.control_channel_id)),
            super::agent_tabs::shared(),
            Arc::new(PipelineJudge::installed()),
            services,
            target,
        )
    }

    /// The daemon's services, dressed as this agent (spec §2.3).
    fn call_services(&self) -> Option<HostServices> {
        let mut s = (self.services)()?
            .with_own_session_state()
            .with_client_context(
                Vec::new(),
                format!("{}{}", crate::registry::AGENT_PROXY_PREFIX, self.short),
            )
            .with_session_id(format!("agent-{}", self.short));
        s.tool_origin = AGENT_TOOL_ORIGIN.to_string();
        // The native gate (`check_tool_permission`) and the permission handler
        // auto-approve an iteration. An agent's call must never look like one.
        s.is_iteration = false;
        s.iteration_loop_id = None;
        Some(s)
    }

    /// Whether `served_by` is still the browser a call from this agent goes to.
    fn still_served_by(&self, services: &HostServices, served_by: &str) -> bool {
        (self.target)(&services.proxy_id).as_deref() == Some(served_by)
    }

    fn tab_label(&self, plan: TabPlan, arguments: &Value) -> String {
        if plan == TabPlan::OpenNew {
            return "a new tab of the agent's".to_string();
        }
        match arguments.get("tab_id").and_then(Value::as_i64) {
            Some(id) => self
                .tabs
                .url_of(&self.channel_id, id)
                .map(|u| format!("{u} (tab {id})"))
                .unwrap_or_else(|| format!("tab {id}")),
            None => "no particular tab".to_string(),
        }
    }

    async fn run(
        &self,
        name: &str,
        arguments: Value,
        approver: &dyn Approver,
        services: Option<HostServices>,
    ) -> Result<String, AgentError> {
        // 0. The whitelist, again: never trust the caller to have checked.
        if !is_allowed(name) {
            return Err(AgentError::UnknownTool);
        }
        // 1. A ceiling, not a question (decision A1).
        let tier = self.tier.tier();
        if !nevoflux_protocol::tier_auto_approves(name, tier) {
            return Err(AgentError::TierDenied {
                tier,
                required: nevoflux_protocol::minimum_tier(name),
            });
        }
        // 2. Which browser would answer — before anything is sent.
        let services = services.ok_or(AgentError::BrowserUnavailable)?;
        let served_by = (self.target)(&services.proxy_id).ok_or(AgentError::BrowserUnavailable)?;
        // 3. The tab.
        let mut arguments = if arguments.is_object() {
            arguments
        } else {
            json!({})
        };
        let primary = self.tabs.primary(&self.channel_id, &served_by);
        let plan = plan_tab(name, &arguments, primary.map(|t| t.id))?;
        apply_plan(plan, &mut arguments);
        // 4. The rules, judged on exactly the arguments that will run.
        let arguments = match self.judge.judge(name, &arguments, &services).await {
            Judgement::Allow => arguments,
            Judgement::Rewrite {
                arguments: rewritten,
            } => settle(plan, rewritten)?,
            Judgement::Deny(denial) => {
                return Err(AgentError::ApprovalDenied {
                    reason: REASON_RULE,
                    message: format!("{} [{}]", denial.message, denial.code),
                })
            }
            Judgement::Ask {
                prompts,
                arguments: rewritten,
            } => {
                // The person is asked about exactly what would run: a rule's
                // rewrite, when there is one, not what the agent sent.
                let effective = match rewritten {
                    Some(rewritten) => settle(plan, rewritten)?,
                    None => arguments,
                };
                let target = self.tab_label(plan, &effective);
                let message = approval_message(name, &effective, &target, &prompts);
                match approver.ask(message).await {
                    Approval::Approved => effective,
                    Approval::Declined => {
                        return Err(AgentError::ApprovalDenied {
                            reason: REASON_DECLINED,
                            message: format!("the person declined {name}"),
                        })
                    }
                    Approval::Unsupported => {
                        return Err(AgentError::ApprovalDenied {
                            reason: REASON_CLIENT_CANNOT_APPROVE,
                            message: format!(
                            "{name} needs the person's approval and this client cannot ask for it"
                        ),
                        })
                    }
                    Approval::TimedOut => return Err(AgentError::ApprovalTimeout),
                    Approval::Failed => {
                        return Err(AgentError::ApprovalDenied {
                            reason: REASON_APPROVAL_FAILED,
                            message: format!("the approval request for {name} failed"),
                        })
                    }
                }
            }
        };
        // 5. The call, bounded. The browser may have changed while the rules
        // or the person were deciding, and the request is re-addressed when
        // it is sent: a tab number planned for the old browser could be one
        // of the person's tabs in the new one. Nothing is sent then.
        if !self.still_served_by(&services, &served_by) {
            return Err(match plan {
                TabPlan::Owned(tab_id) => {
                    self.tabs.forget_pairing(&self.channel_id);
                    AgentError::TabClosed { tab_id }
                }
                _ => AgentError::BrowserUnavailable,
            });
        }
        let call =
            crate::wasm::mcp_tool_executor::execute_agent_browser_tool(name, &arguments, &services);
        let outcome = match tokio::time::timeout(EXECUTE_TIMEOUT, call).await {
            Ok(outcome) => outcome,
            Err(_) => Err("the browser did not answer".to_string()),
        };
        match outcome {
            Ok(text) => Ok(self.after_success(name, plan, &arguments, &services, &served_by, text)),
            Err(detail) => Err(self.after_failure(&arguments, detail)),
        }
    }

    fn after_success(
        &self,
        name: &str,
        plan: TabPlan,
        arguments: &Value,
        services: &HostServices,
        served_by: &str,
        text: String,
    ) -> String {
        match name {
            "browser_navigate" => {
                let parsed: Option<Value> = serde_json::from_str(&text).ok();
                let tab = parsed
                    .as_ref()
                    .and_then(|v| v.get("tab_id"))
                    .and_then(Value::as_i64);
                let url = parsed
                    .as_ref()
                    .and_then(|v| v.get("url"))
                    .and_then(Value::as_str)
                    .or_else(|| arguments.get("url").and_then(Value::as_str))
                    .map(str::to_string);
                match (plan, tab) {
                    // Filed under a browser that no longer answers, the id
                    // would point at whatever tab the new one gives it.
                    (TabPlan::OpenNew, Some(id)) if self.still_served_by(services, served_by) => {
                        self.tabs
                            .record_opened(&self.channel_id, served_by, id, url)
                    }
                    (TabPlan::OpenNew, Some(_)) => tracing::warn!(
                        target: "remote",
                        channel = %self.short,
                        "the browser changed while a tab was opened for the agent; not adopting it"
                    ),
                    (TabPlan::OpenNew, None) => tracing::warn!(
                        target: "remote",
                        channel = %self.short,
                        "a tab was opened for the agent but the browser did not say which"
                    ),
                    (_, Some(id)) => self.tabs.record_url(&self.channel_id, id, url),
                    _ => {}
                }
                text
            }
            "browser_get_tabs" => {
                annotate_owned(&text, &self.tabs.owned_ids(&self.channel_id, served_by))
            }
            _ => text,
        }
    }

    fn after_failure(&self, arguments: &Value, detail: String) -> AgentError {
        if is_tab_gone(&detail) {
            if let Some(id) = arguments.get("tab_id").and_then(Value::as_i64) {
                self.tabs.forget_tab(&self.channel_id, id);
                return AgentError::TabClosed { tab_id: id };
            }
        }
        AgentError::ToolFailed { detail }
    }
}

#[async_trait]
impl AgentToolBackend for DesktopBrowserBackend {
    fn tools(&self) -> Vec<Tool> {
        super::mcp_tools::agent_tools()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        approver: &dyn Approver,
    ) -> Result<CallToolResult, ErrorData> {
        let services = self.call_services();
        let writer = services
            .as_ref()
            .map(|s| SessionEventWriter::new(s.database.clone(), s.session_id.clone()));
        let id = format!("agent-{}", uuid::Uuid::new_v4());
        // Who asked and for what — never with what: a fill carries what was
        // typed, a navigate where someone is going.
        if let Some(w) = &writer {
            w.append(SessionEventPayload::ToolCall {
                id: id.clone(),
                name: name.to_string(),
                args: json!({}),
                origin: ToolOrigin::from_raw(AGENT_TOOL_ORIGIN),
                tab_url: None,
            });
        }
        let started = Instant::now();
        let outcome = self.run(name, arguments, approver, services).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let label = match &outcome {
            Ok(_) => "ok",
            Err(e) => e.code(),
        };
        if let Some(w) = &writer {
            w.append(SessionEventPayload::ToolResult {
                id,
                content: label.to_string(),
                is_error: outcome.is_err(),
                duration_ms,
            });
        }
        tracing::info!(target: "remote", channel = %self.short, tool = name, outcome = label, ms = duration_ms, "agent tool call");
        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(e) => e.to_result(name),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::services::{BrowserRequest, BrowserResponse, BrowserSender};
    use nevoflux_protocol::{BrowserToolAction, BrowserToolError};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    pub(super) const CHANNEL: &str = "2f1c4a90-7b3e-4d1a-9c58-0e6a2b7d4f31";
    pub(super) const SIDEBAR: &str = "proxy-sidebar-1";

    /// A desktop browser: new tabs are numbered from 100, list_tabs shows the
    /// person's tab 1 and tab 100, a closed tab answers "Tab not found", and a
    /// silent browser never answers.
    #[derive(Default)]
    pub(super) struct Browser {
        pub(super) seen: Mutex<Vec<BrowserRequest>>,
        pub(super) closed: Mutex<HashSet<i64>>,
        pub(super) silent: AtomicBool,
        /// Run once, when the next request arrives, before it is answered.
        pub(super) on_request: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    pub(super) fn browser() -> (BrowserSender, Arc<Browser>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(
            BrowserRequest,
            tokio::sync::oneshot::Sender<BrowserResponse>,
        )>(16);
        let state = Arc::new(Browser::default());
        let st = state.clone();
        tokio::spawn(async move {
            let mut next_tab = 100i64;
            let mut parked = Vec::new();
            while let Some((req, reply)) = rx.recv().await {
                st.seen.lock().unwrap().push(req.clone());
                let hook = st.on_request.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
                if st.silent.load(Ordering::SeqCst) {
                    parked.push(reply);
                    continue;
                }
                let gone = req
                    .tab_id
                    .is_some_and(|t| st.closed.lock().unwrap().contains(&t));
                let response = if gone {
                    BrowserResponse {
                        request_id: req.request_id.clone(),
                        success: false,
                        result: None,
                        error: Some(BrowserToolError {
                            code: 3001,
                            message: "Tab not found".into(),
                            recoverable: false,
                        }),
                    }
                } else {
                    let result = match req.action {
                        BrowserToolAction::Navigate if req.params["new_tab"] == json!(true) => {
                            let id = next_tab;
                            next_tab += 1;
                            json!({ "url": req.params["url"], "tab_id": id, "new_tab": true })
                        }
                        BrowserToolAction::Navigate => {
                            json!({ "url": req.params["url"], "tab_id": req.tab_id, "new_tab": false })
                        }
                        BrowserToolAction::ListTabs => json!({ "tabs": [
                            { "id": 1, "url": "https://mine.example/", "active": true },
                            { "id": 100, "url": "https://muse.example/", "active": false }
                        ]}),
                        _ => json!({ "ok": true }),
                    };
                    BrowserResponse {
                        request_id: req.request_id.clone(),
                        success: true,
                        result: Some(result),
                        error: None,
                    }
                };
                let _ = reply.send(response);
            }
        });
        (tx, state)
    }

    pub(super) struct FixedTier(pub(super) Mutex<ExecutionTier>);
    impl TierSource for FixedTier {
        fn tier(&self) -> ExecutionTier {
            *self.0.lock().unwrap()
        }
    }

    pub(super) struct FixedJudge(pub(super) Judgement);
    #[async_trait]
    impl Judge for FixedJudge {
        async fn judge(&self, _: &str, _: &Value, _: &HostServices) -> Judgement {
            self.0.clone()
        }
    }

    pub(super) struct Scripted {
        pub(super) answer: Approval,
        pub(super) asked: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Approver for Scripted {
        async fn ask(&self, message: String) -> Approval {
            self.asked.lock().unwrap().push(message);
            self.answer
        }
    }
    pub(super) fn approver(answer: Approval) -> Scripted {
        Scripted {
            answer,
            asked: Mutex::new(Vec::new()),
        }
    }

    pub(super) struct Rig {
        pub(super) backend: DesktopBrowserBackend,
        pub(super) browser: Arc<Browser>,
        pub(super) tabs: Arc<AgentTabs>,
        pub(super) tier: Arc<FixedTier>,
        pub(super) target: Arc<Mutex<Option<String>>>,
        pub(super) db: Arc<nevoflux_storage::Database>,
    }

    pub(super) fn rig_with(
        tier: ExecutionTier,
        judgement: Judgement,
        template: impl Fn(HostServices) -> HostServices,
    ) -> Rig {
        let (tx, browser) = browser();
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = template(HostServices::new(db.clone()).with_browser_sender(tx));
        let tabs = Arc::new(AgentTabs::new());
        let tier = Arc::new(FixedTier(Mutex::new(tier)));
        let target = Arc::new(Mutex::new(Some(SIDEBAR.to_string())));
        let t = target.clone();
        let backend = DesktopBrowserBackend::new(
            CHANNEL,
            tier.clone(),
            tabs.clone(),
            Arc::new(FixedJudge(judgement)),
            Arc::new(move || Some(services.clone())),
            Arc::new(move |_sender: &str| t.lock().unwrap().clone()),
        );
        Rig {
            backend,
            browser,
            tabs,
            tier,
            target,
            db,
        }
    }

    pub(super) fn rig(tier: ExecutionTier) -> Rig {
        rig_with(tier, Judgement::Allow, |s| s)
    }

    pub(super) async fn call(r: &Rig, tool: &str, args: Value) -> Value {
        let a = approver(Approval::Approved);
        let out = r
            .backend
            .call(tool, args, &a)
            .await
            .expect("never a protocol error");
        serde_json::to_value(&out).unwrap()
    }

    pub(super) fn error(v: &Value) -> Value {
        v["structuredContent"]["error"].clone()
    }

    pub(super) fn text(v: &Value) -> Value {
        serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap_or(Value::Null)
    }

    pub(super) fn sent(r: &Rig) -> Vec<BrowserRequest> {
        r.browser.seen.lock().unwrap().clone()
    }

    #[test]
    fn the_short_id_is_eight_alphanumerics_of_the_channel() {
        assert_eq!(short_id(CHANNEL), "2f1c4a90");
        assert_eq!(short_id("ab-cd"), "abcd");
    }

    #[test]
    fn tab_planning_follows_the_rules() {
        let nav = json!({ "url": "https://x.example/" });
        assert_eq!(
            plan_tab("browser_navigate", &nav, None),
            Ok(TabPlan::OpenNew)
        );
        assert_eq!(
            plan_tab("browser_navigate", &nav, Some(100)),
            Ok(TabPlan::Owned(100))
        );
        assert_eq!(
            plan_tab(
                "browser_navigate",
                &json!({ "url": "u", "new_tab": true, "tab_id": 1 }),
                Some(100)
            ),
            Ok(TabPlan::OpenNew)
        );
        assert_eq!(
            plan_tab("browser_snapshot", &json!({}), Some(100)),
            Ok(TabPlan::Owned(100))
        );
        assert_eq!(
            plan_tab("browser_snapshot", &json!({}), None),
            Err(AgentError::NoAgentTab)
        );
        assert_eq!(
            plan_tab("browser_click", &json!({ "tab_id": 1 }), None),
            Ok(TabPlan::AsGiven)
        );
        assert_eq!(
            plan_tab("browser_get_tabs", &json!({}), None),
            Ok(TabPlan::AsGiven)
        );
        assert!(matches!(
            plan_tab("browser_activate_tab", &json!({}), Some(100)),
            Err(AgentError::ToolFailed { .. })
        ));
    }

    #[test]
    fn placement_and_workspace_keys_are_never_the_agents() {
        let mut args =
            json!({ "url": "u", "background": false, "window": "main", "workspace_dir": "C:/" });
        apply_plan(TabPlan::Owned(100), &mut args);
        assert_eq!(args, json!({ "url": "u", "tab_id": 100 }));
        let mut args = json!({ "url": "u", "tab_id": 1, "background": false });
        apply_plan(TabPlan::OpenNew, &mut args);
        assert_eq!(
            args,
            json!({ "url": "u", "new_tab": true, "background": true })
        );
    }

    #[test]
    fn only_a_missing_tab_counts_as_closed() {
        assert!(is_tab_gone("3001: Tab not found"));
        assert!(is_tab_gone("-1: Tab not found"));
        assert!(is_tab_gone("-1: Invalid tab ID: 42"));
        assert!(!is_tab_gone("-1: Element not found"));
        assert!(!is_tab_gone(
            "-1: agent request 'click' has no tab_id; refusing to act on the active tab"
        ));
    }

    #[tokio::test]
    async fn read_only_refuses_a_click_and_asks_nobody() {
        let r = rig(ExecutionTier::ReadOnly);
        let a = approver(Approval::Approved);
        let out = serde_json::to_value(
            r.backend
                .call(
                    "browser_click",
                    json!({ "selector": "#go", "tab_id": 1 }),
                    &a,
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["isError"], true);
        let e = error(&out);
        assert_eq!(e["code"], "tier_denied");
        assert_eq!(e["tier"], "read-only");
        assert_eq!(e["required"], "browser-auto");
        assert!(a.asked.lock().unwrap().is_empty());
        assert!(sent(&r).is_empty());
    }

    #[tokio::test]
    async fn uploading_needs_full_auto() {
        let r = rig(ExecutionTier::BrowserAutoLocalRead);
        let out = call(
            &r,
            "browser_upload_file",
            json!({ "selector": "#f", "file_path": "x", "tab_id": 1 }),
        )
        .await;
        assert_eq!(error(&out)["required"], "full-auto");
        assert!(sent(&r).is_empty());
    }

    #[tokio::test]
    async fn activating_a_tab_needs_browser_auto() {
        let r = rig(ExecutionTier::ReadOnly);
        assert_eq!(
            error(&call(&r, "browser_activate_tab", json!({ "tab_id": 1 })).await)["code"],
            "tier_denied"
        );
        *r.tier.0.lock().unwrap() = ExecutionTier::BrowserAuto;
        let out = call(&r, "browser_activate_tab", json!({ "tab_id": 1 })).await;
        assert_eq!(out["isError"], false);
        assert_eq!(sent(&r)[0].action, BrowserToolAction::ActivateTab);
        assert_eq!(sent(&r)[0].tab_id, Some(1));
    }

    #[tokio::test]
    async fn no_browser_means_browser_unavailable_and_nothing_is_sent() {
        let r = rig(ExecutionTier::FullAuto);
        *r.target.lock().unwrap() = None;
        let out = call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        assert_eq!(error(&out)["code"], "browser_unavailable");
        assert!(sent(&r).is_empty());
    }

    #[tokio::test]
    async fn no_services_yet_means_browser_unavailable() {
        let backend = DesktopBrowserBackend::new(
            CHANNEL,
            Arc::new(FixedTier(Mutex::new(ExecutionTier::FullAuto))),
            Arc::new(AgentTabs::new()),
            Arc::new(FixedJudge(Judgement::Allow)),
            Arc::new(|| None::<HostServices>),
            Arc::new(|_: &str| Some(SIDEBAR.to_string())),
        );
        let out = serde_json::to_value(
            backend
                .call(
                    "browser_snapshot",
                    json!({ "tab_id": 1 }),
                    &approver(Approval::Approved),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(error(&out)["code"], "browser_unavailable");
    }

    #[tokio::test]
    async fn the_first_navigate_opens_a_background_tab_that_becomes_the_agents() {
        let r = rig(ExecutionTier::ReadOnly);
        let out = call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        assert_eq!(text(&out)["tab_id"], 100);
        let first = sent(&r)[0].clone();
        assert_eq!(first.tab_id, None);
        assert_eq!(first.params["new_tab"], true);
        assert_eq!(first.params["background"], true);
        call(&r, "browser_snapshot", json!({})).await;
        assert_eq!(sent(&r)[1].tab_id, Some(100));
    }

    #[tokio::test]
    async fn a_second_navigate_reuses_the_agents_tab() {
        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/2" }),
        )
        .await;
        let second = sent(&r)[1].clone();
        assert_eq!(second.tab_id, Some(100));
        assert!(second.params.get("new_tab").is_none());
        assert!(second.params.get("background").is_none());
        assert_eq!(
            r.tabs.url_of(CHANNEL, 100).as_deref(),
            Some("https://muse.example/2")
        );
    }

    #[tokio::test]
    async fn new_tab_opens_another_and_moves_the_primary() {
        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://a.example/" }),
        )
        .await;
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://b.example/", "new_tab": true }),
        )
        .await;
        call(&r, "browser_get_markdown", json!({})).await;
        assert_eq!(sent(&r)[2].tab_id, Some(101));
    }

    #[tokio::test]
    async fn new_tab_with_a_tab_id_and_background_false_still_opens_a_background_tab() {
        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/", "new_tab": true, "tab_id": 1, "background": false }),
        )
        .await;
        let first = sent(&r)[0].clone();
        assert_eq!(first.tab_id, None, "the person's tab 1 is not touched");
        assert_eq!(first.params["background"], true);
    }

    #[tokio::test]
    async fn without_a_tab_of_its_own_the_agent_is_told_to_navigate() {
        let r = rig(ExecutionTier::BrowserAuto);
        for tool in [
            "browser_snapshot",
            "browser_click_by_id",
            "browser_wait_for",
        ] {
            let out = call(&r, tool, json!({ "element_id": "e1" })).await;
            assert_eq!(error(&out)["code"], "no_agent_tab", "{tool}");
        }
        assert!(sent(&r).is_empty());
    }

    #[tokio::test]
    async fn a_named_tab_is_used_as_given_and_not_adopted() {
        let r = rig(ExecutionTier::BrowserAuto);
        call(
            &r,
            "browser_click",
            json!({ "selector": "#a", "tab_id": 1 }),
        )
        .await;
        assert_eq!(sent(&r)[0].tab_id, Some(1));
        assert!(r.tabs.owned_ids(CHANNEL, SIDEBAR).is_empty());
    }

    #[tokio::test]
    async fn get_tabs_marks_which_tabs_are_the_agents() {
        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        let out = call(&r, "browser_get_tabs", json!({})).await;
        assert_eq!(sent(&r)[1].tab_id, None);
        let tabs = text(&out)["tabs"].clone();
        assert_eq!(tabs[0]["owned"], false);
        assert_eq!(tabs[1]["owned"], true);
    }

    #[tokio::test]
    async fn a_closed_tab_is_forgotten_and_reported() {
        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        r.browser.closed.lock().unwrap().insert(100);
        let out = call(&r, "browser_snapshot", json!({})).await;
        assert_eq!(error(&out)["code"], "tab_closed");
        assert_eq!(error(&out)["tab_id"], 100);
        assert_eq!(
            error(&call(&r, "browser_snapshot", json!({})).await)["code"],
            "no_agent_tab"
        );
    }

    #[tokio::test]
    async fn a_refusal_from_the_extension_is_a_tool_failure() {
        let r = rig(ExecutionTier::BrowserAuto);
        let err = r.backend.after_failure(
            &json!({ "tab_id": 1 }),
            "-1: agent request 'click' has no tab_id; refusing to act on the active tab".into(),
        );
        assert_eq!(err.code(), "tool_failed");
    }

    #[tokio::test]
    async fn a_browser_reconnect_forgets_every_agent_tab() {
        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        r.tabs.connection_closed(SIDEBAR);
        assert_eq!(
            error(&call(&r, "browser_snapshot", json!({})).await)["code"],
            "no_agent_tab"
        );

        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        *r.target.lock().unwrap() = Some("proxy-sidebar-2".into());
        assert_eq!(
            error(&call(&r, "browser_snapshot", json!({})).await)["code"],
            "no_agent_tab"
        );
    }

    #[tokio::test]
    async fn an_agent_call_never_runs_as_an_iteration_and_says_who_it_is() {
        let r = rig_with(ExecutionTier::ReadOnly, Judgement::Allow, |mut s| {
            s.is_iteration = true;
            s.iteration_loop_id = Some("loop-1".into());
            s.tool_origin = "model".into();
            s.with_session_id("someone-elses".into())
        });
        let s = r.backend.call_services().unwrap();
        assert!(!s.is_iteration, "the native gate auto-approves iterations");
        assert_eq!(s.iteration_loop_id, None);
        assert_eq!(s.proxy_id, "agent:2f1c4a90");
        assert_eq!(s.session_id, "agent-2f1c4a90");
        assert_eq!(s.tool_origin, "mcp:agent");
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        let first = sent(&r)[0].clone();
        assert_eq!(first.proxy_id, "agent:2f1c4a90");
        assert_eq!(first.session_id, "agent-2f1c4a90");
        assert_eq!(first.origin.as_deref(), Some("agent"));
    }

    #[tokio::test]
    async fn the_log_names_the_tool_and_outcome_but_never_the_arguments() {
        let r = rig(ExecutionTier::BrowserAuto);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/secret-path" }),
        )
        .await;
        call(
            &r,
            "browser_fill_by_id",
            json!({ "element_id": "e1", "value": "hunter2" }),
        )
        .await;
        *r.tier.0.lock().unwrap() = ExecutionTier::ReadOnly;
        call(&r, "browser_click", json!({ "selector": "#x" })).await;
        let events: Vec<Value> = nevoflux_storage::repositories::SessionEventRepository::new(&r.db)
            .list("agent-2f1c4a90")
            .unwrap()
            .into_iter()
            .map(|e| serde_json::to_value(&e).unwrap())
            .collect();
        let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "tool/call",
                "tool/result",
                "tool/call",
                "tool/result",
                "tool/call",
                "tool/result"
            ]
        );
        for call in events.iter().filter(|e| e["type"] == "tool/call") {
            assert_eq!(call["args"], json!({}));
            assert_eq!(call["origin"], "mcp:agent");
        }
        let results: Vec<(&str, bool)> = events
            .iter()
            .filter(|e| e["type"] == "tool/result")
            .map(|e| {
                (
                    e["content"].as_str().unwrap(),
                    e["is_error"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            results,
            vec![("ok", false), ("ok", false), ("tier_denied", true)]
        );
        let all = serde_json::to_string(&events).unwrap();
        assert!(!all.contains("hunter2") && !all.contains("secret-path"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_browser_that_never_answers_fails_the_call_instead_of_hanging() {
        let r = rig(ExecutionTier::ReadOnly);
        r.browser.silent.store(true, Ordering::SeqCst);
        let out = call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        assert_eq!(error(&out)["code"], "tool_failed");
        assert!(error(&out)["detail"]
            .as_str()
            .unwrap()
            .contains("did not answer"));
    }

    #[test]
    fn an_unreadable_store_reads_as_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-pairings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let tier = StoredTier::new(Arc::new(PairingStore::new(&path)), CHANNEL);
        assert_eq!(tier.tier(), ExecutionTier::ReadOnly);
    }

    #[test]
    fn a_tier_change_applies_to_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(PairingStore::new(dir.path().join("agent-pairings.json")));
        let mut p = crate::remote::pairing::mint_agent("A-BCDE-FGHJ-KMNP").unwrap();
        p.control_channel_id = CHANNEL.into();
        store.add(p).unwrap();
        let tier = StoredTier::new(store.clone(), CHANNEL);
        assert_eq!(tier.tier(), ExecutionTier::ReadOnly);
        store.set_tier(CHANNEL, ExecutionTier::BrowserAuto).unwrap();
        assert_eq!(tier.tier(), ExecutionTier::BrowserAuto);
        assert_eq!(
            StoredTier::new(store, "someone-else").tier(),
            ExecutionTier::ReadOnly
        );
    }

    fn asks() -> Judgement {
        Judgement::Ask {
            prompts: vec![AskPrompt {
                stage: "site_policy",
                prompt: "this site asks first".into(),
            }],
            arguments: None,
        }
    }

    /// Approves, but moves the agent's calls to another browser (or none)
    /// while the person is being asked.
    struct SwitchesBrowser {
        target: Arc<Mutex<Option<String>>>,
        to: Option<String>,
    }
    #[async_trait]
    impl Approver for SwitchesBrowser {
        async fn ask(&self, _message: String) -> Approval {
            *self.target.lock().unwrap() = self.to.clone();
            Approval::Approved
        }
    }

    #[tokio::test]
    async fn a_truthy_new_tab_that_is_not_a_bool_still_opens_a_background_tab() {
        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://a.example/" }),
        )
        .await;
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://b.example/", "new_tab": "true" }),
        )
        .await;
        let second = sent(&r)[1].clone();
        assert_eq!(second.tab_id, None, "the agent's tab 100 is not reused");
        assert_eq!(second.params["new_tab"], json!(true));
        assert_eq!(second.params["background"], json!(true));
        call(&r, "browser_get_markdown", json!({})).await;
        assert_eq!(sent(&r)[2].tab_id, Some(101), "the new tab is the agent's");

        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/", "new_tab": 1, "tab_id": 1 }),
        )
        .await;
        let first = sent(&r)[0].clone();
        assert_eq!(first.tab_id, None, "the person's tab 1 is not touched");
        assert_eq!(first.params["new_tab"], json!(true));
        assert_eq!(first.params["background"], json!(true));
    }

    #[tokio::test]
    async fn a_falsy_new_tab_is_never_forwarded() {
        let r = rig(ExecutionTier::ReadOnly);
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://a.example/" }),
        )
        .await;
        for falsy in [json!(false), json!("false"), json!(0), json!("")] {
            call(
                &r,
                "browser_navigate",
                json!({ "url": "https://b.example/", "new_tab": falsy }),
            )
            .await;
        }
        for req in &sent(&r)[1..] {
            assert_eq!(req.tab_id, Some(100));
            assert!(req.params.get("new_tab").is_none(), "{:?}", req.params);
        }
    }

    #[tokio::test]
    async fn a_tab_id_that_is_not_a_positive_integer_is_refused() {
        let r = rig(ExecutionTier::BrowserAuto);
        for bad in [json!(0), json!(-3), json!(1.5), json!("1")] {
            let out = call(
                &r,
                "browser_click",
                json!({ "selector": "#a", "tab_id": bad }),
            )
            .await;
            assert_eq!(error(&out)["code"], "tool_failed", "{bad}");
        }
        let out = call(&r, "browser_navigate", json!({ "url": "u", "tab_id": 0 })).await;
        assert_eq!(error(&out)["code"], "tool_failed");
        assert!(sent(&r).is_empty());
    }

    #[tokio::test]
    async fn a_rule_rewrite_cannot_place_the_tab_or_pick_the_workspace() {
        let injected = json!({
            "url": "https://muse.example/",
            "workspace_dir": "C:/",
            "background": false,
            "window": "main",
            "new_tab": "yes"
        });
        // Rewrite, on a navigate that opens a new tab.
        let r = rig_with(
            ExecutionTier::ReadOnly,
            Judgement::Rewrite {
                arguments: injected.clone(),
            },
            |s| s,
        );
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        let first = sent(&r)[0].clone();
        assert_eq!(first.tab_id, None);
        assert_eq!(
            first.params,
            json!({ "url": "https://muse.example/", "new_tab": true, "background": true })
        );

        // Ask, on a navigate in the agent's own tab: the rule may name the tab.
        let mut asked = injected.clone();
        asked["tab_id"] = json!(100);
        let r = rig_with(
            ExecutionTier::ReadOnly,
            Judgement::Ask {
                prompts: vec![AskPrompt {
                    stage: "pack_hook",
                    prompt: "why".into(),
                }],
                arguments: Some(asked),
            },
            |s| s,
        );
        r.tabs.record_opened(CHANNEL, SIDEBAR, 100, None);
        let a = approver(Approval::Approved);
        r.backend
            .call(
                "browser_navigate",
                json!({ "url": "https://muse.example/" }),
                &a,
            )
            .await
            .unwrap();
        let first = sent(&r)[0].clone();
        assert_eq!(first.tab_id, Some(100));
        assert_eq!(first.params, json!({ "url": "https://muse.example/" }));
        let question = a.asked.lock().unwrap()[0].clone();
        assert!(
            !question.contains("workspace_dir") && question.contains("tab 100"),
            "{question}"
        );

        // A rewrite to a falsy tab id would act on the active tab.
        let r = rig_with(
            ExecutionTier::ReadOnly,
            Judgement::Rewrite {
                arguments: json!({ "tab_id": 0 }),
            },
            |s| s,
        );
        r.tabs.record_opened(CHANNEL, SIDEBAR, 100, None);
        let out = call(&r, "browser_snapshot", json!({})).await;
        assert_eq!(error(&out)["code"], "tool_failed");
        assert!(sent(&r).is_empty());
    }

    #[tokio::test]
    async fn a_browser_switch_during_the_question_sends_nothing() {
        // The agent's own tab: its number belongs to the old browser.
        let r = rig_with(ExecutionTier::ReadOnly, asks(), |s| s);
        r.tabs.record_opened(CHANNEL, SIDEBAR, 100, None);
        let a = SwitchesBrowser {
            target: r.target.clone(),
            to: Some("proxy-sidebar-2".into()),
        };
        let out = serde_json::to_value(
            r.backend
                .call("browser_snapshot", json!({}), &a)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(error(&out)["code"], "tab_closed");
        assert_eq!(error(&out)["tab_id"], 100);
        assert!(sent(&r).is_empty());
        assert!(
            r.tabs.owned_ids(CHANNEL, SIDEBAR).is_empty(),
            "the stale table is forgotten"
        );

        // A new tab, and the browser went away.
        let r = rig_with(ExecutionTier::ReadOnly, asks(), |s| s);
        let a = SwitchesBrowser {
            target: r.target.clone(),
            to: None,
        };
        let out = serde_json::to_value(
            r.backend
                .call(
                    "browser_navigate",
                    json!({ "url": "https://muse.example/" }),
                    &a,
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(error(&out)["code"], "browser_unavailable");
        assert!(sent(&r).is_empty());
    }

    #[tokio::test]
    async fn a_tab_opened_while_the_browser_changed_is_not_adopted() {
        let r = rig(ExecutionTier::ReadOnly);
        let t = r.target.clone();
        *r.browser.on_request.lock().unwrap() = Some(Box::new(move || {
            *t.lock().unwrap() = Some("proxy-sidebar-2".into());
        }));
        let out = call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        assert_eq!(out["isError"], false);
        assert!(r.tabs.owned_ids(CHANNEL, SIDEBAR).is_empty());
        assert!(r.tabs.owned_ids(CHANNEL, "proxy-sidebar-2").is_empty());
    }

    fn ask(prompt: &str) -> Judgement {
        Judgement::Ask {
            prompts: vec![AskPrompt {
                stage: "site_policy",
                prompt: prompt.into(),
            }],
            arguments: None,
        }
    }

    async fn call_with(r: &Rig, a: &Scripted, tool: &str, args: Value) -> Value {
        serde_json::to_value(&r.backend.call(tool, args, a).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn an_approved_question_runs_and_the_person_saw_what_and_where() {
        let r = rig_with(
            ExecutionTier::ReadOnly,
            ask("ask.example wants a person to confirm"),
            |s| s,
        );
        let a = approver(Approval::Approved);
        let out = call_with(
            &r,
            &a,
            "browser_navigate",
            json!({ "url": "https://muse.example/" }),
        )
        .await;
        assert_eq!(out["isError"], false);
        let asked = a.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].contains("Navigate to: https://muse.example/"),
            "{}",
            asked[0]
        );
        assert!(
            asked[0].contains("Tab: a new tab of the agent's"),
            "{}",
            asked[0]
        );
        assert!(
            asked[0].contains("ask.example wants a person to confirm"),
            "{}",
            asked[0]
        );
        assert_eq!(sent(&r).len(), 1);
    }

    #[tokio::test]
    async fn every_answer_but_yes_stops_the_call() {
        for (answer, code, reason) in [
            (Approval::Declined, "approval_denied", Some("declined")),
            (
                Approval::Unsupported,
                "approval_denied",
                Some("client_cannot_approve"),
            ),
            (Approval::Failed, "approval_denied", Some("approval_failed")),
            (Approval::TimedOut, "approval_timeout", None),
        ] {
            let r = rig_with(ExecutionTier::BrowserAuto, ask("why"), |s| s);
            let a = approver(answer);
            let out = call_with(
                &r,
                &a,
                "browser_click",
                json!({ "selector": "#go", "tab_id": 1 }),
            )
            .await;
            assert_eq!(error(&out)["code"], code, "{answer:?}");
            assert_eq!(error(&out)["reason"].as_str(), reason, "{answer:?}");
            assert!(sent(&r).is_empty(), "{answer:?}: nothing ran");
        }
    }

    #[tokio::test]
    async fn a_rule_that_refuses_is_final_and_nobody_is_asked() {
        let denial = nevoflux_builtin_wasm::ToolDenial {
            code: "POLICY_DENIED".into(),
            message: "no automation on banking sites".into(),
            rule: None,
            pack: Some("bank-guard".into()),
        };
        let r = rig_with(ExecutionTier::BrowserAuto, Judgement::Deny(denial), |s| s);
        let a = approver(Approval::Approved);
        let out = call_with(
            &r,
            &a,
            "browser_click",
            json!({ "selector": "#pay", "tab_id": 1 }),
        )
        .await;
        assert_eq!(error(&out)["code"], "approval_denied");
        assert_eq!(error(&out)["reason"], "rule");
        assert!(error(&out)["message"]
            .as_str()
            .unwrap()
            .contains("no automation on banking sites"));
        assert!(a.asked.lock().unwrap().is_empty());
        assert!(sent(&r).is_empty());
    }

    #[tokio::test]
    async fn the_tier_is_checked_before_any_rule() {
        let r = rig_with(ExecutionTier::ReadOnly, ask("why"), |s| s);
        let a = approver(Approval::Approved);
        let out = call_with(
            &r,
            &a,
            "browser_click",
            json!({ "selector": "#go", "tab_id": 1 }),
        )
        .await;
        assert_eq!(error(&out)["code"], "tier_denied");
        assert!(a.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn what_runs_after_an_approval_is_what_was_judged() {
        // The agent names tab 1; the rule moves the call to tab 5, the
        // agent's own, and to another URL. The person is asked about the
        // rule's version, and that is the version that runs.
        let judged = json!({ "url": "https://safe.example/", "tab_id": 5 });
        let r = rig_with(
            ExecutionTier::ReadOnly,
            Judgement::Ask {
                prompts: vec![AskPrompt {
                    stage: "site_policy",
                    prompt: "rewritten".into(),
                }],
                arguments: Some(judged.clone()),
            },
            |s| s,
        );
        r.tabs
            .record_opened(CHANNEL, SIDEBAR, 5, Some("https://safe.example/".into()));
        let a = approver(Approval::Approved);
        let out = call_with(
            &r,
            &a,
            "browser_navigate",
            json!({ "url": "https://muse.example/", "tab_id": 1 }),
        )
        .await;
        assert_eq!(out["isError"], false);
        assert_eq!(sent(&r).len(), 1);
        assert_eq!(sent(&r)[0].params["url"], "https://safe.example/");
        assert_eq!(sent(&r)[0].tab_id, Some(5));
        let question = a.asked.lock().unwrap()[0].clone();
        assert!(
            question.contains("Navigate to: https://safe.example/"),
            "{question}"
        );
        assert!(question.contains("tab 5"), "{question}");
        assert!(!question.contains("muse.example"), "{question}");

        let r = rig_with(
            ExecutionTier::ReadOnly,
            Judgement::Rewrite {
                arguments: json!({ "url": "https://rewritten.example/", "tab_id": 1 }),
            },
            |s| s,
        );
        call(
            &r,
            "browser_navigate",
            json!({ "url": "https://muse.example/", "tab_id": 1 }),
        )
        .await;
        assert_eq!(sent(&r)[0].params["url"], "https://rewritten.example/");
        assert_eq!(sent(&r)[0].tab_id, Some(1));
    }

    #[tokio::test]
    async fn the_installed_judge_asks_what_an_active_site_rule_asks() {
        let dir = tempfile::tempdir().unwrap();
        let pd = dir.path().join("ask-guard");
        std::fs::create_dir_all(&pd).unwrap();
        std::fs::write(
            pd.join("pack.toml"),
            "[pack]\nname = \"ask-guard\"\nversion = \"1.0.0\"\nprotocol = \"pack-protocol/0.2\"\nmin_nevoflux = \"0.3.0\"\n\n[[components.tool_policy]]\nscope = \"active\"\nmatch = { url = [\"*://ask.example/*\"] }\ndeny = []\nask = [\"browser_navigate\"]\nmessage = \"ask.example wants a person to confirm\"\n",
        )
        .unwrap();
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db);
        services
            .active_packs
            .write()
            .unwrap()
            .insert("ask-guard".into());
        let judge = PipelineJudge::at(dir.path().to_path_buf());
        assert!(matches!(
            judge.judge("browser_navigate", &json!({ "url": "https://ask.example/x" }), &services).await,
            Judgement::Ask { ref prompts, .. } if prompts[0].prompt.contains("ask-guard")
        ));
        assert!(matches!(
            judge
                .judge(
                    "browser_navigate",
                    &json!({ "url": "https://other.example/" }),
                    &services
                )
                .await,
            Judgement::Allow
        ));
    }

    fn jev_cfg(url: &str) -> crate::config::AgentConfig {
        let mut c = crate::config::AgentConfig::default();
        c.llm.provider = Some("anthropic".into());
        c.jev.enabled = true;
        c.jev.endpoint = url.to_string();
        c.jev.api_key = "k".into();
        c.jev.timeout_ms = 2000;
        c.jev.points.permissions = true;
        c
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_jev_flags_becomes_a_question_and_is_logged() {
        let (url, bodies) = crate::jev::test_support::answering(
            json!({ "answers": { "risk": { "noul": 0.9 } }, "usage": { "input_tokens": 1, "output_tokens": 1 } }),
            Duration::ZERO,
        )
        .await;
        let db = Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let services = HostServices::new(db.clone())
            .with_agent_config(Arc::new(jev_cfg(&url)))
            .with_session_id("agent-2f1c4a90".into());
        let judge = PipelineJudge::at(PathBuf::from("this-directory-does-not-exist"));

        match judge
            .judge("browser_click", &json!({ "selector": "#pay" }), &services)
            .await
        {
            Judgement::Ask { prompts, .. } => assert_eq!(prompts[0].stage, "jev"),
            other => panic!("expected an ask, got {other:?}"),
        }
        let flagged = nevoflux_storage::repositories::SessionEventRepository::new(&db)
            .list("agent-2f1c4a90")
            .unwrap()
            .into_iter()
            .filter(|e| {
                matches!(
                    e.payload,
                    SessionEventPayload::PermissionJev {
                        unattended: false,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(flagged, 1);

        // A read-only tool is never put to Jev.
        let before = bodies.lock().unwrap().len();
        assert!(matches!(
            judge
                .judge("browser_snapshot", &json!({ "tab_id": 1 }), &services)
                .await,
            Judgement::Allow
        ));
        assert_eq!(bodies.lock().unwrap().len(), before);
    }

    #[test]
    fn a_pairing_gets_its_own_short_id_and_the_shared_tab_table() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(PairingStore::new(dir.path().join("agent-pairings.json")));
        let mut p = crate::remote::pairing::mint_agent("A-BCDE-FGHJ-KMNP").unwrap();
        p.control_channel_id = CHANNEL.into();
        let backend = DesktopBrowserBackend::for_pairing(&p, store);
        assert_eq!(backend.short, "2f1c4a90");
        assert!(Arc::ptr_eq(&backend.tabs, &crate::remote::agent_tabs::shared()));
        assert_eq!(backend.tools().len(), 16);
    }

    use crate::remote::channel_codec;
    use crate::remote::mcp_gateway::McpGateway;
    use crate::remote::portal_gateway::WireSink;
    use crate::remote::relay_protocol::WireMessage;
    use crate::remote::session::Wire;

    const KEY: [u8; 32] = [5u8; 32];

    fn fixture() -> Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/muse/tool_errors.json"
        )))
        .unwrap()
    }

    #[derive(Default)]
    struct Collect {
        wires: Mutex<Vec<Wire>>,
    }

    #[async_trait]
    impl WireSink for Collect {
        async fn send(&self, wire: Wire) {
            self.wires.lock().unwrap().push(wire);
        }
    }

    impl Collect {
        fn frames(&self) -> Vec<Value> {
            self.wires
                .lock()
                .unwrap()
                .iter()
                .filter_map(|w| match channel_codec::decode(Some(&KEY), w) {
                    Some(WireMessage::Frame { frame, .. }) => Some(frame),
                    _ => None,
                })
                .collect()
        }

        async fn message(&self, pred: impl Fn(&Value) -> bool) -> Value {
            for _ in 0..250 {
                if let Some(f) = self.frames().into_iter().find(|f| pred(&f["m"])) {
                    return f["m"].clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("no such message from the head");
        }
    }

    struct Agent {
        gw: McpGateway,
        sink: Arc<Collect>,
        ch: String,
        n: u64,
    }

    impl Agent {
        /// Presence -> challenge -> initialize (with `capabilities`) -> initialized.
        async fn connect(backend: DesktopBrowserBackend, capabilities: Value) -> Self {
            let sink = Arc::new(Collect::default());
            let gw = McpGateway::new(KEY, sink.clone(), Arc::new(backend), CHANNEL);
            gw.on_wire_in(&Wire::Text(r#"{"k":"peers","n":1}"#.into())).await;
            let ch = sink.frames()[0]["ch"].as_str().unwrap().to_string();
            let mut agent = Agent { gw, sink, ch, n: 0 };
            agent
                .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-11-25", "capabilities": capabilities,
                    "clientInfo": {"name": "nevoflux-muse", "version": "0.3.0"}}}))
                .await;
            agent.sink.message(|m| m["id"] == json!(1)).await;
            agent.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
            agent
        }

        async fn send(&mut self, m: Value) {
            let frame = json!({"d": "c2h", "n": self.n, "ch": self.ch, "m": m});
            self.n += 1;
            let wire = channel_codec::encode(Some(&KEY), &WireMessage::Frame { seq: None, frame });
            self.gw.on_wire_in(&wire).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_call_end_to_end_over_a_sealed_channel() {
        let r = rig(ExecutionTier::ReadOnly);
        let browser = r.browser.clone();
        let mut agent = Agent::connect(r.backend, json!({})).await;

        agent.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})).await;
        let listed = agent.sink.message(|m| m["id"] == json!(2)).await;
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 16);

        agent
            .send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": "browser_navigate", "arguments": {"url": "https://muse.example/"}}}))
            .await;
        let nav = agent.sink.message(|m| m["id"] == json!(3)).await;
        assert_eq!(nav["result"]["isError"], json!(false));
        assert!(nav["result"]["content"][0]["text"].as_str().unwrap().contains("\"tab_id\":100"));
        let first = browser.seen.lock().unwrap()[0].clone();
        assert_eq!(first.origin.as_deref(), Some("agent"));
        assert_eq!(first.params["background"], json!(true));

        agent
            .send(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
                "name": "browser_click", "arguments": {"selector": "#buy"}}}))
            .await;
        let click = agent.sink.message(|m| m["id"] == json!(4)).await;
        let f = fixture();
        assert_eq!(click["result"]["isError"], json!(true));
        assert_eq!(
            click["result"]["structuredContent"]["error"]["code"],
            f["errors"]["tier_denied"]["structuredContent"]["error"]["code"]
        );
        assert_eq!(browser.seen.lock().unwrap().len(), 1, "the click never reached the browser");
    }

    /// The approval request on the wire is the fixture's, `_meta` included,
    /// and the fixture's answers settle the call both ways.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_approval_round_trip_matches_the_shared_fixture() {
        let f = fixture();
        for (answer, runs) in [(f["approve"].clone(), true), (f["deny"].clone(), false)] {
            let r = rig_with(
                ExecutionTier::BrowserAuto,
                Judgement::Ask {
                    prompts: vec![AskPrompt {
                        stage: "site_policy",
                        prompt: "ask-guard wants a person to confirm".into(),
                    }],
                    arguments: None,
                },
                |s| s,
            );
            let browser = r.browser.clone();
            let mut agent = Agent::connect(r.backend, json!({"elicitation": {}})).await;
            // `browser_click` with an `element_id`: the desktop dialog words it
            // "Click on element 'e1'", which is what the fixture's message says,
            // and the fixture's `approval_denied` example names `browser_click`.
            agent
                .send(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
                    "name": "browser_click",
                    "arguments": {"element_id": "e1", "tab_id": 1},
                    "_meta": {"io.nevoflux/call": "0badc0de"}}}))
                .await;
            let request = agent.sink.message(|m| m["method"] == "elicitation/create").await;
            assert_eq!(request["method"], f["approval_request"]["method"]);
            assert_eq!(request["params"], f["approval_request"]["params"]);
            agent
                .send(json!({"jsonrpc": "2.0", "id": request["id"].clone(), "result": answer}))
                .await;
            let result = agent.sink.message(|m| m["id"] == json!(5)).await;
            if runs {
                assert_eq!(result["result"]["isError"], json!(false));
                assert_eq!(browser.seen.lock().unwrap().len(), 1);
            } else {
                assert_eq!(
                    result["result"]["structuredContent"],
                    f["errors"]["approval_denied"]["structuredContent"]
                );
                assert!(browser.seen.lock().unwrap().is_empty());
            }
        }
    }
}
