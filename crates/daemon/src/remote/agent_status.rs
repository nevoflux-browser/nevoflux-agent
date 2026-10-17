//! What the desktop is shown about each paired agent (M2b §2.3–2.4).
//!
//! An agent may read and write every tab without being stopped (decision
//! D5); the person at the desktop is compensated by seeing it. This module
//! holds, per agent channel, whether a session is live, which calls are
//! running and which question is open, and publishes the whole picture —
//! never a delta — as the sticky `system:agent:status` event, so a sidebar
//! that opens late or an extension that reloads gets the current state on
//! subscribe (B6). How each question ended goes out once on
//! `ui:notification:agent` (B3).
//!
//! Nothing here ever sees a tool's arguments: what is shown is the tool's
//! name and a summary the backend wrote without the values
//! (`agent_backend::approval_summary`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use serde_json::{json, Value};

use super::mcp_tools::Approval;
use super::pairing::Pairing;
use crate::event_bus::{BusEvent, EventBus, PublisherIdentity};

/// The sticky snapshot topic (contract §1).
pub const TOPIC_STATUS: &str = "system:agent:status";
/// Where approval outcomes are announced (contract §3).
pub const TOPIC_NOTIFICATION: &str = "ui:notification:agent";
/// `source` of an approval-outcome notice; `background.js` keys its system
/// notification on it.
pub const APPROVAL_SOURCE: &str = "agent_approval";
/// What an agent without a label is called.
const DEFAULT_LABEL: &str = "Muse";

/// Where an agent stands, as the snapshot says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// An MCP session is live on its channel.
    Connected,
    /// The person paused it; its channel is not dialled.
    Paused,
    /// Neither.
    Offline,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Connected => "connected",
            AgentState::Paused => "paused",
            AgentState::Offline => "offline",
        }
    }
}

/// How a question put to the agent's client ended (contract §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Declined,
    TimedOut,
    /// The client cannot be asked, the question could not be put, or the
    /// session went away while it was open.
    Cancelled,
    /// Approved, but the call did not run.
    Stopped(StopReason),
}

/// Why an approved call did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The person lowered the agent's tier while it was asked.
    TierLowered,
    /// Calls from the agent now go to another browser, or to none: the tab
    /// the question named is not there to act on.
    BrowserChanged,
}

impl ApprovalOutcome {
    pub fn of(answer: Approval) -> Self {
        match answer {
            Approval::Approved => ApprovalOutcome::Approved,
            Approval::Declined => ApprovalOutcome::Declined,
            Approval::TimedOut => ApprovalOutcome::TimedOut,
            Approval::Unsupported | Approval::Failed => ApprovalOutcome::Cancelled,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalOutcome::Approved => "approved",
            ApprovalOutcome::Declined => "declined",
            ApprovalOutcome::TimedOut => "timed_out",
            ApprovalOutcome::Cancelled => "cancelled",
            ApprovalOutcome::Stopped(_) => "stopped",
        }
    }

    /// The notice's title, naming the agent by its label when it has one.
    pub fn title(self, label: Option<&str>) -> String {
        let who = label
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or(DEFAULT_LABEL);
        match self {
            ApprovalOutcome::Approved => format!("{who} approved"),
            ApprovalOutcome::Declined => format!("{who} declined"),
            ApprovalOutcome::TimedOut => format!(
                "{who} did not answer within {} s — denied",
                super::elicit::APPROVAL_TIMEOUT.as_secs()
            ),
            ApprovalOutcome::Cancelled => format!("Approval cancelled — {who} disconnected"),
            ApprovalOutcome::Stopped(StopReason::TierLowered) => {
                format!("{who} approved — stopped: the agent's tier was lowered")
            }
            ApprovalOutcome::Stopped(StopReason::BrowserChanged) => {
                format!("{who} approved — stopped: the browser changed")
            }
        }
    }
}

/// One agent pairing as the snapshot lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub channel_id: String,
    pub label: Option<String>,
    pub created_at: i64,
    pub paused: bool,
    /// The pairing's R39 toggle, as stored (contract v1.2 §4).
    pub muse_may_approve_guarded: bool,
}

impl AgentRow {
    /// The agents among `pairings`, oldest first. Devices never appear.
    pub fn from_pairings(pairings: &[Pairing]) -> Vec<AgentRow> {
        let mut rows: Vec<AgentRow> = pairings
            .iter()
            .filter(|p| p.is_agent())
            .map(|p| AgentRow {
                channel_id: p.control_channel_id.clone(),
                label: p.label.clone(),
                created_at: p.created_at,
                paused: p.paused,
                muse_may_approve_guarded: p.muse_may_approve_guarded,
            })
            .collect();
        rows.sort_by_key(|r| r.created_at);
        rows
    }
}

/// A question that is open on a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// The call it belongs to.
    pub id: u64,
    /// The tool awaiting the answer.
    pub tool: String,
    /// What is being asked, without any value the agent typed.
    pub summary: String,
    /// Unix seconds.
    pub started_at: i64,
}

/// What is happening on one agent channel.
#[derive(Debug, Clone, Default)]
pub struct ChannelState {
    /// An MCP session is live.
    pub connected: bool,
    /// Calls running, oldest first: (call id, tool). Several can run at once.
    pub tools: Vec<(u64, String)>,
    /// Questions open, oldest first.
    pub approvals: Vec<PendingApproval>,
    /// The state last shown, and since when.
    pub shown: Option<(AgentState, i64)>,
    /// The delegated task this agent is running (M3 contract §4), or `None`.
    pub task: Option<Value>,
}

/// The full snapshot (contract §1).
///
/// Pure apart from the `since` bookkeeping it owns: an agent whose state
/// differs from the one last shown is stamped `now`. While a question is
/// open its tool is the one shown (the oldest open question — the one whose
/// countdown ends first); otherwise the most recent running call.
pub fn snapshot(
    rows: &[AgentRow],
    channels: &mut HashMap<String, ChannelState>,
    now: i64,
) -> Value {
    let agents: Vec<Value> = rows
        .iter()
        .map(|row| {
            let ch = channels.entry(row.channel_id.clone()).or_default();
            let state = if row.paused {
                AgentState::Paused
            } else if ch.connected {
                AgentState::Connected
            } else {
                AgentState::Offline
            };
            let since = match ch.shown {
                Some((shown, since)) if shown == state => since,
                _ => {
                    ch.shown = Some((state, now));
                    now
                }
            };
            let approval = ch.approvals.first();
            let tool = approval
                .map(|a| a.tool.clone())
                .or_else(|| ch.tools.last().map(|(_, t)| t.clone()));
            json!({
                "channel_id": row.channel_id,
                "label": row.label,
                "state": state.as_str(),
                "muse_may_approve_guarded": row.muse_may_approve_guarded,
                "since": since,
                "tool": tool,
                "approval": approval.map(|a| json!({
                    "summary": a.summary,
                    "started_at": a.started_at,
                })),
                "task": ch.task.clone(),
            })
        })
        .collect();
    json!({ "agents": agents })
}

/// The one notice for how a question ended (contract §3). Never carries a
/// value the agent typed: `summary` is built without them.
pub fn approval_notification(
    outcome: ApprovalOutcome,
    label: Option<&str>,
    summary: &str,
) -> Value {
    json!({
        "event_id": uuid::Uuid::new_v4().to_string(),
        "title": outcome.title(label),
        "body": summary,
        "source": APPROVAL_SOURCE,
        "outcome": outcome.as_str(),
    })
}

/// Where the registry's events go.
pub trait StatusSink: Send + Sync {
    /// Called with the registry's lock held: must not call back into
    /// [`AgentStatus`].
    fn send(&self, event: BusEvent);
}

/// The daemon's event bus, fed through one task.
///
/// One task, not a spawn per event: two spawned publishes can land in either
/// order, and a stale snapshot landing last would be what every later
/// subscriber replays as the current state.
pub struct BusSink {
    tx: tokio::sync::mpsc::UnboundedSender<BusEvent>,
}

impl BusSink {
    /// Must be called inside the runtime.
    pub fn spawn(bus: Arc<EventBus>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BusEvent>();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let topic = event.topic.clone();
                if let Err(e) = bus.publish(event).await {
                    tracing::warn!(target: "remote", "could not publish {topic}: {e}");
                }
            }
        });
        Self { tx }
    }
}

impl StatusSink for BusSink {
    fn send(&self, event: BusEvent) {
        let _ = self.tx.send(event);
    }
}

#[derive(Default)]
struct Inner {
    rows: Vec<AgentRow>,
    channels: HashMap<String, ChannelState>,
    next_call: u64,
}

/// What every agent channel is doing, published whole on every change.
pub struct AgentStatus {
    inner: Mutex<Inner>,
    sink: RwLock<Option<Arc<dyn StatusSink>>>,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for AgentStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentStatus {
    /// A registry that sends nothing until [`Self::attach`] gives it a sink.
    pub fn new() -> Self {
        Self {
            inner: Mutex::default(),
            sink: RwLock::new(None),
            clock: Box::new(unix_now),
        }
    }

    pub fn with_sink(self, sink: Arc<dyn StatusSink>) -> Self {
        self.attach(sink);
        self
    }

    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Start (or redirect) publishing.
    pub fn attach(&self, sink: Arc<dyn StatusSink>) {
        *self.sink.write().unwrap_or_else(|p| p.into_inner()) = Some(sink);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn send(&self, event: BusEvent) {
        if let Some(sink) = self.sink.read().unwrap_or_else(|p| p.into_inner()).as_ref() {
            sink.send(event);
        }
    }

    /// Recompute and send the snapshot. Called with `inner` held, so
    /// snapshots leave in the order they were made.
    fn publish(&self, inner: &mut Inner) {
        let now = (self.clock)();
        let Inner { rows, channels, .. } = inner;
        let snap = snapshot(rows, channels, now);
        self.send(BusEvent::sticky(
            TOPIC_STATUS,
            snap,
            PublisherIdentity::Internal,
        ));
    }

    /// The snapshot as it stands, without sending it.
    pub fn current(&self) -> Value {
        let now = (self.clock)();
        let mut inner = self.lock();
        let Inner { rows, channels, .. } = &mut *inner;
        snapshot(rows, channels, now)
    }

    /// The pairings changed (startup, a new pairing, pause, resume, unpair).
    pub fn set_pairings(&self, pairings: &[Pairing]) {
        let mut inner = self.lock();
        inner.rows = AgentRow::from_pairings(pairings);
        self.publish(&mut inner);
    }

    /// Drop everything known about `channel` (its pairing is gone). Every
    /// question still open on it is announced as cancelled, once; no snapshot
    /// is sent: the caller republishes with [`Self::set_pairings`]. A watch
    /// still running on it ends later without bringing the entry back or
    /// announcing anything.
    ///
    /// Call this BEFORE `set_pairings` drops the channel's row, so the
    /// notices still carry the pairing's label.
    pub fn forget(&self, channel: &str) {
        let mut inner = self.lock();
        let Some(ch) = inner.channels.remove(channel) else {
            return;
        };
        let label = inner
            .rows
            .iter()
            .find(|r| r.channel_id == channel)
            .and_then(|r| r.label.clone());
        for pending in ch.approvals {
            self.send(BusEvent::ephemeral(
                TOPIC_NOTIFICATION,
                approval_notification(
                    ApprovalOutcome::Cancelled,
                    label.as_deref(),
                    &pending.summary,
                ),
                PublisherIdentity::Internal,
            ));
        }
    }

    /// The delegated task this agent is running now, or `None` once it ended
    /// (M3 contract §4). Republished only when it changed. Clearing an
    /// unknown channel does not bring it back.
    pub fn set_task(&self, channel: &str, task: Option<Value>) {
        let mut inner = self.lock();
        if task.is_none() && !inner.channels.contains_key(channel) {
            return;
        }
        let ch = inner.channels.entry(channel.to_string()).or_default();
        if ch.task == task {
            return;
        }
        ch.task = task;
        self.publish(&mut inner);
    }

    /// One ephemeral notice on `ui:notification:agent` (a delegated task
    /// ended, M3 contract §9).
    pub fn notify(&self, payload: Value) {
        self.send(BusEvent::ephemeral(
            TOPIC_NOTIFICATION,
            payload,
            PublisherIdentity::Internal,
        ));
    }

    pub fn session_started(&self, channel: &str) {
        let mut inner = self.lock();
        inner
            .channels
            .entry(channel.to_string())
            .or_default()
            .connected = true;
        self.publish(&mut inner);
    }

    /// The session closed. A channel already forgotten (its pairing is gone)
    /// is not brought back by a teardown landing late, and nothing is sent.
    pub fn session_ended(&self, channel: &str) {
        let mut inner = self.lock();
        let Some(ch) = inner.channels.get_mut(channel) else {
            return;
        };
        ch.connected = false;
        self.publish(&mut inner);
    }

    /// A whitelisted call began; it is shown until the watch is dropped.
    pub fn tool_started(self: &Arc<Self>, channel: &str, tool: &str) -> ToolWatch {
        let mut inner = self.lock();
        inner.next_call += 1;
        let id = inner.next_call;
        inner
            .channels
            .entry(channel.to_string())
            .or_default()
            .tools
            .push((id, tool.to_string()));
        self.publish(&mut inner);
        ToolWatch {
            status: self.clone(),
            channel: channel.to_string(),
            id,
        }
    }

    fn tool_ended(&self, channel: &str, id: u64) {
        let mut inner = self.lock();
        let Some(ch) = inner.channels.get_mut(channel) else {
            return;
        };
        ch.tools.retain(|(call, _)| *call != id);
        self.publish(&mut inner);
    }

    /// A question was put; it is shown until the watch finishes or drops.
    pub fn approval_started(
        self: &Arc<Self>,
        channel: &str,
        tool: &str,
        summary: &str,
    ) -> ApprovalWatch {
        let started_at = (self.clock)();
        let mut inner = self.lock();
        inner.next_call += 1;
        let id = inner.next_call;
        inner
            .channels
            .entry(channel.to_string())
            .or_default()
            .approvals
            .push(PendingApproval {
                id,
                tool: tool.to_string(),
                summary: summary.to_string(),
                started_at,
            });
        self.publish(&mut inner);
        ApprovalWatch {
            status: self.clone(),
            channel: channel.to_string(),
            id,
            finished: false,
        }
    }

    /// The question ended: one notice, then the snapshot without it. A
    /// question already ended is not announced again.
    fn approval_ended(&self, channel: &str, id: u64, outcome: ApprovalOutcome) {
        let mut inner = self.lock();
        let Some(pending) = inner.channels.get_mut(channel).and_then(|ch| {
            let at = ch.approvals.iter().position(|a| a.id == id)?;
            Some(ch.approvals.remove(at))
        }) else {
            return;
        };
        let label = inner
            .rows
            .iter()
            .find(|r| r.channel_id == channel)
            .and_then(|r| r.label.clone());
        self.send(BusEvent::ephemeral(
            TOPIC_NOTIFICATION,
            approval_notification(outcome, label.as_deref(), &pending.summary),
            PublisherIdentity::Internal,
        ));
        self.publish(&mut inner);
    }
}

/// A running call. Dropping it takes the call off the snapshot, however the
/// call ended — returned, failed, or its future dropped.
#[must_use]
pub struct ToolWatch {
    status: Arc<AgentStatus>,
    channel: String,
    id: u64,
}

impl Drop for ToolWatch {
    fn drop(&mut self) {
        self.status.tool_ended(&self.channel, self.id);
    }
}

/// An open question. [`ApprovalWatch::finish`] announces its outcome; dropped
/// without one — the call's future dropped mid-question — it is announced as
/// cancelled. Either way, exactly once.
#[must_use]
pub struct ApprovalWatch {
    status: Arc<AgentStatus>,
    channel: String,
    id: u64,
    finished: bool,
}

impl ApprovalWatch {
    pub fn finish(self, answer: Approval) {
        self.finish_as(ApprovalOutcome::of(answer));
    }

    pub fn finish_as(mut self, outcome: ApprovalOutcome) {
        self.finished = true;
        self.status.approval_ended(&self.channel, self.id, outcome);
    }
}

impl Drop for ApprovalWatch {
    fn drop(&mut self) {
        if !self.finished {
            self.status
                .approval_ended(&self.channel, self.id, ApprovalOutcome::Cancelled);
        }
    }
}

/// The process's one registry. `server.rs` attaches it to the event bus at
/// startup and hands it to `ControlDeps`; channels and backends built without
/// one explicitly report here.
pub fn shared() -> Arc<AgentStatus> {
    static SHARED: OnceLock<Arc<AgentStatus>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(AgentStatus::new())).clone()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::remote::pairing::PairingKind;

    /// An agent pairing with nothing but an id and an age.
    pub(crate) fn agent(id: &str, created_at: i64) -> Pairing {
        Pairing {
            control_channel_id: id.into(),
            data_channel_id: String::new(),
            control_key: "cc".repeat(32),
            data_key: String::new(),
            created_at,
            label: None,
            push: None,
            kind: PairingKind::Agent,
            tier: None,
            paused: false,
            muse_may_approve_guarded: false,
        }
    }

    use crate::event_bus::Delivery;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// Everything a registry sent, in order.
    #[derive(Default)]
    pub(crate) struct Captured(Mutex<Vec<BusEvent>>);

    impl StatusSink for Captured {
        fn send(&self, event: BusEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl Captured {
        /// Every snapshot, oldest first; each must have been sticky.
        pub(crate) fn statuses(&self) -> Vec<Value> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.topic == TOPIC_STATUS)
                .map(|e| {
                    assert_eq!(e.delivery, Delivery::Sticky, "a snapshot is sticky");
                    e.payload.clone()
                })
                .collect()
        }

        /// Every approval notice, oldest first; each must have been ephemeral.
        pub(crate) fn notifications(&self) -> Vec<Value> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.topic == TOPIC_NOTIFICATION)
                .map(|e| {
                    assert_eq!(e.delivery, Delivery::Ephemeral, "a notice is not replayed");
                    e.payload.clone()
                })
                .collect()
        }

        pub(crate) fn clear(&self) {
            self.0.lock().unwrap().clear();
        }
    }

    /// A registry sending into a `Captured`, its clock reading `at` until moved.
    pub(crate) fn watched(at: i64) -> (Arc<AgentStatus>, Arc<Captured>, Arc<AtomicI64>) {
        let seen = Arc::new(Captured::default());
        let clock = Arc::new(AtomicI64::new(at));
        let c = clock.clone();
        let status = AgentStatus::new()
            .with_sink(seen.clone())
            .with_clock(move || c.load(Ordering::SeqCst));
        (Arc::new(status), seen, clock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::pairing::PairingKind;

    use crate::event_bus::{BackpressurePolicy, Delivery, SubscriberIdentity, TopicPattern};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[test]
    fn every_change_publishes_a_full_sticky_snapshot() {
        let (status, seen, clock) = testing::watched(100);
        status.set_pairings(&[testing::agent("a", 1)]);
        status.session_started("a");
        clock.store(130, Ordering::SeqCst);
        let running = status.tool_started("a", "browser_screenshot");
        drop(running);
        let shown: Vec<(Value, Value, Value)> = seen
            .statuses()
            .iter()
            .map(|s| {
                let a = &s["agents"][0];
                (a["state"].clone(), a["since"].clone(), a["tool"].clone())
            })
            .collect();
        assert_eq!(
            shown,
            vec![
                (json!("offline"), json!(100), Value::Null),
                (json!("connected"), json!(100), Value::Null),
                (json!("connected"), json!(100), json!("browser_screenshot")),
                (json!("connected"), json!(100), Value::Null),
            ]
        );
        assert!(seen.notifications().is_empty(), "no question, no notice");
    }

    #[test]
    fn an_approval_is_announced_exactly_once() {
        let (status, seen, clock) = testing::watched(100);
        let labelled = Pairing {
            label: Some("Claude".into()),
            ..testing::agent("a", 1)
        };
        status.set_pairings(&[labelled]);
        let watch =
            status.approval_started("a", "browser_click", "Click on element '#go' in tab 1");
        let open = status.current();
        assert_eq!(open["agents"][0]["tool"], "browser_click");
        assert_eq!(
            open["agents"][0]["approval"],
            json!({ "summary": "Click on element '#go' in tab 1", "started_at": 100 })
        );
        assert!(
            seen.notifications().is_empty(),
            "nothing is announced when a question opens"
        );
        clock.store(110, Ordering::SeqCst);
        watch.finish(Approval::Approved);
        let notes = seen.notifications();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["title"], "Claude approved");
        assert_eq!(notes[0]["body"], "Click on element '#go' in tab 1");
        assert_eq!(notes[0]["outcome"], "approved");
        assert!(status.current()["agents"][0]["approval"].is_null());
    }

    #[test]
    fn an_approval_dropped_unanswered_is_announced_as_cancelled() {
        // A call's future can be dropped while its question is open (its
        // session torn down). The desktop must still hear how it ended, once.
        let (status, seen, _clock) = testing::watched(100);
        status.set_pairings(&[testing::agent("a", 1)]);
        drop(status.approval_started("a", "browser_click", "Click on element '#go' in tab 1"));
        let notes = seen.notifications();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["outcome"], "cancelled");
        assert_eq!(notes[0]["title"], "Approval cancelled — Muse disconnected");
        assert!(status.current()["agents"][0]["approval"].is_null());
    }

    #[test]
    fn two_calls_at_once_each_clear_only_their_own() {
        let (status, _seen, _clock) = testing::watched(100);
        status.set_pairings(&[testing::agent("a", 1)]);
        let first = status.tool_started("a", "browser_snapshot");
        let second = status.tool_started("a", "browser_screenshot");
        drop(first);
        assert_eq!(status.current()["agents"][0]["tool"], "browser_screenshot");
        drop(second);
        assert!(status.current()["agents"][0]["tool"].is_null());
    }

    #[test]
    fn without_a_sink_nothing_is_sent_and_nothing_breaks() {
        let status = Arc::new(AgentStatus::new());
        status.set_pairings(&[testing::agent("a", 1)]);
        status.session_started("a");
        drop(status.approval_started("a", "browser_click", "Click"));
        assert_eq!(status.current()["agents"][0]["state"], "connected");
    }

    #[test]
    fn a_forgotten_channel_comes_back_with_a_fresh_state() {
        let (status, seen, clock) = testing::watched(100);
        let labelled = Pairing {
            label: Some("Claude".into()),
            ..testing::agent("a", 1)
        };
        status.set_pairings(&[labelled]);
        status.session_started("a");
        let running = status.tool_started("a", "browser_snapshot");
        let asking = status.approval_started("a", "browser_click", "Click");
        seen.clear();
        status.forget("a");
        assert!(
            seen.statuses().is_empty(),
            "forgetting publishes no snapshot"
        );
        let notes = seen.notifications();
        assert_eq!(notes.len(), 1, "the open question is announced once");
        assert_eq!(notes[0]["outcome"], "cancelled");
        assert_eq!(
            notes[0]["title"],
            "Approval cancelled — Claude disconnected"
        );
        assert_eq!(notes[0]["body"], "Click");
        status.set_pairings(&[]);
        seen.clear();
        status.forget("a");
        assert!(seen.statuses().is_empty(), "forgetting publishes nothing");
        assert!(seen.notifications().is_empty(), "nothing left to announce");
        clock.store(200, Ordering::SeqCst);
        status.set_pairings(&[testing::agent("a", 1)]);
        let fresh = json!({ "channel_id": "a", "label": null, "state": "offline",
                            "muse_may_approve_guarded": false,
                            "since": 200, "tool": null, "approval": null, "task": null });
        assert_eq!(status.current()["agents"][0], fresh);
        // Watches from before the forget end without bringing anything back.
        seen.clear();
        drop(running);
        drop(asking);
        assert_eq!(status.current()["agents"][0], fresh);
        assert!(
            seen.notifications().is_empty(),
            "a question forgotten is not announced again"
        );
    }

    #[test]
    fn a_teardown_after_forget_brings_nothing_back() {
        let (status, seen, _clock) = testing::watched(100);
        status.set_pairings(&[testing::agent("a", 1)]);
        status.session_started("a");
        let running = status.tool_started("a", "browser_snapshot");
        status.forget("a");
        status.set_pairings(&[]);
        seen.clear();
        status.session_ended("a");
        drop(running);
        assert!(
            !status.lock().channels.contains_key("a"),
            "no entry re-created"
        );
        assert!(seen.statuses().is_empty(), "no snapshot published");
        assert!(seen.notifications().is_empty());
    }

    #[tokio::test]
    async fn the_bus_replays_the_latest_snapshot_to_a_late_subscriber() {
        let bus = Arc::new(EventBus::new());
        let status = AgentStatus::new().with_sink(Arc::new(BusSink::spawn(bus.clone())));
        status.set_pairings(&[testing::agent("a", 1)]);
        status.session_started("a");
        for _ in 0..200 {
            if bus.total_published() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(bus.total_published(), 2);
        let mut late = bus
            .subscribe(
                TopicPattern::exact(TOPIC_STATUS),
                SubscriberIdentity::Extension {
                    proxy_id: "sidebar".into(),
                },
                BackpressurePolicy::DropOldest,
                256,
            )
            .expect("system:* is open to the extension");
        let mut last = None;
        while let Ok(ev) = late.rx.try_recv() {
            assert_eq!(ev.delivery, Delivery::Sticky);
            last = Some(ev.payload);
        }
        assert_eq!(last.expect("replayed")["agents"][0]["state"], "connected");
    }

    #[test]
    fn a_snapshot_lists_agents_oldest_first_and_never_a_device() {
        let device = Pairing {
            kind: PairingKind::Device,
            data_channel_id: "d-data".into(),
            ..testing::agent("d1", 0)
        };
        let early = Pairing {
            label: Some("Claude".into()),
            ..testing::agent("early", 3)
        };
        let rows = AgentRow::from_pairings(&[testing::agent("late", 9), device, early]);
        let snap = snapshot(&rows, &mut HashMap::new(), 100);
        assert_eq!(
            snap,
            json!({ "agents": [
                { "channel_id": "early", "label": "Claude", "state": "offline",
                  "muse_may_approve_guarded": false,
                  "since": 100, "tool": null, "approval": null, "task": null },
                { "channel_id": "late", "label": null, "state": "offline",
                  "muse_may_approve_guarded": false,
                  "since": 100, "tool": null, "approval": null, "task": null },
            ]})
        );
    }

    #[test]
    fn the_snapshot_says_whether_muse_may_approve_sensitive_actions() {
        let (status, _seen, _clock) = testing::watched(100);
        let on = Pairing { muse_may_approve_guarded: true, ..testing::agent("a", 1) };
        status.set_pairings(&[on, testing::agent("b", 2)]);
        let agents = status.current()["agents"].clone();
        assert_eq!(agents[0]["muse_may_approve_guarded"], true);
        assert_eq!(agents[1]["muse_may_approve_guarded"], false);
    }

    #[test]
    fn paused_outranks_connected_and_connected_outranks_offline() {
        let paused = Pairing {
            paused: true,
            ..testing::agent("b", 2)
        };
        let rows =
            AgentRow::from_pairings(&[testing::agent("a", 1), paused, testing::agent("c", 3)]);
        let mut channels = HashMap::new();
        for id in ["a", "b"] {
            channels.insert(
                id.to_string(),
                ChannelState {
                    connected: true,
                    ..Default::default()
                },
            );
        }
        let snap = snapshot(&rows, &mut channels, 100);
        let states: Vec<&str> = snap["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["state"].as_str().unwrap())
            .collect();
        assert_eq!(states, ["connected", "paused", "offline"]);
    }

    #[test]
    fn since_moves_only_when_the_state_does() {
        let rows = AgentRow::from_pairings(&[testing::agent("a", 1)]);
        let mut channels = HashMap::new();
        assert_eq!(
            snapshot(&rows, &mut channels, 100)["agents"][0]["since"],
            100
        );
        assert_eq!(
            snapshot(&rows, &mut channels, 150)["agents"][0]["since"],
            100,
            "still offline"
        );
        channels.get_mut("a").unwrap().connected = true;
        assert_eq!(
            snapshot(&rows, &mut channels, 200)["agents"][0]["since"],
            200
        );
        channels
            .get_mut("a")
            .unwrap()
            .tools
            .push((1, "browser_click".into()));
        assert_eq!(
            snapshot(&rows, &mut channels, 250)["agents"][0]["since"],
            200,
            "a running tool is not a new state"
        );
    }

    #[test]
    fn an_open_question_names_its_own_tool() {
        let rows = AgentRow::from_pairings(&[testing::agent("a", 1)]);
        let mut channels = HashMap::new();
        channels.insert(
            "a".to_string(),
            ChannelState {
                connected: true,
                tools: vec![(1, "browser_fill".into()), (2, "browser_screenshot".into())],
                approvals: vec![PendingApproval {
                    id: 1,
                    tool: "browser_fill".into(),
                    summary: "Fill a form field in tab 3".into(),
                    started_at: 90,
                }],
                shown: None,
                task: None,
            },
        );
        let snap = snapshot(&rows, &mut channels, 100);
        assert_eq!(snap["agents"][0]["tool"], "browser_fill");
        assert_eq!(
            snap["agents"][0]["approval"],
            json!({ "summary": "Fill a form field in tab 3", "started_at": 90 })
        );
        channels.get_mut("a").unwrap().approvals.clear();
        assert_eq!(
            snapshot(&rows, &mut channels, 100)["agents"][0]["tool"],
            "browser_screenshot",
            "with no question open, the latest call is shown"
        );
    }

    #[test]
    fn the_titles_are_exact_and_take_the_label() {
        use ApprovalOutcome::*;
        assert_eq!(Approved.title(None), "Muse approved");
        assert_eq!(Declined.title(None), "Muse declined");
        assert_eq!(
            TimedOut.title(None),
            "Muse did not answer within 60 s — denied"
        );
        assert_eq!(
            Cancelled.title(None),
            "Approval cancelled — Muse disconnected"
        );
        assert_eq!(Approved.title(Some("Claude")), "Claude approved");
        assert_eq!(Declined.title(Some("Claude")), "Claude declined");
        assert_eq!(
            TimedOut.title(Some("Claude")),
            "Claude did not answer within 60 s — denied"
        );
        assert_eq!(
            Cancelled.title(Some("Claude")),
            "Approval cancelled — Claude disconnected"
        );
        assert_eq!(
            Stopped(StopReason::TierLowered).title(None),
            "Muse approved — stopped: the agent's tier was lowered"
        );
        assert_eq!(
            Stopped(StopReason::TierLowered).title(Some("Claude")),
            "Claude approved — stopped: the agent's tier was lowered"
        );
        assert_eq!(
            Stopped(StopReason::BrowserChanged).title(Some("Claude")),
            "Claude approved — stopped: the browser changed"
        );
        assert_eq!(Stopped(StopReason::TierLowered).as_str(), "stopped");
        assert_eq!(Stopped(StopReason::BrowserChanged).as_str(), "stopped");
        assert_eq!(
            Declined.title(Some("  ")),
            "Muse declined",
            "a blank label is no label"
        );
    }

    #[test]
    fn every_answer_maps_to_one_outcome() {
        for (answer, outcome) in [
            (Approval::Approved, "approved"),
            (Approval::Declined, "declined"),
            (Approval::TimedOut, "timed_out"),
            (Approval::Unsupported, "cancelled"),
            (Approval::Failed, "cancelled"),
        ] {
            assert_eq!(ApprovalOutcome::of(answer).as_str(), outcome, "{answer:?}");
        }
    }

    #[test]
    fn an_approval_notification_has_the_contract_shape() {
        let summary = "Click on element '#go' in tab 1";
        let n = approval_notification(ApprovalOutcome::Declined, Some("Claude"), summary);
        assert!(uuid::Uuid::parse_str(n["event_id"].as_str().unwrap()).is_ok());
        let mut rest = n.clone();
        rest.as_object_mut().unwrap().remove("event_id");
        assert_eq!(
            rest,
            json!({ "title": "Claude declined", "body": summary,
                    "source": "agent_approval", "outcome": "declined" })
        );
        let again = approval_notification(ApprovalOutcome::Declined, Some("Claude"), summary);
        assert_ne!(
            again["event_id"], n["event_id"],
            "every notice has its own id"
        );
    }

    #[test]
    fn a_delegated_task_is_shown_on_its_agent_and_announced_on_its_own() {
        let (status, seen, _clock) = testing::watched(100);
        status.set_pairings(&[testing::agent("a", 1), testing::agent("b", 2)]);
        let task = json!({"id": "t1", "state": "running"});
        status.set_task("a", Some(task.clone()));
        let snap = seen.statuses().last().cloned().unwrap();
        assert_eq!(snap["agents"][0]["task"], task);
        assert_eq!(snap["agents"][1]["task"], Value::Null);
        let before = seen.statuses().len();
        status.set_task("a", Some(task));
        assert_eq!(seen.statuses().len(), before, "an unchanged task publishes nothing");
        status.set_task("a", None);
        assert_eq!(seen.statuses().last().unwrap()["agents"][0]["task"], Value::Null);
        let before = seen.statuses().len();
        status.set_task("gone", None);
        assert_eq!(seen.statuses().len(), before, "clearing an unknown channel publishes nothing");
        status.notify(json!({"source": "agent_task"}));
        assert_eq!(seen.notifications(), vec![json!({"source": "agent_task"})]);
    }
}
