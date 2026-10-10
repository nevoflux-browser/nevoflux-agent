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

use std::sync::Arc;
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

/// Decide the tab before anything runs.
pub fn plan_tab(
    tool: &str,
    arguments: &Value,
    primary: Option<i64>,
) -> Result<TabPlan, AgentError> {
    if tool == "browser_navigate" && arguments.get("new_tab").and_then(Value::as_bool) == Some(true)
    {
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
/// dropped from whatever the agent sent first.
pub fn apply_plan(plan: TabPlan, arguments: &mut Value) {
    let Some(args) = arguments.as_object_mut() else {
        return;
    };
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
            } => rewritten,
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
                let effective = rewritten.unwrap_or(arguments);
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
                    Approval::Unsupported => return Err(AgentError::ApprovalDenied {
                        reason: REASON_CLIENT_CANNOT_APPROVE,
                        message: format!(
                            "{name} needs the person's approval and this client cannot ask for it"
                        ),
                    }),
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
        // 5. The call, bounded.
        let call =
            crate::wasm::mcp_tool_executor::execute_agent_browser_tool(name, &arguments, &services);
        let outcome = match tokio::time::timeout(EXECUTE_TIMEOUT, call).await {
            Ok(outcome) => outcome,
            Err(_) => Err("the browser did not answer".to_string()),
        };
        match outcome {
            Ok(text) => Ok(self.after_success(name, plan, &arguments, &served_by, text)),
            Err(detail) => Err(self.after_failure(&arguments, detail)),
        }
    }

    fn after_success(
        &self,
        name: &str,
        plan: TabPlan,
        arguments: &Value,
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
                    (TabPlan::OpenNew, Some(id)) => {
                        self.tabs
                            .record_opened(&self.channel_id, served_by, id, url)
                    }
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
}
