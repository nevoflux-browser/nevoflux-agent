//! One delegated task, followed through the frames its turn sends (M3 §3,
//! contract §2).
//!
//! Pure: no clock, no I/O, no locks. The engine hands it every chat frame of
//! the task's session with the time and asks it for what the paired agent
//! (`wait_json`) and the desktop status bar (`status_json`) are shown, so all
//! of it is testable frame by frame — and the fixtures the Muse client pins
//! are produced by these same functions.

use std::collections::VecDeque;

use serde_json::{json, Value};

use super::limits::{
    APPROVAL_SUMMARY_MAX_CHARS, DELEGATED_HOLD, REPLY_MAX_CHARS, STATUS_SUMMARY_MAX_CHARS,
    STEPS_MAX,
};

/// Where a task stands (contract §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    AwaitingApproval,
    Done,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Running => "running",
            TaskState::AwaitingApproval => "awaiting_approval",
            TaskState::Done => "done",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Failed | TaskState::Cancelled
        )
    }
}

/// What an approval is (contract §2.1, R13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// A permission prompt: tier gate, Jev flag, site/pack rule, ACP request.
    Tool,
    /// The model's own `ask_user`.
    Question,
    /// A plan waiting to be confirmed.
    Plan,
}

impl ApprovalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalKind::Tool => "tool",
            ApprovalKind::Question => "question",
            ApprovalKind::Plan => "plan",
        }
    }
}

/// One question the task's turn is waiting on.
#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    /// The wire `request_id`, or a minted id for a plan (R7).
    pub approval_id: String,
    pub kind: ApprovalKind,
    pub summary: String,
    /// The model's question, for `Question` only.
    pub question: Option<String>,
    /// The buttons or choices offered (kept for every kind: R15 maps onto them).
    pub options: Vec<String>,
    pub plan_steps: Vec<String>,
    /// Raised by the must-approve stage (contract v1.2 §2.1). Whether the
    /// agent may answer it is not stored here: it is the pairing's toggle,
    /// read at each wait and each answer (R39).
    pub must_approve: bool,
    /// Unix seconds.
    pub at: i64,
}

/// One step event (R9): a tool call started, or ended `ok` / `failed`.
#[derive(Debug, Clone, PartialEq)]
pub struct StepEvent {
    pub seq: u64,
    pub id: String,
    pub at: i64,
    pub tool: String,
    pub status: &'static str,
}

/// What the result screenshot shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Artifact(String),
    Tab(i64),
}

/// What one frame did to the record.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub changed: bool,
    pub new_approval: bool,
    /// An approval was settled by its deadline passing (`reason: "timeout"`).
    pub timed_out: bool,
}

/// How a turn's end leaves the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conclusion {
    /// The task is over (failed now, or was already over).
    Ended,
    /// Take the result screenshot, then `finish`.
    Capture,
}

pub const APPROVAL_TIMEOUT_MESSAGE: &str =
    "nobody answered an approval within 30 minutes; it was denied";
pub const BROWSER_GONE_MESSAGE: &str = "the NevoFlux desktop browser disconnected";
pub const INTERRUPTED_MESSAGE: &str = "the task was interrupted";

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// One delegated task.
pub struct TaskRecord {
    pub task_id: String,
    pub pairing: String,
    pub session: String,
    /// The prompt's first line, for the status bar and the end notice.
    pub summary: String,
    /// The sidebar connection the turn was injected under.
    pub proxy_id: String,
    pub state: TaskState,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// Open approvals, oldest first.
    pub approvals: Vec<Approval>,
    pub reply: Option<String>,
    pub reply_truncated: bool,
    /// `(code, message)` of a failed task.
    pub error: Option<(String, String)>,
    /// The result screenshot, base64 JPEG.
    pub screenshot: Option<String>,
    pub screenshot_error: Option<String>,
    /// The newest tab or Canvas the turn opened or generated.
    pub produced: Option<Target>,
    /// The tab the turn last acted on.
    pub last_tab: Option<i64>,
    /// The message of an `error` frame the turn sent.
    pub saw_error: Option<String>,
    pub turn_started: bool,
    /// The turn has ended and the screenshot is being taken.
    pub finishing: bool,
    /// The end notice has gone out.
    pub notified: bool,
    events: VecDeque<StepEvent>,
    next_seq: u64,
    /// Tool calls started and not yet ended: `(tool_id, tool)`.
    open: Vec<(String, String)>,
    /// Assistant text since the last tool call started.
    reply_buf: String,
    mint: fn() -> String,
}

impl TaskRecord {
    pub fn new(
        task_id: &str,
        pairing: &str,
        session: &str,
        prompt: &str,
        proxy_id: &str,
        now: i64,
    ) -> Self {
        Self {
            task_id: task_id.to_string(),
            pairing: pairing.to_string(),
            session: session.to_string(),
            summary: summary_line(prompt),
            proxy_id: proxy_id.to_string(),
            state: TaskState::Running,
            started_at: now,
            ended_at: None,
            approvals: Vec::new(),
            reply: None,
            reply_truncated: false,
            error: None,
            screenshot: None,
            screenshot_error: None,
            produced: None,
            last_tab: None,
            saw_error: None,
            turn_started: false,
            finishing: false,
            notified: false,
            events: VecDeque::new(),
            next_seq: 0,
            open: Vec::new(),
            reply_buf: String::new(),
            mint: new_id,
        }
    }

    /// Mint plan approval ids with `mint` instead of random UUIDs (fixtures).
    pub fn with_minter(mut self, mint: fn() -> String) -> Self {
        self.mint = mint;
        self
    }

    /// Still following its turn: not over, not taking its screenshot.
    pub fn is_live(&self) -> bool {
        !self.state.is_terminal() && !self.finishing
    }

    /// Apply one chat frame of this task's session.
    pub fn observe(&mut self, payload: &Value, now: i64) -> Observed {
        let mut out = Observed::default();
        if !self.is_live() {
            return out;
        }
        let Some(p) = payload.get("payload") else {
            return out;
        };
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "stream_chunk" => {
                if let Some(text) = p.get("content").and_then(Value::as_str) {
                    self.reply_buf.push_str(text);
                }
                if let Some(event) = p.get("event") {
                    out.changed |= self.tool_event(event, now);
                }
            }
            "browser_tool_request" => {
                // The task's own result screenshot is not something the turn did.
                if p.get("origin").and_then(Value::as_str) == Some("system") {
                    return out;
                }
                if p.get("action").and_then(Value::as_str) == Some("ask_user") {
                    if let Some(approval) = ask_user_approval(p, now) {
                        self.approvals.push(approval);
                        out.changed = true;
                        out.new_approval = true;
                    }
                } else if let Some(tab) = p.get("tab_id").and_then(Value::as_i64) {
                    self.last_tab = Some(tab);
                }
            }
            "plan_proposal" => {
                self.approvals.retain(|a| a.kind != ApprovalKind::Plan);
                let steps = p
                    .get("steps")
                    .and_then(Value::as_array)
                    .map(|steps| {
                        steps
                            .iter()
                            .filter_map(|s| s.get("description").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                self.approvals.push(Approval {
                    approval_id: (self.mint)(),
                    kind: ApprovalKind::Plan,
                    summary: clip(
                        p.get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or("Review this plan?"),
                        APPROVAL_SUMMARY_MAX_CHARS,
                    ),
                    question: None,
                    options: Vec::new(),
                    plan_steps: steps,
                    must_approve: false,
                    at: now,
                });
                out.changed = true;
                out.new_approval = true;
            }
            "browser_tool_resolved" => {
                let id = p.get("request_id").and_then(Value::as_str).unwrap_or("");
                if self.remove_approval(id) {
                    out.changed = true;
                    out.timed_out = timed_out(p);
                }
            }
            "plan_resolved" => {
                let before = self.approvals.len();
                self.approvals.retain(|a| a.kind != ApprovalKind::Plan);
                if self.approvals.len() != before {
                    out.changed = true;
                    out.timed_out = timed_out(p);
                }
            }
            "artifact_start" => {
                if let Some(id) = p.get("id").and_then(Value::as_str) {
                    self.produced = Some(Target::Artifact(id.to_string()));
                }
            }
            "error" => {
                let message = p
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                self.saw_error = Some(message.to_string());
            }
            _ => {}
        }
        self.refresh_state();
        out
    }

    /// A browser call of the turn answered with a tab: the one it acted on,
    /// and the one it produced when it opened a new one.
    pub fn note_tab(&mut self, tab: i64, new_tab: bool) {
        if !self.is_live() {
            return;
        }
        if new_tab {
            self.produced = Some(Target::Tab(tab));
        }
        self.last_tab = Some(tab);
    }

    /// The most recent tool call that started and has not ended.
    pub fn current_tool(&self) -> Option<&str> {
        self.open.last().map(|(_, tool)| tool.as_str())
    }

    /// The sequence number the next step event will get.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Retained events from `cursor` on, and whether some after `cursor`
    /// were dropped (R9).
    pub fn events_since(&self, cursor: u64) -> (Vec<&StepEvent>, bool) {
        let truncated = self.events.front().is_some_and(|e| cursor < e.seq);
        (
            self.events.iter().filter(|e| e.seq >= cursor).collect(),
            truncated,
        )
    }

    /// Drop an open approval. Reports whether it was open.
    pub fn remove_approval(&mut self, approval_id: &str) -> bool {
        let before = self.approvals.len();
        self.approvals.retain(|a| a.approval_id != approval_id);
        let removed = self.approvals.len() != before;
        self.refresh_state();
        removed
    }

    /// [`Self::wait_json_for`] with the pairing's toggle off (contract v1.2
    /// R39's default): what tests and fixtures without a pairing see.
    pub fn wait_json(&self, cursor: u64, now: i64) -> Value {
        self.wait_json_for(cursor, now, false)
    }

    /// The `nevoflux_task_wait` result for a reader at `cursor` (contract §2),
    /// with the pairing's R39 toggle as read for this wait.
    pub fn wait_json_for(&self, cursor: u64, now: i64, muse_may_approve_guarded: bool) -> Value {
        let (events, truncated) = self.events_since(cursor);
        let steps: Vec<Value> = events
            .iter()
            .map(|e| json!({"id": e.id, "at": rfc3339(e.at), "tool": e.tool, "status": e.status}))
            .collect();
        let mut v = json!({
            "task_id": self.task_id,
            "session": self.session,
            "state": self.state.as_str(),
            "steps": steps,
        });
        if truncated {
            v["steps_truncated"] = json!(true);
        }
        match self.state {
            TaskState::AwaitingApproval => {
                let items: Vec<Value> = self
                    .approvals
                    .iter()
                    .map(|a| self.approval_item(a, now, muse_may_approve_guarded))
                    .collect();
                v["approvals"] = json!(items);
            }
            TaskState::Done => {
                v["reply"] = json!(self.reply.clone().unwrap_or_default());
                if self.reply_truncated {
                    v["reply_truncated"] = json!(true);
                }
                if self.screenshot.is_none() {
                    if let Some(e) = &self.screenshot_error {
                        v["screenshot_error"] = json!(e);
                    }
                }
            }
            TaskState::Failed => {
                if let Some((code, message)) = &self.error {
                    v["error"] = json!({"code": code, "message": message});
                }
            }
            TaskState::Running | TaskState::Cancelled => {}
        }
        v
    }

    fn approval_item(&self, a: &Approval, now: i64, muse_may_approve_guarded: bool) -> Value {
        let mut v = json!({
            "approval_id": a.approval_id,
            "kind": a.kind.as_str(),
            "summary": a.summary,
            "tool": self.current_tool(),
            "target": Value::Null,
            "choices": ["allow", "deny"],
            "at": rfc3339(a.at),
            "expires_in": (a.at + DELEGATED_HOLD.as_secs() as i64 - now).max(0),
        });
        match a.kind {
            ApprovalKind::Question => {
                v["question"] = json!(a.question);
                v["options"] = json!(a.options);
            }
            ApprovalKind::Plan => {
                v["plan_steps"] = json!(a.plan_steps);
            }
            ApprovalKind::Tool => {}
        }
        // Contract v1.2 §2.1 / R39: only the desktop may answer, unless the
        // person turned the pairing's toggle on.
        if a.must_approve && !muse_may_approve_guarded {
            v["desktop_only"] = json!(true);
            v["choices"] = json!([]);
        }
        v
    }

    /// The `task` of this pairing's entry in `system:agent:status`
    /// (contract §4). Only asked of a task that has not ended.
    pub fn status_json(&self) -> Value {
        let approval = self.approvals.first().map(|a| {
            let mut o = json!({
                "approval_id": a.approval_id,
                "kind": a.kind.as_str(),
                "summary": a.summary,
                "choices": desktop_choices(a),
                "started_at": a.at,
                "expires_at": a.at + DELEGATED_HOLD.as_secs() as i64,
            });
            // Contract v1.2 §4: where the sidebar's "Muse may also answer" hint goes.
            if a.must_approve {
                o["must_approve"] = json!(true);
            }
            o
        });
        json!({
            "id": self.task_id,
            "session_id": self.session,
            "summary": self.summary,
            "state": self.state.as_str(),
            "tool": self.current_tool(),
            "started_at": self.started_at,
            "approval": approval,
        })
    }

    /// The turn ended. `connected`: the sidebar it ran under is still there.
    /// `panicking`: it ended by unwinding.
    pub fn conclude(&mut self, connected: bool, panicking: bool, now: i64) -> Conclusion {
        if !self.is_live() {
            return Conclusion::Ended;
        }
        self.approvals.clear();
        if panicking {
            self.fail("interrupted", INTERRUPTED_MESSAGE, now);
            return Conclusion::Ended;
        }
        if let Some(detail) = self.saw_error.clone() {
            self.fail(
                "llm_error",
                &format!("the model call failed: {detail}"),
                now,
            );
            return Conclusion::Ended;
        }
        if !connected {
            self.fail("browser_unavailable", BROWSER_GONE_MESSAGE, now);
            return Conclusion::Ended;
        }
        self.finishing = true;
        self.state = TaskState::Running;
        Conclusion::Capture
    }

    /// Done, with the screenshot or why there is none.
    pub fn finish(&mut self, shot: Result<String, String>, now: i64) {
        if self.state.is_terminal() {
            return;
        }
        let text = self.reply_buf.trim();
        self.reply_truncated = text.chars().count() > REPLY_MAX_CHARS;
        self.reply = Some(text.chars().take(REPLY_MAX_CHARS).collect());
        match shot {
            Ok(data) => self.screenshot = Some(data),
            Err(why) => self.screenshot_error = Some(why),
        }
        self.end(TaskState::Done, now);
    }

    pub fn fail(&mut self, code: &str, message: &str, now: i64) {
        if self.state.is_terminal() {
            return;
        }
        self.error = Some((code.to_string(), message.to_string()));
        self.end(TaskState::Failed, now);
    }

    pub fn cancel(&mut self, now: i64) {
        if self.state.is_terminal() {
            return;
        }
        self.end(TaskState::Cancelled, now);
    }

    fn end(&mut self, state: TaskState, now: i64) {
        self.approvals.clear();
        self.finishing = false;
        self.state = state;
        self.ended_at = Some(now);
    }

    fn refresh_state(&mut self) {
        if self.is_live() {
            self.state = if self.approvals.is_empty() {
                TaskState::Running
            } else {
                TaskState::AwaitingApproval
            };
        }
    }

    fn tool_event(&mut self, event: &Value, now: i64) -> bool {
        let id = event
            .get("tool_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match event.get("type").and_then(Value::as_str) {
            Some("tool_start") => {
                let tool = event
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // The reply is what the model says after its last tool call.
                self.reply_buf.clear();
                self.open.push((id.clone(), tool.clone()));
                self.push_event(id, tool, "started", now);
                true
            }
            Some("tool_end") => {
                let status = match event.get("status").and_then(Value::as_str) {
                    Some("success") => "ok",
                    Some("failed") => "failed",
                    _ => return false,
                };
                let Some(at) = self.open.iter().position(|(open, _)| *open == id) else {
                    return false;
                };
                let (_, tool) = self.open.remove(at);
                self.push_event(id, tool, status, now);
                true
            }
            _ => false,
        }
    }

    fn push_event(&mut self, id: String, tool: String, status: &'static str, now: i64) {
        self.events.push_back(StepEvent {
            seq: self.next_seq,
            id,
            at: now,
            tool,
            status,
        });
        self.next_seq += 1;
        while self.events.len() > STEPS_MAX {
            self.events.pop_front();
        }
    }
}

fn timed_out(payload: &Value) -> bool {
    payload.get("reason").and_then(Value::as_str) == Some(crate::pending::REASON_TIMEOUT)
}

/// An `ask_user` request as an approval: `tool` when it is stamped as a
/// permission prompt (R13), otherwise the model's `question`.
fn ask_user_approval(p: &Value, now: i64) -> Option<Approval> {
    let id = p
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let params = p.get("params");
    let text = |key: &str| {
        params
            .and_then(|q| q.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let kind = if params
        .and_then(|q| q.get("purpose"))
        .and_then(Value::as_str)
        == Some(super::PERMISSION_PURPOSE)
    {
        ApprovalKind::Tool
    } else {
        ApprovalKind::Question
    };
    let must_approve = kind == ApprovalKind::Tool
        && params
            .and_then(|q| q.get(super::MUST_APPROVE_KEY))
            .and_then(Value::as_bool)
            == Some(true);
    let question = text("question");
    let summary = text("description")
        .or_else(|| question.clone())
        .unwrap_or_else(|| "Allow this action?".to_string());
    let options = params
        .and_then(|q| q.get("options"))
        .and_then(Value::as_array)
        .map(|o| {
            o.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(Approval {
        approval_id: id.to_string(),
        kind,
        summary: clip(&summary, APPROVAL_SUMMARY_MAX_CHARS),
        question: if kind == ApprovalKind::Question {
            question
        } else {
            None
        },
        options,
        plan_steps: Vec::new(),
        must_approve,
        at: now,
    })
}

/// RFC 3339 UTC, second precision, `Z` (R10).
pub fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// `text`, at most `max` characters, the last one `…` when cut.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The prompt's first non-blank line, whitespace collapsed, at most 80
/// characters plus `…` when cut (contract §4).
pub fn summary_line(prompt: &str) -> String {
    let first = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= STATUS_SUMMARY_MAX_CHARS {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(STATUS_SUMMARY_MAX_CHARS).collect();
    out.push('…');
    out
}

/// The buttons the desktop status bar offers for an approval (contract §4).
pub fn desktop_choices(a: &Approval) -> Vec<&'static str> {
    match a.kind {
        // Never "always" for a must-approve tool, whatever the toggle (v1.1/v1.2).
        _ if a.must_approve => vec!["allow", "deny"],
        ApprovalKind::Tool if a.options.iter().any(|o| o.starts_with("Always allow")) => {
            vec!["allow", "allow_always", "deny"]
        }
        ApprovalKind::Tool | ApprovalKind::Plan => vec!["allow", "deny"],
        ApprovalKind::Question => vec!["deny"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_792_000_000; // 2026-10-14T17:46:40Z

    fn record() -> TaskRecord {
        TaskRecord::new(
            "task-1",
            "chan-1",
            "s1",
            "Generate a calculator Canvas",
            "sidebar-1",
            T0,
        )
    }

    fn event(session: &str, event: Value) -> Value {
        json!({"type": "stream_chunk", "payload": {"session_id": session, "content": "", "done": false, "event": event}})
    }

    fn start(id: &str, tool: &str) -> Value {
        event(
            "s1",
            json!({"type": "tool_start", "tool_id": id, "tool_name": tool, "icon": "", "summary": ""}),
        )
    }

    fn end(id: &str, status: &str) -> Value {
        event(
            "s1",
            json!({"type": "tool_end", "tool_id": id, "status": status, "duration_ms": 5, "summary": ""}),
        )
    }

    fn ask(id: &str, purpose: Option<&str>, options: &[&str]) -> Value {
        let mut params = json!({"question": "Which colour?", "options": options});
        if let Some(p) = purpose {
            params["purpose"] = json!(p);
            params["description"] = json!("Create a Canvas artifact: \"Calculator\"");
        }
        json!({"type": "browser_tool_request", "payload": {"request_id": id, "session_id": "s1",
               "tab_id": null, "action": "ask_user", "params": params, "timeout_ms": 86400000}})
    }

    #[test]
    fn a_must_approve_question_is_desktop_only_unless_the_pairing_lets_muse_answer() {
        let mut r = record();
        let mut frame = ask("req-9", Some("permission"), &["Allow", "Deny"]);
        frame["payload"]["params"]["must_approve"] = json!(true);
        r.observe(&frame, T0 + 20);
        assert!(r.approvals[0].must_approve);
        // Off (the default, and what `wait_json` means): the agent is offered nothing.
        let item = &r.wait_json(0, T0 + 30)["approvals"][0];
        assert_eq!(item["kind"], "tool");
        assert_eq!(item["desktop_only"], true);
        assert_eq!(item["choices"], json!([]));
        assert_eq!(r.wait_json_for(0, T0 + 30, false)["approvals"][0], *item);
        // On: an ordinary approval for the agent, still never `allow_always`.
        let on = &r.wait_json_for(0, T0 + 30, true)["approvals"][0];
        assert!(on.get("desktop_only").is_none(), "{on}");
        assert_eq!(on["choices"], json!(["allow", "deny"]));
        // The desktop's buttons do not depend on the toggle.
        assert_eq!(desktop_choices(&r.approvals[0]), ["allow", "deny"]);
        assert_eq!(r.status_json()["approval"]["must_approve"], true);
        assert_eq!(r.status_json()["approval"]["choices"], json!(["allow", "deny"]));
        // An ordinary permission prompt carries neither key, whatever the toggle.
        r.observe(&ask("req-10", Some("permission"), &["Allow", "Deny"]), T0 + 21);
        assert!(r.wait_json_for(0, T0 + 30, false)["approvals"][1].get("desktop_only").is_none());
        assert!(!r.approvals[1].must_approve);
    }

    #[test]
    fn tool_events_become_started_and_ended_steps() {
        let mut r = record();
        assert!(r.observe(&start("t1", "skill_load"), T0 + 5).changed);
        r.observe(&end("t1", "success"), T0 + 9);
        r.observe(&start("t2", "browser_click"), T0 + 12);
        r.observe(&end("t2", "failed"), T0 + 13);
        r.observe(&end("never-started", "success"), T0 + 14);
        let v = r.wait_json(0, T0 + 15);
        assert_eq!(
            v["steps"],
            json!([
                {"id": "t1", "at": "2026-10-14T17:46:45Z", "tool": "skill_load", "status": "started"},
                {"id": "t1", "at": "2026-10-14T17:46:49Z", "tool": "skill_load", "status": "ok"},
                {"id": "t2", "at": "2026-10-14T17:46:52Z", "tool": "browser_click", "status": "started"},
                {"id": "t2", "at": "2026-10-14T17:46:53Z", "tool": "browser_click", "status": "failed"},
            ])
        );
        assert_eq!(
            r.wait_json(4, T0 + 15)["steps"],
            json!([]),
            "a cursor skips what was returned"
        );
        assert!(v.get("steps_truncated").is_none());
    }

    #[test]
    fn only_the_last_500_events_are_kept() {
        let mut r = record();
        for i in 0..260 {
            r.observe(&start(&format!("t{i}"), "think"), T0);
            r.observe(&end(&format!("t{i}"), "success"), T0);
        }
        let v = r.wait_json(0, T0);
        assert_eq!(v["steps"].as_array().unwrap().len(), 500);
        assert_eq!(v["steps_truncated"], true);
        assert!(
            r.wait_json(20, T0).get("steps_truncated").is_none(),
            "nothing after event 20 was lost"
        );
    }

    #[test]
    fn a_permission_prompt_is_a_tool_approval_and_the_models_question_a_question() {
        let mut r = record();
        r.observe(&start("t2", "create_artifact"), T0 + 12);
        let seen = r.observe(
            &ask(
                "req-1",
                Some("permission"),
                &["Allow", "Always allow this type of action", "Deny"],
            ),
            T0 + 20,
        );
        assert!(seen.new_approval);
        r.observe(&ask("req-2", None, &["Blue", "Green"]), T0 + 30);
        assert_eq!(r.state, TaskState::AwaitingApproval);
        let v = r.wait_json(0, T0 + 80);
        assert_eq!(
            v["approvals"][0],
            json!({"approval_id": "req-1", "kind": "tool", "summary": "Create a Canvas artifact: \"Calculator\"",
                   "tool": "create_artifact", "target": null, "choices": ["allow", "deny"],
                   "at": "2026-10-14T17:47:00Z", "expires_in": 1740})
        );
        assert_eq!(
            v["approvals"][1],
            json!({"approval_id": "req-2", "kind": "question", "summary": "Which colour?",
                   "tool": "create_artifact", "target": null, "question": "Which colour?",
                   "options": ["Blue", "Green"], "choices": ["allow", "deny"],
                   "at": "2026-10-14T17:47:10Z", "expires_in": 1750})
        );
    }

    #[test]
    fn a_plan_is_an_approval_with_a_minted_id_and_its_steps() {
        fn fixed() -> String {
            "plan-id".into()
        }
        let mut r = record().with_minter(fixed);
        r.observe(
            &json!({"type": "plan_proposal", "payload": {"session_id": "s1", "summary": "Build the calculator",
                    "steps": [{"description": "Write the HTML"}, {"description": "Render the Canvas"}]}}),
            T0 + 40,
        );
        let item = &r.wait_json(0, T0 + 60)["approvals"][0];
        assert_eq!(item["approval_id"], "plan-id");
        assert_eq!(item["kind"], "plan");
        assert_eq!(
            item["plan_steps"],
            json!(["Write the HTML", "Render the Canvas"])
        );
        assert!(item.get("question").is_none() && item.get("options").is_none());
        r.observe(&json!({"type": "plan_resolved", "payload": {"session_id": "s1", "response": "confirmed"}}), T0 + 61);
        assert_eq!(r.state, TaskState::Running);
    }

    #[test]
    fn a_resolved_approval_leaves_and_a_timed_out_one_says_so() {
        let mut r = record();
        r.observe(&ask("req-1", Some("permission"), &["Allow", "Deny"]), T0);
        r.observe(&ask("req-2", Some("permission"), &["Allow", "Deny"]), T0);
        let seen = r.observe(
            &crate::pending::browser_resolved("req-1", "s1", None),
            T0 + 1,
        );
        assert!(seen.changed && !seen.timed_out);
        assert_eq!(r.state, TaskState::AwaitingApproval, "one is still open");
        let seen = r.observe(
            &crate::pending::browser_resolved("req-2", "s1", Some("timeout")),
            T0 + 2,
        );
        assert!(seen.timed_out);
        assert_eq!(r.state, TaskState::Running);
    }

    #[test]
    fn the_screenshot_target_and_the_reply_come_from_the_turn() {
        let mut r = record();
        r.observe(&json!({"type": "browser_tool_request", "payload": {"request_id": "x", "session_id": "s1",
                    "tab_id": 41, "action": "click", "params": {}, "timeout_ms": 30000}}), T0);
        assert_eq!(r.last_tab, Some(41));
        r.observe(&json!({"type": "browser_tool_request", "payload": {"request_id": "y", "session_id": "s1",
                    "tab_id": 99, "action": "screenshot", "params": {}, "timeout_ms": 30000, "origin": "system"}}), T0);
        assert_eq!(
            r.last_tab,
            Some(41),
            "the task's own screenshot is not something the turn did"
        );
        r.observe(&json!({"type": "stream_chunk", "payload": {"session_id": "s1", "content": "Working…", "done": false}}), T0);
        r.observe(&start("t1", "create_artifact"), T0);
        r.observe(&json!({"type": "stream_chunk", "payload": {"session_id": "s1", "content": "Here is ", "done": false}}), T0);
        r.observe(&json!({"type": "stream_chunk", "payload": {"session_id": "s1", "content": "your calculator.", "done": false}}), T0);
        r.observe(&json!({"type": "artifact_start", "payload": {"session_id": "s1", "id": "calc-1", "title": "C", "content_type": "text/html"}}), T0);
        assert_eq!(r.produced, Some(Target::Artifact("calc-1".into())));
        assert_eq!(r.conclude(true, false, T0 + 1), Conclusion::Capture);
        r.finish(Ok("/9j/".into()), T0 + 2);
        let v = r.wait_json(0, T0 + 3);
        assert_eq!(v["state"], "done");
        assert_eq!(
            v["reply"], "Here is your calculator.",
            "the text after the last tool call"
        );
        assert!(v.get("screenshot_error").is_none());
    }

    #[test]
    fn how_a_turn_ends_decides_how_the_task_ends() {
        let mut errored = record();
        errored.observe(&json!({"type": "error", "payload": {"session_id": "s1", "code": "AGENT_ERROR", "message": "Agent error: rate limited"}}), T0);
        assert_eq!(errored.conclude(true, false, T0), Conclusion::Ended);
        assert_eq!(
            errored.wait_json(0, T0)["error"],
            json!({"code": "llm_error", "message": "the model call failed: Agent error: rate limited"})
        );
        let mut orphaned = record();
        assert_eq!(orphaned.conclude(false, false, T0), Conclusion::Ended);
        assert_eq!(
            orphaned.wait_json(0, T0)["error"]["code"],
            "browser_unavailable"
        );
        assert_eq!(
            orphaned.wait_json(0, T0)["error"]["message"],
            "the NevoFlux desktop browser disconnected"
        );
        let mut panicked = record();
        assert_eq!(panicked.conclude(true, true, T0), Conclusion::Ended);
        assert_eq!(panicked.wait_json(0, T0)["error"]["code"], "interrupted");
        let mut cancelled = record();
        cancelled.cancel(T0);
        assert_eq!(
            cancelled.conclude(true, false, T0),
            Conclusion::Ended,
            "already over"
        );
        assert_eq!(
            cancelled.wait_json(0, T0),
            json!({"task_id": "task-1", "session": "s1", "state": "cancelled", "steps": []})
        );
    }

    #[test]
    fn a_long_reply_is_cut_and_says_so() {
        let mut r = record();
        r.observe(&json!({"type": "stream_chunk", "payload": {"session_id": "s1", "content": "é".repeat(32_005), "done": false}}), T0);
        r.conclude(true, false, T0);
        r.finish(Err("no_tab".into()), T0);
        let v = r.wait_json(0, T0);
        assert_eq!(v["reply"].as_str().unwrap().chars().count(), 32_000);
        assert_eq!(v["reply_truncated"], true);
        assert_eq!(v["screenshot_error"], "no_tab");
    }

    #[test]
    fn the_status_bar_sees_the_oldest_approval_with_desktop_choices() {
        let mut r = record();
        r.observe(
            &ask(
                "req-1",
                Some("permission"),
                &["Allow", "Always allow this type of action", "Deny"],
            ),
            T0 + 20,
        );
        assert_eq!(
            r.status_json(),
            json!({"id": "task-1", "session_id": "s1", "summary": "Generate a calculator Canvas",
                   "state": "awaiting_approval", "tool": null, "started_at": T0,
                   "approval": {"approval_id": "req-1", "kind": "tool",
                                "summary": "Create a Canvas artifact: \"Calculator\"",
                                "choices": ["allow", "allow_always", "deny"],
                                "started_at": T0 + 20, "expires_at": T0 + 20 + 1800}})
        );
        let no_always = Approval {
            options: vec!["Allow".into(), "Deny".into()],
            ..r.approvals[0].clone()
        };
        assert_eq!(desktop_choices(&no_always), ["allow", "deny"]);
        let question = Approval {
            kind: ApprovalKind::Question,
            ..r.approvals[0].clone()
        };
        assert_eq!(desktop_choices(&question), ["deny"]);
        let plan = Approval {
            kind: ApprovalKind::Plan,
            ..r.approvals[0].clone()
        };
        assert_eq!(desktop_choices(&plan), ["allow", "deny"]);
    }

    #[test]
    fn summaries_are_first_lines_cut_to_size() {
        assert_eq!(
            summary_line("  Build   a\tcalculator \n and more"),
            "Build a calculator"
        );
        let long = "x".repeat(81);
        assert_eq!(summary_line(&long), format!("{}…", "x".repeat(80)));
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
    }
}
