//! The task engine (M3 §2, §5): delegated tasks from start to result.
//!
//! One engine per daemon (`super::install`). It never runs a turn itself. A
//! task starts by injecting a `chat_message` (mode `agent`) into the daemon's
//! message loop under the live sidebar connection, so the turn is the same
//! function, with the same gates, Canvas tabs and dialogs, as a person's own
//! (decision C4). It is followed through the chat tap (`observe`), the
//! browser-response hook (`observe_browser_response`) and a guard around the
//! turn (`begin_turn` → `TurnGuard`), and its approvals are answered by taking
//! the waiting entry out of `crate::pending` — the same registry every other
//! answerer takes from, which is what makes the first answer the only one.
//!
//! State is in memory: a daemon restart forgets every task (their sessions
//! stay in the database), and `nevoflux_task_wait` then answers
//! `task_not_found`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use nevoflux_protocol::chat::{ChatMessage, SidebarMessage, StopGeneration};
use nevoflux_storage::Database;
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};

use super::answer::{answer_for, response_for, Answer, Choice};
use super::limits::{
    DELEGATED_HOLD, RUNNING_PER_PAIRING, SCREENSHOT_WAIT, STARTS_PER_WINDOW, START_WINDOW_SECS,
    TERMINAL_RETENTION_SECS,
};
use super::record::{Approval, Conclusion, StepEvent, TaskRecord, TaskState};
use crate::pending::Pending;
use crate::remote::agent_status::AgentStatus;
use crate::remote::mcp_tools::AgentError;
use crate::remote::pairing::Pairing;
use crate::wasm::services::{BrowserRequest, BrowserResponse};

/// What the engine needs from the daemon. `host::DaemonTaskHost` is the real
/// one; tests use `testing::FakeHost`.
#[async_trait]
pub trait TaskHost: Send + Sync {
    /// The live sidebar connection a turn is injected under, if any.
    fn sidebar(&self) -> Option<String>;
    /// Whether `proxy_id` is still connected.
    fn is_connected(&self, proxy_id: &str) -> bool;
    fn database(&self) -> Arc<Database>;
    /// The agent pairing on control channel `channel`, read now.
    fn pairing(&self, channel: &str) -> Option<Pairing>;
    /// Where turns wait for answers (`crate::pending::shared()`).
    fn pending(&self) -> Arc<Pending>;
    /// Send a chat frame to the connection `proxy_id`. It must leave through
    /// the chat writer, past the chat tap (`super::observe`), behind every
    /// frame already queued: the engine marks the end of a turn this way and
    /// learns that the turn's last frame has been observed when the marker
    /// comes back through the tap.
    async fn send(&self, proxy_id: &str, payload: Value);
    /// Put a message into the daemon's message loop as if `proxy_id` sent it.
    async fn inject(&self, proxy_id: &str, payload: Value);
    /// Send a browser request and wait for its answer; `None` when it could
    /// not be sent or nothing came back. The engine bounds the wait itself
    /// (`SCREENSHOT_WAIT`).
    async fn browser(&self, request: BrowserRequest) -> Option<BrowserResponse>;
}

/// Where `notifications/progress` for one wait go.
#[async_trait]
pub trait ProgressSink: Send + Sync {
    /// `count` is 1, 2, … within this wait (contract §2.2).
    async fn progress(&self, count: u64, message: String);
}

/// What a wait returns: the TaskState JSON and, once per MCP session, the
/// result image.
#[derive(Debug)]
pub struct Waited {
    pub state: Value,
    pub image: Option<String>,
}

/// Who is answering an approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answerer {
    /// The paired agent on this control channel, over MCP.
    Agent(String),
    /// The person, from the status bar.
    Desktop,
}

/// Why an answer was not given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveError {
    TaskNotFound,
    ApprovalNotFound,
    Invalid(&'static str),
    /// A must-approve approval, and the pairing does not let its agent
    /// answer those (contract v1.2 R39).
    DesktopOnly,
}

/// Whether a turn that is starting should run.
pub enum TurnStart {
    Run(TurnGuard),
    /// Its task was cancelled before the turn began.
    Skip,
}

/// Held for the length of a turn; dropping it tells the engine the turn
/// ended, however it ended. It names the task the turn is, not the session:
/// a later task can be running in the same session by the time it drops.
pub struct TurnGuard {
    watched: Option<(Arc<TaskEngine>, String)>,
}

impl TurnGuard {
    /// A guard for a turn no task is waiting on.
    pub fn untracked() -> Self {
        Self { watched: None }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Some((engine, task_id)) = self.watched.take() {
            engine.turn_ended(&task_id, std::thread::panicking());
        }
    }
}

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, TaskRecord>,
    by_session: HashMap<String, String>,
    /// Successful starts per pairing, unix seconds, oldest first.
    starts: HashMap<String, VecDeque<i64>>,
    /// Next step event per (MCP session, task).
    cursors: HashMap<(String, String), u64>,
    /// (MCP session, task) pairs that were sent the image.
    shown: HashSet<(String, String)>,
    /// Tasks whose turn ended and whose end marker is on its way through the
    /// chat tap: `task -> (session, fired when it arrives)`. Until it arrives,
    /// the session's frames are still that turn's.
    ending: HashMap<String, (String, oneshot::Sender<()>)>,
    /// Tasks whose chat message was injected and whose turn has not ended
    /// (or been skipped). At most one per session: `start` refuses to
    /// continue a session while its previous task's turn is pending.
    turn_pending: HashSet<String>,
}

/// How long a turn's end waits for its marker to come back through the chat
/// tap before the task is concluded anyway.
const TURN_END_GRACE: Duration = Duration::from_secs(5);

/// Every delegated task the daemon knows about.
pub struct TaskEngine {
    inner: Mutex<Inner>,
    host: Arc<dyn TaskHost>,
    status: Arc<AgentStatus>,
    changed: watch::Sender<u64>,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl TaskEngine {
    pub fn new(host: Arc<dyn TaskHost>, status: Arc<AgentStatus>) -> Self {
        Self {
            inner: Mutex::default(),
            host,
            status,
            changed: watch::channel(0).0,
            clock: Box::new(unix_now),
        }
    }

    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Every critical section is a map edit; none awaits.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Wake every waiter: something about some task changed.
    fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Forget tasks that ended more than an hour ago.
    fn purge(inner: &mut Inner, now: i64) {
        let gone: Vec<String> = inner
            .tasks
            .iter()
            .filter(|(_, t)| {
                t.ended_at
                    .is_some_and(|e| now - e >= TERMINAL_RETENTION_SECS)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            if let Some(task) = inner.tasks.remove(&id) {
                if inner.by_session.get(&task.session) == Some(&id) {
                    inner.by_session.remove(&task.session);
                }
            }
            inner.turn_pending.remove(&id);
            inner.cursors.retain(|(_, task), _| *task != id);
            inner.shown.retain(|(_, task)| *task != id);
        }
    }

    /// The task a frame of `session` belongs to: the one whose turn ended
    /// there and whose end marker has not come back yet, else the session's
    /// current task. Frames are tapped in the order they were queued, so a
    /// turn's late frames arrive before its marker.
    fn task_of(inner: &Inner, session: &str) -> Option<String> {
        inner
            .ending
            .iter()
            .find(|(_, (s, _))| s == session)
            .map(|(id, _)| id.clone())
            .or_else(|| inner.by_session.get(session).cloned())
    }

    fn owned<'a>(inner: &'a Inner, pairing: &str, task_id: &str) -> Option<&'a TaskRecord> {
        inner.tasks.get(task_id).filter(|t| t.pairing == pairing)
    }

    /// Busy, then the hourly window (contract §1.1).
    fn check_admission(inner: &mut Inner, pairing: &str, now: i64) -> Result<(), AgentError> {
        let running: Vec<String> = inner
            .tasks
            .values()
            .filter(|t| t.pairing == pairing && !t.state.is_terminal())
            .map(|t| t.task_id.clone())
            .collect();
        if running.len() >= RUNNING_PER_PAIRING {
            return Err(AgentError::TaskBusy {
                task_id: running[0].clone(),
            });
        }
        let window = inner.starts.entry(pairing.to_string()).or_default();
        while window
            .front()
            .is_some_and(|t| now - *t >= START_WINDOW_SECS)
        {
            window.pop_front();
        }
        if window.len() >= STARTS_PER_WINDOW {
            let oldest = *window.front().expect("a full window has a first start");
            return Err(AgentError::RateLimited {
                retry_after: (oldest + START_WINDOW_SECS - now).max(1) as u64,
            });
        }
        Ok(())
    }

    /// `nevoflux_task_start` (spec §3.2): a new or continued delegated
    /// session, an Agent-mode turn injected under the sidebar connection.
    pub async fn start(
        &self,
        pairing: &str,
        prompt: &str,
        session: Option<&str>,
    ) -> Result<Value, AgentError> {
        let now = self.now();
        if self.host.pairing(pairing).is_none_or(|p| p.paused) {
            return Err(AgentError::Paused);
        }
        {
            let mut inner = self.lock();
            Self::purge(&mut inner, now);
            Self::check_admission(&mut inner, pairing, now)?;
        }
        let db = self.host.database();
        if let Some(s) = session {
            if super::origin::delegated_pairing(&db, s).as_deref() != Some(pairing) {
                return Err(AgentError::SessionNotFound {
                    session: s.to_string(),
                });
            }
        }
        let Some(proxy) = self.host.sidebar() else {
            return Err(AgentError::BrowserUnavailable);
        };
        let session_id = match session {
            Some(s) => s.to_string(),
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                super::origin::create_delegated(&db, &id, pairing)
                    .map_err(|detail| AgentError::ToolFailed { detail })?;
                id
            }
        };
        let task_id = uuid::Uuid::new_v4().to_string();
        {
            let mut inner = self.lock();
            // Again, under the lock that files the task: two starts racing
            // past the first check must not both run. (The loser may leave an
            // empty session behind; `session.list` hides empty sessions.)
            Self::check_admission(&mut inner, pairing, now)?;
            // The previous task's turn has not ended (or been skipped) yet:
            // a turn started now would be taken for this task's.
            if let Some(previous) = inner.by_session.get(&session_id) {
                if inner.turn_pending.contains(previous) {
                    return Err(AgentError::TaskBusy {
                        task_id: previous.clone(),
                    });
                }
            }
            inner
                .starts
                .entry(pairing.to_string())
                .or_default()
                .push_back(now);
            inner.by_session.insert(session_id.clone(), task_id.clone());
            inner.turn_pending.insert(task_id.clone());
            inner.tasks.insert(
                task_id.clone(),
                TaskRecord::new(&task_id, pairing, &session_id, prompt, &proxy, now),
            );
        }
        self.publish_status(pairing);
        // Before the turn: the sidebar must know the session is delegated by
        // the time its first frame (a Canvas, even) arrives (R19).
        self.host
            .send(&proxy, session_origin_frame(&session_id, pairing))
            .await;
        self.host
            .inject(&proxy, chat_frame(&session_id, prompt))
            .await;
        tracing::info!(
            target: "remote",
            task = %task_id,
            channel = %crate::remote::agent_backend::short_id(pairing),
            "delegated task started"
        );
        self.bump();
        Ok(start_result(&task_id, &session_id))
    }

    /// One chat frame on its way to a sidebar (the M2 tap). Synchronous and
    /// cheap: a frame of a session no task runs in costs one map lookup.
    pub fn observe(&self, payload: &Value) {
        let Some(session) = payload
            .get("payload")
            .and_then(|p| p.get("session_id"))
            .and_then(Value::as_str)
        else {
            return;
        };
        if payload.get("type").and_then(Value::as_str) == Some("session_origin") {
            // The marker `turn_ended` sent: every frame its turn queued before
            // it ended has been observed by now.
            let mut inner = self.lock();
            let ended = inner
                .ending
                .iter()
                .find(|(_, (s, _))| s == session)
                .map(|(id, _)| id.clone());
            if let Some((_, arrived)) = ended.and_then(|id| inner.ending.remove(&id)) {
                let _ = arrived.send(());
            }
            return;
        }
        let now = self.now();
        let (task_id, pairing, seen, stop) = {
            let mut inner = self.lock();
            let Some(task_id) = Self::task_of(&inner, session) else {
                return;
            };
            let Some(task) = inner.tasks.get_mut(&task_id) else {
                return;
            };
            let seen = task.observe(payload, now);
            let mut stop = None;
            if seen.timed_out {
                // Nobody answered in 30 minutes: the turn has been refused
                // and would carry on unattended. The task ends here.
                task.fail(
                    "approval_timeout",
                    super::record::APPROVAL_TIMEOUT_MESSAGE,
                    now,
                );
                stop = Some((task.session.clone(), task.proxy_id.clone()));
            }
            (task_id, task.pairing.clone(), seen, stop)
        };
        if let Some((session, proxy)) = stop {
            self.settled(&task_id);
            self.spawn_inject(proxy, stop_frame(&session));
        }
        if seen.changed || seen.timed_out {
            self.publish_status(&pairing);
            self.bump();
        }
    }

    /// A browser call of `session` was answered: where it acted, and the
    /// tab it opened if it opened one.
    pub fn observe_browser_response(&self, session: &str, result: Option<&Value>) {
        let Some(tab) = result.and_then(|r| r.get("tab_id")).and_then(Value::as_i64) else {
            return;
        };
        let new_tab = result
            .and_then(|r| r.get("new_tab"))
            .and_then(Value::as_bool)
            == Some(true);
        let mut inner = self.lock();
        let Some(id) = Self::task_of(&inner, session) else {
            return;
        };
        if let Some(task) = inner.tasks.get_mut(&id) {
            task.note_tab(tab, new_tab);
        }
    }

    /// A turn of `session` is starting. It is the turn of the session's task
    /// whose turn is pending (there is at most one); any other turn there is
    /// not watched.
    pub fn begin_turn(self: &Arc<Self>, session: &str) -> TurnStart {
        let mut inner = self.lock();
        let Some(id) = inner
            .by_session
            .get(session)
            .filter(|id| inner.turn_pending.contains(*id))
            .cloned()
        else {
            return TurnStart::Run(TurnGuard::untracked());
        };
        let Some(task) = inner.tasks.get_mut(&id) else {
            inner.turn_pending.remove(&id);
            return TurnStart::Run(TurnGuard::untracked());
        };
        if task.turn_started {
            return TurnStart::Run(TurnGuard::untracked());
        }
        task.turn_started = true;
        let state = task.state;
        if state == TaskState::Cancelled {
            inner.turn_pending.remove(&id);
            return TurnStart::Skip;
        }
        if state.is_terminal() {
            inner.turn_pending.remove(&id);
            return TurnStart::Run(TurnGuard::untracked());
        }
        TurnStart::Run(TurnGuard {
            watched: Some((self.clone(), id)),
        })
    }

    /// The turn of `task_id` ended. Its last frames may still be queued for
    /// the chat tap (the turn hands them to the writer and moves on), and the
    /// record stops reading frames once it concludes, so the conclusion waits
    /// for a marker sent behind them — or `TURN_END_GRACE`, should it never
    /// come back. A turn that unwound, or whose sidebar is gone, has nothing
    /// more coming and concludes now.
    fn turn_ended(self: &Arc<Self>, task_id: &str, panicking: bool) {
        let Some((session, proxy, pairing)) = self.lock().tasks.get(task_id).map(|task| {
            (
                task.session.clone(),
                task.proxy_id.clone(),
                task.pairing.clone(),
            )
        }) else {
            self.lock().turn_pending.remove(task_id);
            return;
        };
        let task_id = task_id.to_string();
        let rt = match tokio::runtime::Handle::try_current() {
            Ok(rt) if !panicking && self.host.is_connected(&proxy) => rt,
            _ => {
                self.lock().turn_pending.remove(&task_id);
                return self.conclude_turn(&task_id, panicking);
            }
        };
        let (arrived, marker) = oneshot::channel();
        {
            // One step: the session's frames stay this turn's from the moment
            // another task may start there.
            let mut inner = self.lock();
            inner.turn_pending.remove(&task_id);
            inner
                .ending
                .insert(task_id.clone(), (session.clone(), arrived));
        }
        let engine = self.clone();
        rt.spawn(async move {
            engine
                .host
                .send(&proxy, session_origin_frame(&session, &pairing))
                .await;
            let _ = tokio::time::timeout(TURN_END_GRACE, marker).await;
            engine.lock().ending.remove(&task_id);
            engine.conclude_turn(&task_id, false);
        });
    }

    fn conclude_turn(self: &Arc<Self>, task_id: &str, panicking: bool) {
        let now = self.now();
        let Some(proxy) = self.lock().tasks.get(task_id).map(|t| t.proxy_id.clone()) else {
            return;
        };
        let connected = self.host.is_connected(&proxy);
        let (pairing, conclusion) = {
            let mut inner = self.lock();
            let Some(task) = inner.tasks.get_mut(task_id) else {
                return;
            };
            (
                task.pairing.clone(),
                task.conclude(connected, panicking, now),
            )
        };
        match conclusion {
            Conclusion::Ended => {
                self.settled(task_id);
                self.publish_status(&pairing);
                self.bump();
            }
            Conclusion::Capture => {
                self.publish_status(&pairing);
                match tokio::runtime::Handle::try_current() {
                    Ok(rt) => {
                        let engine = self.clone();
                        let task_id = task_id.to_string();
                        rt.spawn(async move { engine.capture(task_id).await });
                    }
                    Err(_) => self.complete(task_id, Err("capture_failed".to_string())),
                }
            }
        }
    }

    /// Take the result screenshot, then report the task done (R12: `done`
    /// only once the attempt ended, so the first `done` wait carries it).
    async fn capture(self: Arc<Self>, task_id: String) {
        let plan = {
            let inner = self.lock();
            inner.tasks.get(&task_id).map(|t| {
                (
                    t.session.clone(),
                    super::screenshot::choose(t.produced.as_ref(), t.last_tab),
                )
            })
        };
        let Some((session, target)) = plan else {
            return;
        };
        let shot = match (target, self.host.sidebar()) {
            (None, _) => Err("no_tab"),
            (Some(target), Some(proxy)) if self.host.is_connected(&proxy) => {
                let request = super::screenshot::request(&target, &session, &proxy);
                let answer = tokio::time::timeout(SCREENSHOT_WAIT, self.host.browser(request))
                    .await
                    .ok()
                    .flatten();
                if answer.is_none() && !self.host.is_connected(&proxy) {
                    // Not a slow answer: there was nobody to send it to.
                    Err("browser_unavailable")
                } else {
                    super::screenshot::outcome(answer)
                }
            }
            _ => Err("browser_unavailable"),
        };
        self.complete(&task_id, shot.map_err(str::to_string));
    }

    fn complete(&self, task_id: &str, shot: Result<String, String>) {
        let now = self.now();
        let pairing = {
            let mut inner = self.lock();
            let Some(task) = inner.tasks.get_mut(task_id) else {
                return;
            };
            task.finish(shot, now);
            task.pairing.clone()
        };
        self.settled(task_id);
        self.publish_status(&pairing);
        self.bump();
    }

    /// `nevoflux_task_wait` (spec §3.3): returns at once while an approval is
    /// open or the task has ended, otherwise when one of those happens or at
    /// `timeout`; every step event and new approval meanwhile goes to
    /// `progress`.
    pub async fn wait(
        &self,
        pairing: &str,
        mcp_session: &str,
        task_id: &str,
        timeout: Duration,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<Waited, AgentError> {
        let not_found = || AgentError::TaskNotFound {
            task_id: task_id.to_string(),
        };
        let key = (mcp_session.to_string(), task_id.to_string());
        let deadline = tokio::time::Instant::now() + timeout;
        let mut changes = self.changed.subscribe();
        let mut told = {
            let mut inner = self.lock();
            Self::purge(&mut inner, self.now());
            if Self::owned(&inner, pairing, task_id).is_none() {
                return Err(not_found());
            }
            inner.cursors.get(&key).copied().unwrap_or(0)
        };
        let mut told_approvals: HashSet<String> = HashSet::new();
        let mut sent = 0u64;
        loop {
            let (messages, settled) = {
                let inner = self.lock();
                let task = Self::owned(&inner, pairing, task_id).ok_or_else(not_found)?;
                let mut messages = Vec::new();
                for event in task.events_since(told).0 {
                    messages.push(progress_message(event));
                    told = event.seq + 1;
                }
                for approval in &task.approvals {
                    if told_approvals.insert(approval.approval_id.clone()) {
                        messages.push(approval_progress(approval));
                    }
                }
                (
                    messages,
                    task.state.is_terminal() || task.state == TaskState::AwaitingApproval,
                )
            };
            if let Some(sink) = progress {
                for message in messages {
                    sent += 1;
                    sink.progress(sent, message).await;
                }
            }
            if settled {
                break;
            }
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        let now = self.now();
        // Read before the lock: the store is a file.
        let guarded = self.muse_may_approve_guarded(pairing);
        let mut inner = self.lock();
        let cursor = inner.cursors.get(&key).copied().unwrap_or(0);
        let (state, next, image) = {
            let task = Self::owned(&inner, pairing, task_id).ok_or_else(not_found)?;
            let image = if task.state == TaskState::Done {
                task.screenshot.clone()
            } else {
                None
            };
            (task.wait_json_for(cursor, now, guarded), task.next_seq(), image)
        };
        inner.cursors.insert(key.clone(), next);
        let image = match image {
            Some(data) if inner.shown.insert(key) => Some(data),
            _ => None,
        };
        Ok(Waited { state, image })
    }

    /// `nevoflux_task_cancel`, `remote.task_cancel`, pause and unpair (spec
    /// §3.6): `cancelled` at once, the turn stopped through the same
    /// `stop_generation` the stop button sends — which also releases what the
    /// turn waits on. Idempotent; a task already over keeps its state.
    /// `pairing` is `None` for the desktop.
    pub async fn cancel(&self, pairing: Option<&str>, task_id: &str) -> Result<Value, AgentError> {
        let now = self.now();
        let (session, proxy, owner) = {
            let mut inner = self.lock();
            Self::purge(&mut inner, now);
            let task = inner
                .tasks
                .get_mut(task_id)
                .filter(|t| pairing.is_none_or(|p| t.pairing == p))
                .ok_or_else(|| AgentError::TaskNotFound {
                    task_id: task_id.to_string(),
                })?;
            if task.state.is_terminal() {
                return Ok(cancel_result(task_id, task.state));
            }
            task.cancel(now);
            (
                task.session.clone(),
                task.proxy_id.clone(),
                task.pairing.clone(),
            )
        };
        let proxy = self.host.sidebar().unwrap_or(proxy);
        self.host.inject(&proxy, stop_frame(&session)).await;
        self.settled(task_id);
        self.publish_status(&owner);
        self.bump();
        Ok(cancel_result(task_id, TaskState::Cancelled))
    }

    /// `nevoflux_task_approve` and `remote.task_answer` (spec §3.5, R23): the
    /// waiting entry is taken out of `crate::pending` — the registry the
    /// desktop's own answer, the sweeper and a stop take from too — so exactly
    /// one answer reaches the turn and every later one gets
    /// `ApprovalNotFound`. The connection showing the dialog is told it is
    /// settled (R28).
    pub async fn approve(
        &self,
        by: Answerer,
        task_id: &str,
        approval_id: &str,
        choice: Choice,
        text: Option<&str>,
    ) -> Result<TaskState, ApproveError> {
        let now = self.now();
        let (approval, session, pairing) = {
            let mut inner = self.lock();
            Self::purge(&mut inner, now);
            let task = inner
                .tasks
                .get(task_id)
                .filter(|t| match &by {
                    Answerer::Agent(pairing) => t.pairing == *pairing,
                    Answerer::Desktop => true,
                })
                .ok_or(ApproveError::TaskNotFound)?;
            let approval = task
                .approvals
                .iter()
                .find(|a| a.approval_id == approval_id)
                .cloned()
                .ok_or(ApproveError::ApprovalNotFound)?;
            (approval, task.session.clone(), task.pairing.clone())
        };
        // Contract v1.2 R39: read at the answer, outside the lock.
        if approval.must_approve
            && matches!(by, Answerer::Agent(_))
            && !self.muse_may_approve_guarded(&pairing)
        {
            return Err(ApproveError::DesktopOnly);
        }
        if matches!(by, Answerer::Agent(_)) && choice == Choice::AllowAlways {
            // Decision C8: "always" leaves a permanent rule; only the person
            // may. `Choice::parse_agent` never yields it — this holds anyway.
            return Err(ApproveError::Invalid(super::answer::NOT_OFFERED));
        }
        let answer = answer_for(&approval, choice, text).map_err(ApproveError::Invalid)?;
        let pending = self.host.pending();
        let delivered = match answer {
            Answer::Plan(response) => {
                let plan = pending
                    .take_plan(&session)
                    .ok_or(ApproveError::ApprovalNotFound)?;
                crate::pending::answer_plan(&session, plan, response)
            }
            other => {
                let entry = pending
                    .take_browser(&approval.approval_id)
                    .ok_or(ApproveError::ApprovalNotFound)?;
                let response = response_for(&approval.approval_id, other);
                crate::pending::answer_browser(&approval.approval_id, entry, response)
            }
        };
        // When the turn's own timer ran out first nothing was answered, and
        // the dialog is told it timed out.
        let (Ok(announcement) | Err(announcement)) = &delivered;
        self.host
            .send(&announcement.proxy_id, announcement.frame.clone())
            .await;
        if delivered.is_err() {
            return Err(ApproveError::ApprovalNotFound);
        }
        let state = {
            let mut inner = self.lock();
            match inner.tasks.get_mut(task_id) {
                Some(task) => {
                    task.remove_approval(approval_id);
                    task.state
                }
                None => TaskState::Running,
            }
        };
        tracing::info!(
            target: "remote",
            task = %task_id,
            kind = approval.kind.as_str(),
            choice = choice.as_str(),
            by = if matches!(by, Answerer::Desktop) { "desktop" } else { "agent" },
            must_approve = approval.must_approve,
            "delegated task approval answered"
        );
        self.publish_status(&pairing);
        self.bump();
        Ok(state)
    }

    /// Contract v1.2 R39: whether `pairing` lets its agent answer
    /// must-approve approvals. Read from the store now, never cached; a
    /// pairing that cannot be read or is gone reads as no.
    fn muse_may_approve_guarded(&self, pairing: &str) -> bool {
        self.host
            .pairing(pairing)
            .is_some_and(|p| p.muse_may_approve_guarded)
    }

    /// Cancel every running task of `pairing` (it was paused or unpaired).
    pub async fn cancel_for_pairing(&self, pairing: &str) {
        let running: Vec<String> = {
            let inner = self.lock();
            inner
                .tasks
                .values()
                .filter(|t| t.pairing == pairing && !t.state.is_terminal())
                .map(|t| t.task_id.clone())
                .collect()
        };
        for id in running {
            let _ = self.cancel(Some(pairing), &id).await;
        }
    }

    /// How long a question from `session` is held: thirty minutes while a
    /// task runs there (§3.8), else `None` (the caller's own default).
    pub fn hold_for(&self, session: &str) -> Option<Duration> {
        let inner = self.lock();
        let id = inner.by_session.get(session)?;
        let task = inner.tasks.get(id)?;
        (!task.state.is_terminal()).then_some(DELEGATED_HOLD)
    }

    fn spawn_inject(&self, proxy: String, payload: Value) {
        let host = self.host.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { host.inject(&proxy, payload).await });
        }
    }

    /// Republish this pairing's `task` on the status snapshot.
    fn publish_status(&self, pairing: &str) {
        let task = {
            let inner = self.lock();
            inner
                .tasks
                .values()
                .find(|t| t.pairing == pairing && !t.state.is_terminal())
                .map(TaskRecord::status_json)
        };
        self.status.set_task(pairing, task);
    }

    /// The one end notice and audit line for a task that is over.
    fn settled(&self, task_id: &str) {
        let ended = {
            let mut inner = self.lock();
            let Some(task) = inner.tasks.get_mut(task_id) else {
                return;
            };
            if !task.state.is_terminal() || task.notified {
                return;
            }
            task.notified = true;
            (
                task.pairing.clone(),
                task.state,
                task.summary.clone(),
                task.session.clone(),
                task.error.as_ref().map(|(code, _)| code.clone()),
                task.ended_at.unwrap_or(task.started_at) - task.started_at,
            )
        };
        let (pairing, state, summary, session, code, secs) = ended;
        let label = self.host.pairing(&pairing).and_then(|p| p.label);
        self.status.notify(task_notification(
            task_id,
            &session,
            &summary,
            state,
            label.as_deref(),
        ));
        tracing::info!(
            target: "remote",
            task = %task_id,
            outcome = state.as_str(),
            code = code.as_deref().unwrap_or(""),
            secs,
            "delegated task ended"
        );
    }
}

/// `source` of a task's end notice (contract R24).
pub const TASK_SOURCE: &str = "agent_task";

/// The end notice (R24): titles name the agent by its label when it has one.
pub fn task_notification(
    task_id: &str,
    session_id: &str,
    summary: &str,
    state: TaskState,
    label: Option<&str>,
) -> Value {
    let who = label
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or("Muse");
    let (title, outcome) = match state {
        TaskState::Done => (format!("Task from {who} finished"), "done"),
        TaskState::Failed => (format!("Task from {who} failed"), "failed"),
        _ => (format!("Task from {who} was cancelled"), "cancelled"),
    };
    json!({
        "event_id": uuid::Uuid::new_v4().to_string(),
        "title": title,
        "body": summary,
        "source": TASK_SOURCE,
        "outcome": outcome,
        "task_id": task_id,
        "session_id": session_id,
    })
}

/// R19: tells the sidebar connection a session is a paired agent's.
pub fn session_origin_frame(session_id: &str, pairing: &str) -> Value {
    json!({
        "type": "session_origin",
        "payload": { "session_id": session_id, "origin": super::origin::muse_origin(pairing) }
    })
}

/// The turn itself: an Agent-mode chat message (decision C5).
pub fn chat_frame(session_id: &str, prompt: &str) -> Value {
    serde_json::to_value(SidebarMessage::ChatMessage(ChatMessage {
        session_id: session_id.to_string(),
        message_id: uuid::Uuid::new_v4().to_string(),
        text: prompt.to_string(),
        mode: Some("agent".to_string()),
        attachments: Vec::new(),
        tab_id: None,
        tab_ids: Vec::new(),
    }))
    .unwrap_or_default()
}

/// The stop button's own message.
pub fn stop_frame(session_id: &str) -> Value {
    serde_json::to_value(SidebarMessage::StopGeneration(StopGeneration {
        session_id: session_id.to_string(),
    }))
    .unwrap_or_default()
}

pub fn start_result(task_id: &str, session: &str) -> Value {
    json!({ "task_id": task_id, "session": session, "state": "running" })
}

pub fn cancel_result(task_id: &str, state: TaskState) -> Value {
    json!({ "task_id": task_id, "state": state.as_str() })
}

pub fn approve_result(state: TaskState) -> Value {
    json!({ "ok": true, "state": state.as_str() })
}

/// R17 messages for a step event.
pub fn progress_message(event: &StepEvent) -> String {
    match event.status {
        "started" => format!("Running {}", event.tool),
        "ok" => format!("{} finished", event.tool),
        _ => format!("{} failed", event.tool),
    }
}

/// R17 message for a new approval.
pub fn approval_progress(approval: &Approval) -> String {
    format!("Waiting for approval: {}", approval.summary)
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
    use crate::remote::agent_status::testing as status_testing;
    use std::sync::atomic::{AtomicI64, Ordering};

    pub(crate) const CHANNEL: &str = "chan-1";
    pub(crate) const SIDEBAR: &str = "sidebar-1";
    /// 2026-10-14T17:46:40Z.
    pub(crate) const T0: i64 = 1_792_000_000;

    /// A daemon without a daemon: records what the engine sends and injects.
    pub(crate) struct FakeHost {
        pub(crate) db: Arc<Database>,
        pub(crate) sidebar: Mutex<Option<String>>,
        pub(crate) connected: Mutex<bool>,
        pub(crate) pairings: Mutex<Vec<Pairing>>,
        /// `("send" | "inject", proxy, payload)`, in order.
        pub(crate) log: Mutex<Vec<(&'static str, String, Value)>>,
        pub(crate) shots: Mutex<Vec<BrowserRequest>>,
        pub(crate) shot_answer: Mutex<Option<BrowserResponse>>,
        /// The sidebar disconnects while the screenshot is asked for.
        pub(crate) gone_on_shot: Mutex<bool>,
        pub(crate) pending: Arc<Pending>,
        /// Where `send` hands every frame, as the daemon's chat tap does:
        /// the engine itself, unless a test cuts it.
        pub(crate) tap: Mutex<Option<std::sync::Weak<TaskEngine>>>,
    }

    impl FakeHost {
        pub(crate) fn sent(&self) -> Vec<(String, Value)> {
            self.of("send")
        }
        pub(crate) fn injected(&self) -> Vec<(String, Value)> {
            self.of("inject")
        }
        fn of(&self, kind: &str) -> Vec<(String, Value)> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _, _)| *k == kind)
                .map(|(_, p, v)| (p.clone(), v.clone()))
                .collect()
        }
    }

    #[async_trait]
    impl TaskHost for FakeHost {
        fn sidebar(&self) -> Option<String> {
            self.sidebar.lock().unwrap().clone()
        }
        fn is_connected(&self, _proxy_id: &str) -> bool {
            *self.connected.lock().unwrap()
        }
        fn database(&self) -> Arc<Database> {
            self.db.clone()
        }
        fn pairing(&self, channel: &str) -> Option<Pairing> {
            self.pairings
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.control_channel_id == channel)
                .cloned()
        }
        fn pending(&self) -> Arc<Pending> {
            self.pending.clone()
        }
        async fn send(&self, proxy_id: &str, payload: Value) {
            self.log
                .lock()
                .unwrap()
                .push(("send", proxy_id.to_string(), payload.clone()));
            let tap = self
                .tap
                .lock()
                .unwrap()
                .as_ref()
                .and_then(std::sync::Weak::upgrade);
            if let Some(engine) = tap {
                engine.observe(&payload);
            }
        }
        async fn inject(&self, proxy_id: &str, payload: Value) {
            self.log
                .lock()
                .unwrap()
                .push(("inject", proxy_id.to_string(), payload));
        }
        async fn browser(&self, request: BrowserRequest) -> Option<BrowserResponse> {
            self.shots.lock().unwrap().push(request);
            if *self.gone_on_shot.lock().unwrap() {
                *self.connected.lock().unwrap() = false;
            }
            self.shot_answer.lock().unwrap().clone()
        }
    }

    pub(crate) struct Rig {
        pub(crate) engine: Arc<TaskEngine>,
        pub(crate) host: Arc<FakeHost>,
        pub(crate) seen: Arc<status_testing::Captured>,
        clock: Arc<AtomicI64>,
    }

    pub(crate) fn rig() -> Rig {
        let host = Arc::new(FakeHost {
            db: Arc::new(Database::open_in_memory().unwrap()),
            sidebar: Mutex::new(Some(SIDEBAR.to_string())),
            connected: Mutex::new(true),
            pairings: Mutex::new(vec![status_testing::agent(CHANNEL, 1)]),
            log: Mutex::new(Vec::new()),
            shots: Mutex::new(Vec::new()),
            shot_answer: Mutex::new(None),
            gone_on_shot: Mutex::new(false),
            pending: Arc::new(Pending::new()),
            tap: Mutex::new(None),
        });
        let (status, seen, _status_clock) = status_testing::watched(T0);
        status.set_pairings(&[status_testing::agent(CHANNEL, 1)]);
        let clock = Arc::new(AtomicI64::new(T0));
        let c = clock.clone();
        let engine = Arc::new(
            TaskEngine::new(host.clone(), status).with_clock(move || c.load(Ordering::SeqCst)),
        );
        *host.tap.lock().unwrap() = Some(Arc::downgrade(&engine));
        Rig {
            engine,
            host,
            seen,
            clock,
        }
    }

    impl Rig {
        /// Start a task as the paired agent; `(task_id, session)`.
        pub(crate) async fn start(&self) -> (String, String) {
            let v = self
                .engine
                .start(CHANNEL, "Generate a calculator Canvas", None)
                .await
                .expect("started");
            (
                v["task_id"].as_str().unwrap().to_string(),
                v["session"].as_str().unwrap().to_string(),
            )
        }

        /// Move the engine's clock to `T0 + secs`.
        pub(crate) fn at(&self, secs: i64) {
            self.clock.store(T0 + secs, Ordering::SeqCst);
        }
    }

    pub(crate) fn tool_start(session: &str, id: &str, tool: &str) -> Value {
        json!({"type": "stream_chunk", "payload": {"session_id": session, "content": "", "done": false,
               "event": {"type": "tool_start", "tool_id": id, "tool_name": tool, "icon": "", "summary": ""}}})
    }

    pub(crate) fn tool_end(session: &str, id: &str, status: &str) -> Value {
        json!({"type": "stream_chunk", "payload": {"session_id": session, "content": "", "done": false,
               "event": {"type": "tool_end", "tool_id": id, "status": status, "duration_ms": 10, "summary": ""}}})
    }

    pub(crate) fn text(session: &str, content: &str) -> Value {
        json!({"type": "stream_chunk", "payload": {"session_id": session, "content": content, "done": false}})
    }

    /// An `ask_user` request: a permission prompt when `purpose` is given.
    pub(crate) fn ask(
        session: &str,
        request_id: &str,
        purpose: Option<&str>,
        options: &[&str],
    ) -> Value {
        let mut params = json!({
            "question": "AI wants to perform an action:\n\nCreate a Canvas artifact: \"Calculator\"\n\nDo you want to allow this?",
            "options": options, "allow_custom": false, "timeout_ms": 86400000
        });
        if let Some(p) = purpose {
            params["purpose"] = json!(p);
            params["description"] = json!("Create a Canvas artifact: \"Calculator\"");
        }
        json!({"type": "browser_tool_request", "payload": {"request_id": request_id, "session_id": session,
               "tab_id": null, "action": "ask_user", "params": params, "timeout_ms": 86400000}})
    }

    pub(crate) fn resolved(session: &str, request_id: &str, reason: Option<&str>) -> Value {
        crate::pending::browser_resolved(request_id, session, reason)
    }

    /// A turn waiting on `request_id`, as the browser request handler files it.
    pub(crate) fn waiting_request(
        rig: &Rig,
        request_id: &str,
        session: &str,
    ) -> tokio::sync::oneshot::Receiver<BrowserResponse> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        rig.host.pending.insert_browser(
            request_id.to_string(),
            crate::pending::PendingBrowserRequest {
                session_id: session.to_string(),
                proxy_id: SIDEBAR.to_string(),
                client_identity: SIDEBAR.as_bytes().to_vec(),
                deadline: std::time::Instant::now() + DELEGATED_HOLD,
                sender: tx,
            },
        );
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[derive(Default)]
    struct Collect(Mutex<Vec<(u64, String)>>);

    #[async_trait]
    impl ProgressSink for Collect {
        async fn progress(&self, count: u64, message: String) {
            self.0.lock().unwrap().push((count, message));
        }
    }

    fn shot() -> BrowserResponse {
        BrowserResponse {
            request_id: "shot".into(),
            success: true,
            result: Some(
                json!({"mime_type": "image/jpeg", "data": "/9j/4AAQ", "width": 10,
                                "height": 10, "bytes": 6, "tab_kind": "canvas"}),
            ),
            error: None,
        }
    }

    fn guard(r: &Rig, session: &str) -> TurnGuard {
        match r.engine.begin_turn(session) {
            TurnStart::Run(g) => g,
            TurnStart::Skip => panic!("the turn was skipped"),
        }
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_task_starts_in_a_new_session_marked_with_its_pairing() {
        let r = rig();
        let (task_id, session) = r.start().await;
        assert_eq!(task_id.len(), 36);
        assert_eq!(
            super::super::origin::delegated_pairing(&r.host.db, &session).as_deref(),
            Some(CHANNEL)
        );
        let log = r.host.log.lock().unwrap().clone();
        assert_eq!(log.len(), 2);
        assert_eq!(
            log[0],
            (
                "send",
                SIDEBAR.to_string(),
                json!({"type": "session_origin", "payload": {"session_id": session, "origin": {"kind": "muse", "pairing": CHANNEL}}})
            ),
            "the sidebar learns the session is delegated before its turn arrives"
        );
        let (kind, proxy, chat) = &log[1];
        assert_eq!((*kind, proxy.as_str()), ("inject", SIDEBAR));
        assert_eq!(chat["type"], "chat_message");
        assert_eq!(chat["payload"]["session_id"], session.as_str());
        assert_eq!(chat["payload"]["mode"], "agent");
        assert_eq!(chat["payload"]["content"], "Generate a calculator Canvas");
    }

    #[tokio::test]
    async fn a_second_task_while_one_runs_is_busy() {
        let r = rig();
        let (first, _) = r.start().await;
        assert_eq!(
            r.engine.start(CHANNEL, "again", None).await.unwrap_err(),
            AgentError::TaskBusy { task_id: first }
        );
    }

    #[tokio::test]
    async fn the_twenty_first_start_in_an_hour_is_rate_limited() {
        let r = rig();
        for i in 0..20 {
            r.at(i * 60);
            let (id, _) = r.start().await;
            r.engine.cancel(Some(CHANNEL), &id).await.unwrap();
        }
        r.at(20 * 60);
        assert_eq!(
            r.engine.start(CHANNEL, "one more", None).await.unwrap_err(),
            AgentError::RateLimited {
                retry_after: 3600 - 20 * 60
            }
        );
        r.at(3600);
        assert!(
            r.engine.start(CHANNEL, "now", None).await.is_ok(),
            "the first start left the window"
        );
    }

    #[tokio::test]
    async fn only_this_agents_task_sessions_can_be_continued() {
        let r = rig();
        nevoflux_storage::SessionRepository::new(&r.host.db)
            .create(nevoflux_storage::models::CreateSessionParams::new().with_id("s-own"))
            .unwrap();
        super::super::origin::create_delegated(&r.host.db, "s-other", "chan-2").unwrap();
        for s in ["s-own", "s-other", "s-missing"] {
            assert_eq!(
                r.engine.start(CHANNEL, "go on", Some(s)).await.unwrap_err(),
                AgentError::SessionNotFound {
                    session: s.to_string()
                }
            );
        }
        let (id, session) = r.start().await;
        r.engine.cancel(Some(CHANNEL), &id).await.unwrap();
        assert!(matches!(r.engine.begin_turn(&session), TurnStart::Skip));
        let again = r
            .engine
            .start(CHANNEL, "make it blue", Some(&session))
            .await
            .unwrap();
        assert_eq!(again["session"], session.as_str());
    }

    #[tokio::test]
    async fn a_session_is_continued_only_once_its_previous_turn_was_skipped() {
        let r = rig();
        let (first, s) = r.start().await;
        r.engine.cancel(Some(CHANNEL), &first).await.unwrap();
        assert_eq!(
            r.engine
                .start(CHANNEL, "go on", Some(&s))
                .await
                .unwrap_err(),
            AgentError::TaskBusy {
                task_id: first.clone()
            },
            "the cancelled task's chat message has not reached its turn yet"
        );
        assert!(
            matches!(r.engine.begin_turn(&s), TurnStart::Skip),
            "the first task's prompt is skipped, not taken for a later task's turn"
        );
        let second = r.engine.start(CHANNEL, "go on", Some(&s)).await.unwrap();
        let second = second["task_id"].as_str().unwrap().to_string();
        let turn = guard(&r, &s);
        r.engine.observe(&text(&s, "Continued."));
        drop(turn);
        let w = r
            .engine
            .wait(CHANNEL, "m1", &second, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "done");
        assert_eq!(w.state["reply"], "Continued.");
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_that_ends_never_concludes_the_task_that_continued_its_session() {
        let r = rig();
        // Markers come back only when the test replays them.
        *r.host.tap.lock().unwrap() = None;
        let (first, s) = r.start().await;
        let turn1 = guard(&r, &s);
        r.engine.cancel(Some(CHANNEL), &first).await.unwrap();
        assert_eq!(
            r.engine
                .start(CHANNEL, "go on", Some(&s))
                .await
                .unwrap_err(),
            AgentError::TaskBusy {
                task_id: first.clone()
            },
            "the first turn is still running"
        );
        drop(turn1);
        let second = r.engine.start(CHANNEL, "go on", Some(&s)).await.unwrap();
        let second = second["task_id"].as_str().unwrap().to_string();
        // The first turn's last frame, then its marker, reach the tap after
        // the second task started.
        r.engine
            .observe(&tool_start(&s, "toolu_late", "browser_click"));
        r.engine.observe(&text(&s, "Late from the first turn."));
        r.engine.observe(&session_origin_frame(&s, CHANNEL));
        settle().await;
        let turn2 = guard(&r, &s);
        r.engine.observe(&text(&s, "From the second turn."));
        // A stale marker for the first turn while the second runs.
        r.engine.observe(&session_origin_frame(&s, CHANNEL));
        settle().await;
        let w = r
            .engine
            .wait(CHANNEL, "m1", &second, Duration::from_millis(1), None)
            .await
            .unwrap();
        assert_eq!(
            w.state["state"], "running",
            "the first turn did not conclude the second task"
        );
        assert_eq!(
            w.state["steps"],
            json!([]),
            "the first turn's late frames are not the second task's"
        );
        drop(turn2);
        r.engine.observe(&session_origin_frame(&s, CHANNEL));
        let w = r
            .engine
            .wait(CHANNEL, "m1", &second, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "done");
        assert_eq!(w.state["reply"], "From the second turn.");
        let w = r
            .engine
            .wait(CHANNEL, "m1", &first, Duration::from_millis(1), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "cancelled");
    }

    #[tokio::test]
    async fn a_sidebar_that_left_during_the_screenshot_is_browser_unavailable() {
        let r = rig();
        let (id, s) = r.start().await;
        *r.host.gone_on_shot.lock().unwrap() = true;
        let turn = guard(&r, &s);
        r.engine.observe(
            &json!({"type": "artifact_start", "payload": {"session_id": s, "id": "calc-1",
                                 "title": "Calculator", "content_type": "text/html"}}),
        );
        drop(turn);
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "done");
        assert_eq!(
            w.state["screenshot_error"], "browser_unavailable",
            "not timeout"
        );
        assert_eq!(r.host.shots.lock().unwrap().len(), 1, "it was asked");
    }

    #[tokio::test]
    async fn a_paused_agent_or_a_missing_browser_cannot_start() {
        let r = rig();
        r.host.pairings.lock().unwrap()[0].paused = true;
        assert_eq!(
            r.engine.start(CHANNEL, "x", None).await.unwrap_err(),
            AgentError::Paused
        );
        r.host.pairings.lock().unwrap()[0].paused = false;
        *r.host.sidebar.lock().unwrap() = None;
        assert_eq!(
            r.engine.start(CHANNEL, "x", None).await.unwrap_err(),
            AgentError::BrowserUnavailable
        );
    }

    #[tokio::test]
    async fn frames_of_other_sessions_change_nothing() {
        let r = rig();
        let (id, _session) = r.start().await;
        r.engine
            .observe(&tool_start("someone-else", "t1", "browser_click"));
        r.engine.observe(&ask(
            "someone-else",
            "r9",
            Some("permission"),
            &["Allow", "Deny"],
        ));
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_millis(1), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "running");
        assert_eq!(w.state["steps"], json!([]));
    }

    #[tokio::test]
    async fn each_wait_returns_only_new_steps_and_a_reconnect_replays_them() {
        let r = rig();
        let (id, s) = r.start().await;
        r.at(5);
        r.engine.observe(&tool_start(&s, "toolu_01", "skill_load"));
        r.at(9);
        r.engine.observe(&tool_end(&s, "toolu_01", "success"));
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_millis(1), None)
            .await
            .unwrap();
        assert_eq!(
            w.state["steps"],
            json!([{"id": "toolu_01", "at": "2026-10-14T17:46:45Z", "tool": "skill_load", "status": "started"},
                   {"id": "toolu_01", "at": "2026-10-14T17:46:49Z", "tool": "skill_load", "status": "ok"}])
        );
        r.engine
            .observe(&tool_start(&s, "toolu_02", "browser_click"));
        r.engine.observe(&tool_end(&s, "toolu_02", "failed"));
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_millis(1), None)
            .await
            .unwrap();
        let statuses: Vec<&str> = w.state["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["started", "failed"]);
        let w = r
            .engine
            .wait(CHANNEL, "m2", &id, Duration::from_millis(1), None)
            .await
            .unwrap();
        assert_eq!(
            w.state["steps"].as_array().unwrap().len(),
            4,
            "a new MCP session starts from the first event"
        );
    }

    #[tokio::test]
    async fn an_open_approval_ends_a_wait_at_once_and_is_reported_as_progress() {
        let r = rig();
        let (id, s) = r.start().await;
        r.engine
            .observe(&tool_start(&s, "toolu_02", "create_artifact"));
        r.engine.observe(&ask(
            &s,
            "req-1",
            Some("permission"),
            &["Allow", "Always allow this type of action", "Deny"],
        ));
        let sink = Collect::default();
        let began = std::time::Instant::now();
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(30), Some(&sink))
            .await
            .unwrap();
        assert!(began.elapsed() < Duration::from_secs(5));
        assert_eq!(w.state["state"], "awaiting_approval");
        assert_eq!(w.state["approvals"][0]["kind"], "tool");
        assert_eq!(w.state["approvals"][0]["tool"], "create_artifact");
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![
                (1, "Running create_artifact".to_string()),
                (
                    2,
                    "Waiting for approval: Create a Canvas artifact: \"Calculator\"".to_string()
                ),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_does_not_end_a_wait_early() {
        let r = rig();
        let (id, s) = r.start().await;
        let engine = r.engine.clone();
        let waiting = tokio::spawn(async move {
            engine
                .wait(CHANNEL, "m1", &id, Duration::from_secs(30), None)
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        r.engine.observe(&tool_start(&s, "toolu_01", "skill_load"));
        settle().await;
        assert!(
            !waiting.is_finished(),
            "a step is not a reason to return early"
        );
        tokio::time::advance(Duration::from_secs(21)).await;
        let w = waiting.await.unwrap().unwrap();
        assert_eq!(w.state["steps"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_finished_turn_is_photographed_then_done_with_one_image_per_session() {
        let r = rig();
        let (id, s) = r.start().await;
        *r.host.shot_answer.lock().unwrap() = Some(shot());
        let turn = guard(&r, &s);
        r.engine.observe(
            &json!({"type": "artifact_start", "payload": {"session_id": s, "id": "calc-1",
                                 "title": "Calculator", "content_type": "text/html"}}),
        );
        r.engine.observe(&text(&s, "Here is your calculator."));
        drop(turn);
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "done");
        assert_eq!(w.state["reply"], "Here is your calculator.");
        assert_eq!(w.image.as_deref(), Some("/9j/4AAQ"));
        let asked = r.host.shots.lock().unwrap()[0].clone();
        assert_eq!(asked.origin.as_deref(), Some("system"));
        assert_eq!(asked.params["artifact_id"], "calc-1");
        assert_eq!(asked.proxy_id, SIDEBAR);
        let again = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(1), None)
            .await
            .unwrap();
        assert!(again.image.is_none(), "sent once per MCP session");
        assert!(again.state.get("screenshot_error").is_none());
        let other = r
            .engine
            .wait(CHANNEL, "m2", &id, Duration::from_secs(1), None)
            .await
            .unwrap();
        assert!(
            other.image.is_some(),
            "a reconnected client gets it once too"
        );
    }

    #[tokio::test]
    async fn a_task_that_touched_nothing_is_done_without_a_picture() {
        let r = rig();
        let (id, s) = r.start().await;
        drop(guard(&r, &s));
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "done");
        assert_eq!(w.state["screenshot_error"], "no_tab");
        assert!(r.host.shots.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_turn_that_reported_an_error_fails_with_llm_error() {
        let r = rig();
        let (id, s) = r.start().await;
        let turn = guard(&r, &s);
        r.engine.observe(
            &json!({"type": "error", "payload": {"session_id": s, "code": "AGENT_ERROR",
                                 "message": "Agent error: rate limited"}}),
        );
        drop(turn);
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(
            w.state["error"],
            json!({"code": "llm_error", "message": "the model call failed: Agent error: rate limited"})
        );
        assert!(r.host.shots.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_turn_whose_sidebar_went_away_fails_as_browser_unavailable() {
        let r = rig();
        let (id, s) = r.start().await;
        let turn = guard(&r, &s);
        *r.host.connected.lock().unwrap() = false;
        drop(turn);
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["error"]["code"], "browser_unavailable");
    }

    #[tokio::test]
    async fn a_task_cancelled_before_its_turn_skips_the_turn() {
        let r = rig();
        let (id, s) = r.start().await;
        assert_eq!(
            r.engine.cancel(Some(CHANNEL), &id).await.unwrap(),
            json!({"task_id": id, "state": "cancelled"})
        );
        assert!(matches!(r.engine.begin_turn(&s), TurnStart::Skip));
        let (_, stop) = r.host.injected().last().cloned().unwrap();
        assert_eq!(stop["type"], "stop_generation");
        assert_eq!(stop["payload"]["session_id"], s.as_str());
    }

    #[tokio::test]
    async fn cancel_is_idempotent_and_a_finished_task_keeps_its_state() {
        let r = rig();
        let (id, s) = r.start().await;
        r.engine.cancel(Some(CHANNEL), &id).await.unwrap();
        let stops = r.host.injected().len();
        assert_eq!(
            r.engine.cancel(Some(CHANNEL), &id).await.unwrap()["state"],
            "cancelled"
        );
        assert_eq!(
            r.host.injected().len(),
            stops,
            "nothing to stop the second time"
        );
        assert_eq!(
            r.engine.cancel(Some("chan-2"), &id).await.unwrap_err(),
            AgentError::TaskNotFound {
                task_id: id.clone()
            },
            "another agent's task"
        );
        let (done, s2) = r.start().await;
        drop(guard(&r, &s2));
        r.engine
            .wait(CHANNEL, "m1", &done, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(
            r.engine.cancel(Some(CHANNEL), &done).await.unwrap()["state"],
            "done"
        );
        let _ = s;
    }

    #[tokio::test]
    async fn pausing_or_unpairing_cancels_the_agents_task() {
        let r = rig();
        let (id, _) = r.start().await;
        r.engine.cancel_for_pairing(CHANNEL).await;
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_millis(1), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "cancelled");
    }

    #[tokio::test]
    async fn the_status_bar_sees_the_task_until_it_ends_and_the_end_is_announced_once() {
        let r = rig();
        let (id, s) = r.start().await;
        let task = r.seen.statuses().last().unwrap()["agents"][0]["task"].clone();
        assert_eq!(
            task,
            json!({"id": id, "session_id": s, "summary": "Generate a calculator Canvas", "state": "running",
                   "tool": null, "started_at": T0, "approval": null})
        );
        r.at(20);
        r.engine.observe(&ask(
            &s,
            "req-1",
            Some("permission"),
            &["Allow", "Always allow this type of action", "Deny"],
        ));
        let task = r.seen.statuses().last().unwrap()["agents"][0]["task"].clone();
        assert_eq!(task["state"], "awaiting_approval");
        assert_eq!(
            task["approval"]["choices"],
            json!(["allow", "allow_always", "deny"])
        );
        assert_eq!(task["approval"]["expires_at"], T0 + 20 + 1800);
        r.engine.cancel(Some(CHANNEL), &id).await.unwrap();
        assert_eq!(
            r.seen.statuses().last().unwrap()["agents"][0]["task"],
            Value::Null
        );
        let notes = r.seen.notifications();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["title"], "Task from Muse was cancelled");
        assert_eq!(notes[0]["source"], "agent_task");
        assert_eq!(notes[0]["outcome"], "cancelled");
        assert_eq!(notes[0]["body"], "Generate a calculator Canvas");
        assert_eq!(notes[0]["task_id"], id.as_str());
        assert_eq!(notes[0]["session_id"], s.as_str());
        r.engine.cancel(Some(CHANNEL), &id).await.unwrap();
        assert_eq!(r.seen.notifications().len(), 1, "announced once");
    }

    #[test]
    fn the_end_notice_uses_the_agents_label() {
        let done = task_notification("t", "s", "Sum", TaskState::Done, Some("Claude"));
        assert_eq!(done["title"], "Task from Claude finished");
        assert_eq!(
            task_notification("t", "s", "Sum", TaskState::Failed, None)["title"],
            "Task from Muse failed"
        );
        assert_eq!(
            task_notification("t", "s", "Sum", TaskState::Failed, Some(" "))["outcome"],
            "failed"
        );
    }

    #[tokio::test]
    async fn a_finished_task_is_forgotten_after_an_hour_and_other_agents_never_see_it() {
        let r = rig();
        let (id, _) = r.start().await;
        assert_eq!(
            r.engine
                .wait("chan-2", "m1", &id, Duration::from_millis(1), None)
                .await
                .unwrap_err(),
            AgentError::TaskNotFound {
                task_id: id.clone()
            }
        );
        r.engine.cancel(Some(CHANNEL), &id).await.unwrap();
        r.at(3599);
        assert!(r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_millis(1), None)
            .await
            .is_ok());
        r.at(3600);
        assert_eq!(
            r.engine
                .wait(CHANNEL, "m1", &id, Duration::from_millis(1), None)
                .await
                .unwrap_err(),
            AgentError::TaskNotFound { task_id: id }
        );
    }

    #[tokio::test]
    async fn a_new_tab_the_turn_opened_is_what_gets_photographed() {
        let r = rig();
        let (id, s) = r.start().await;
        *r.host.shot_answer.lock().unwrap() = Some(shot());
        let turn = guard(&r, &s);
        r.engine.observe_browser_response(
            &s,
            Some(&json!({"url": "https://x.example/", "tab_id": 7, "new_tab": true})),
        );
        r.engine
            .observe_browser_response(&s, Some(&json!({"ok": true})));
        drop(turn);
        r.engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(r.host.shots.lock().unwrap()[0].tab_id, Some(7));
    }

    #[tokio::test]
    async fn reply_text_still_on_its_way_when_the_turn_ends_is_kept() {
        let r = rig();
        let (id, s) = r.start().await;
        let turn = guard(&r, &s);
        r.engine.observe(&text(&s, "Here is "));
        drop(turn);
        // Queued by the turn before it ended; reaches the tap after the drop.
        r.engine.observe(&text(&s, "your calculator."));
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "done");
        assert_eq!(w.state["reply"], "Here is your calculator.");
        assert_eq!(
            r.host.sent().last().cloned().unwrap(),
            (SIDEBAR.to_string(), session_origin_frame(&s, CHANNEL)),
            "the turn's end is marked behind its frames"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_end_whose_marker_never_comes_back_still_concludes() {
        let r = rig();
        *r.host.tap.lock().unwrap() = None;
        let (id, s) = r.start().await;
        drop(guard(&r, &s));
        let engine = r.engine.clone();
        let waiting = tokio::spawn(async move {
            engine
                .wait(CHANNEL, "m1", &id, Duration::from_secs(60), None)
                .await
        });
        tokio::task::yield_now().await;
        settle().await;
        assert!(
            !waiting.is_finished(),
            "still waiting for the turn's last frame"
        );
        tokio::time::advance(TURN_END_GRACE).await;
        let w = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("concluded after the grace")
            .unwrap()
            .unwrap();
        assert_eq!(w.state["state"], "done");
    }

    #[tokio::test]
    async fn an_approval_nobody_answered_fails_the_task_and_stops_its_turn() {
        let r = rig();
        let (id, s) = r.start().await;
        r.engine
            .observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Deny"]));
        r.engine.observe(&resolved(&s, "req-1", Some("timeout")));
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "failed");
        assert_eq!(
            w.state["error"],
            json!({"code": "approval_timeout",
                   "message": "nobody answered an approval within 30 minutes; it was denied"})
        );
        settle().await;
        let (proxy, stop) = r.host.injected().last().cloned().unwrap();
        assert_eq!(proxy, SIDEBAR);
        assert_eq!(stop, stop_frame(&s));
        assert_eq!(r.seen.notifications().len(), 1);
        assert_eq!(
            r.seen.statuses().last().unwrap()["agents"][0]["task"],
            Value::Null
        );
    }

    #[tokio::test]
    async fn an_answered_approval_only_returns_the_task_to_running() {
        let r = rig();
        let (id, s) = r.start().await;
        r.engine
            .observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Deny"]));
        r.engine.observe(&resolved(&s, "req-1", None));
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_millis(1), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "running");
        settle().await;
        assert!(r
            .host
            .injected()
            .iter()
            .all(|(_, v)| v["type"] != "stop_generation"));
    }

    #[tokio::test]
    async fn an_error_without_a_session_belongs_to_the_view_not_the_task() {
        let r = rig();
        let (id, s) = r.start().await;
        let turn = guard(&r, &s);
        r.engine.observe(
            &json!({"type": "error", "payload": {"code": "EMPTY_MESSAGE",
                                 "message": "Message is empty"}}),
        );
        drop(turn);
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "done");
    }

    #[tokio::test]
    async fn no_sidebar_to_photograph_with_is_browser_unavailable_and_no_answer_is_timeout() {
        let r = rig();
        let (id, s) = r.start().await;
        let turn = guard(&r, &s);
        r.engine.observe(
            &json!({"type": "artifact_start", "payload": {"session_id": s, "id": "calc-1",
                                 "title": "Calculator", "content_type": "text/html"}}),
        );
        drop(turn);
        *r.host.sidebar.lock().unwrap() = None;
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["state"], "done");
        assert_eq!(w.state["screenshot_error"], "browser_unavailable");
        assert!(r.host.shots.lock().unwrap().is_empty(), "nothing was sent");

        *r.host.sidebar.lock().unwrap() = Some(SIDEBAR.to_string());
        let again = r
            .engine
            .start(CHANNEL, "make it blue", Some(&s))
            .await
            .unwrap();
        let id2 = again["task_id"].as_str().unwrap().to_string();
        assert_ne!(id2, id);
        let turn = guard(&r, &s);
        r.engine.observe(
            &json!({"type": "artifact_start", "payload": {"session_id": s, "id": "calc-1",
                                 "title": "Calculator", "content_type": "text/html"}}),
        );
        drop(turn);
        let w = r
            .engine
            .wait(CHANNEL, "m1", &id2, Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(w.state["screenshot_error"], "timeout");
        assert_eq!(r.host.shots.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn questions_of_a_running_task_are_held_thirty_minutes() {
        let r = rig();
        let (id, s) = r.start().await;
        assert_eq!(r.engine.hold_for(&s), Some(DELEGATED_HOLD));
        assert_eq!(r.engine.hold_for("someone-else"), None);
        r.engine.cancel(Some(CHANNEL), &id).await.unwrap();
        assert_eq!(r.engine.hold_for(&s), None);
    }

    use super::super::answer::Choice;

    #[tokio::test]
    async fn approving_a_tool_answers_the_waiting_turn_with_its_allow_button() {
        let r = rig();
        let (id, s) = r.start().await;
        let mut rx = waiting_request(&r, "req-1", &s);
        r.engine.observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Always allow this type of action", "Deny"]));
        let state = r
            .engine
            .approve(Answerer::Agent(CHANNEL.into()), &id, "req-1", Choice::Allow, None)
            .await
            .unwrap();
        assert_eq!(state, TaskState::Running);
        let answer = rx.try_recv().unwrap();
        assert!(answer.success);
        assert_eq!(answer.result.unwrap()["answer"], "Allow");
        assert!(r.host.sent().contains(&(SIDEBAR.to_string(), crate::pending::browser_resolved("req-1", &s, None))),
            "the desktop dialog is told to go away");
    }

    #[tokio::test]
    async fn the_desktop_may_answer_always_allow() {
        let r = rig();
        let (id, s) = r.start().await;
        let mut rx = waiting_request(&r, "req-1", &s);
        r.engine.observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Always allow this type of action", "Deny"]));
        r.engine.approve(Answerer::Desktop, &id, "req-1", Choice::AllowAlways, None).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().result.unwrap()["answer"], "Always allow this type of action");
    }

    #[tokio::test]
    async fn a_question_is_answered_with_text_or_cancelled() {
        let r = rig();
        let (id, s) = r.start().await;
        let mut first = waiting_request(&r, "req-q1", &s);
        let mut second = waiting_request(&r, "req-q2", &s);
        r.engine.observe(&ask(&s, "req-q1", None, &["Blue", "Green"]));
        r.engine.observe(&ask(&s, "req-q2", None, &[]));
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-q1", Choice::Allow, None).await,
            Err(ApproveError::Invalid("answer is required for a question"))
        );
        r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-q1", Choice::Allow, Some("Blue")).await.unwrap();
        assert_eq!(first.try_recv().unwrap().result.unwrap()["answer"], "Blue");
        r.engine.approve(Answerer::Desktop, &id, "req-q2", Choice::Deny, None).await.unwrap();
        let cancelled = second.try_recv().unwrap();
        assert!(!cancelled.success);
        assert_eq!(cancelled.error.unwrap().code, 8001);
    }

    #[tokio::test]
    async fn a_plan_is_confirmed_through_the_plan_registry() {
        let r = rig();
        let (id, s) = r.start().await;
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        r.host.pending.insert_plan(&s, crate::pending::PendingPlan {
            sender: tx,
            proxy_id: SIDEBAR.into(),
            client_identity: SIDEBAR.as_bytes().to_vec(),
        });
        r.engine.observe(&json!({"type": "plan_proposal", "payload": {"session_id": s, "summary": "Build the calculator",
                                 "steps": [{"description": "Write the HTML"}]}}));
        let w = r.engine.wait(CHANNEL, "m1", &id, Duration::from_secs(1), None).await.unwrap();
        let plan_id = w.state["approvals"][0]["approval_id"].as_str().unwrap().to_string();
        assert_eq!(plan_id.len(), 36);
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, &plan_id, Choice::Allow, None).await,
            Ok(TaskState::Running)
        );
        assert_eq!(rx.try_recv().unwrap(), nevoflux_protocol::PlanResponse::Confirmed);
        assert!(r.host.sent().contains(&(
            SIDEBAR.to_string(),
            crate::pending::plan_resolved(&s, &nevoflux_protocol::PlanResponse::Confirmed, None)
        )));
    }

    #[tokio::test]
    async fn the_second_answer_finds_nothing_to_answer() {
        let r = rig();
        let (id, s) = r.start().await;
        let mut rx = waiting_request(&r, "req-1", &s);
        r.engine.observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Deny"]));
        // The desktop's dialog answered first: the message loop took the entry.
        let desk = r.host.pending.take_browser("req-1").unwrap();
        let _ = desk.sender.send(super::super::answer::response_for(
            "req-1",
            super::super::answer::Answer::Option("Deny".into()),
        ));
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-1", Choice::Allow, None).await,
            Err(ApproveError::ApprovalNotFound)
        );
        assert_eq!(rx.try_recv().unwrap().result.unwrap()["answer"], "Deny", "only the first answer arrived");
        r.engine.observe(&resolved(&s, "req-1", None));
        assert_eq!(
            r.engine.approve(Answerer::Desktop, &id, "req-1", Choice::Allow, None).await,
            Err(ApproveError::ApprovalNotFound),
            "and once its resolution passed the tap it is gone for everyone"
        );
        assert_eq!(
            r.engine.approve(Answerer::Agent("chan-2".into()), &id, "req-1", Choice::Allow, None).await,
            Err(ApproveError::TaskNotFound)
        );
    }

    #[tokio::test]
    async fn an_answer_after_the_deadline_or_a_stop_finds_nothing_to_answer() {
        let r = rig();
        let (id, s) = r.start().await;
        let mut expired = waiting_request(&r, "req-1", &s);
        r.engine.observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Deny"]));
        let (tx, mut plan_rx) = tokio::sync::oneshot::channel();
        r.host.pending.insert_plan(&s, crate::pending::PendingPlan {
            sender: tx,
            proxy_id: SIDEBAR.into(),
            client_identity: SIDEBAR.as_bytes().to_vec(),
        });
        r.engine.observe(&json!({"type": "plan_proposal", "payload": {"session_id": s, "summary": "Build it", "steps": []}}));
        let w = r.engine.wait(CHANNEL, "m1", &id, Duration::from_secs(1), None).await.unwrap();
        let plan_id = w.state["approvals"][1]["approval_id"].as_str().unwrap().to_string();
        assert_eq!(w.state["approvals"][1]["kind"], "plan");
        // The sweeper got there first: the question's deadline passed.
        let swept = r.host.pending.expire(std::time::Instant::now() + DELEGATED_HOLD);
        assert_eq!(swept.len(), 1);
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-1", Choice::Allow, None).await,
            Err(ApproveError::ApprovalNotFound)
        );
        assert!(!expired.try_recv().unwrap().success, "the refusal is the only answer");
        // A stop settles the plan before anybody answers it.
        assert_eq!(r.host.pending.release_session(&s).len(), 1);
        assert_eq!(
            r.engine.approve(Answerer::Desktop, &id, &plan_id, Choice::Allow, None).await,
            Err(ApproveError::ApprovalNotFound)
        );
        assert_eq!(plan_rx.try_recv().unwrap(), nevoflux_protocol::PlanResponse::Cancelled);
    }

    #[tokio::test]
    async fn an_approval_that_timed_out_fails_the_task_and_stops_its_turn() {
        let r = rig();
        let (id, s) = r.start().await;
        r.engine.observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Deny"]));
        r.engine.observe(&resolved(&s, "req-1", Some("timeout")));
        let w = r.engine.wait(CHANNEL, "m1", &id, Duration::from_secs(1), None).await.unwrap();
        assert_eq!(
            w.state["error"],
            json!({"code": "approval_timeout", "message": "nobody answered an approval within 30 minutes; it was denied"})
        );
        settle().await;
        let (_, stop) = r.host.injected().last().cloned().unwrap();
        assert_eq!(stop["type"], "stop_generation");
    }

    #[tokio::test(start_paused = true)]
    async fn a_delegated_question_nobody_answers_times_out_in_the_registry_and_fails_the_task() {
        let r = rig();
        let (id, s) = r.start().await;
        let filed = std::time::Instant::now();
        let rx = waiting_request(&r, "req-q", &s);
        // The turn's own timer, as `execute_browser_action` sets it.
        let wait = Duration::from_millis(crate::agent_host::browser_wait_ms(
            nevoflux_protocol::BrowserToolAction::AskUser,
            r.engine.hold_for(&s),
        ));
        let turn = tokio::spawn(tokio::time::timeout(wait, rx));
        tokio::task::yield_now().await;
        r.engine.observe(&ask(&s, "req-q", None, &[]));
        // The sweeper reaches the question at most one interval past its deadline.
        let swept_at = DELEGATED_HOLD + crate::pending::SWEEP_INTERVAL;
        tokio::time::advance(swept_at).await;
        tokio::task::yield_now().await;
        assert!(!turn.is_finished(), "the turn still waits when the sweeper gets there");
        let swept = r.host.pending.expire(filed + swept_at);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].frame, resolved(&s, "req-q", Some("timeout")));
        let refused = tokio::time::timeout(Duration::from_secs(1), turn)
            .await
            .expect("the refusal ends the wait")
            .unwrap()
            .expect("answered by the registry, not by the turn's own timer")
            .unwrap();
        assert!(!refused.success);
        r.engine.observe(&swept[0].frame); // the chat tap
        let w = r.engine.wait(CHANNEL, "m1", &id, Duration::from_secs(1), None).await.unwrap();
        assert_eq!(w.state["state"], "failed");
        assert_eq!(w.state["error"]["code"], "approval_timeout");
    }

    #[tokio::test]
    async fn an_answer_the_turn_no_longer_waits_for_finds_nothing_to_answer() {
        let r = rig();
        let (id, s) = r.start().await;
        let rx = waiting_request(&r, "req-1", &s);
        r.engine.observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Deny"]));
        drop(rx); // the turn's own timer ran out
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-1", Choice::Allow, None).await,
            Err(ApproveError::ApprovalNotFound)
        );
        assert!(r.host.sent().contains(&(SIDEBAR.to_string(), resolved(&s, "req-1", Some("timeout")))),
            "the dialog is not told it was answered");
    }

    #[tokio::test]
    async fn a_plan_the_turn_stopped_waiting_for_finds_nothing_to_answer() {
        let r = rig();
        let (id, s) = r.start().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        r.host.pending.insert_plan(&s, crate::pending::PendingPlan {
            sender: tx,
            proxy_id: SIDEBAR.into(),
            client_identity: SIDEBAR.as_bytes().to_vec(),
        });
        r.engine.observe(&json!({"type": "plan_proposal", "payload": {"session_id": s, "summary": "Build it", "steps": []}}));
        let w = r.engine.wait(CHANNEL, "m1", &id, Duration::from_secs(1), None).await.unwrap();
        let plan_id = w.state["approvals"][0]["approval_id"].as_str().unwrap().to_string();
        drop(rx); // its hold passed and the turn went on as cancelled
        assert_eq!(
            r.engine.approve(Answerer::Desktop, &id, &plan_id, Choice::Allow, None).await,
            Err(ApproveError::ApprovalNotFound)
        );
        assert!(r.host.sent().contains(&(
            SIDEBAR.to_string(),
            crate::pending::plan_resolved(&s, &nevoflux_protocol::PlanResponse::Cancelled, Some("timeout"))
        )));
    }

    #[tokio::test]
    async fn a_paired_agent_can_never_answer_always_allow() {
        let r = rig();
        let (id, s) = r.start().await;
        let mut rx = waiting_request(&r, "req-1", &s);
        r.engine.observe(&ask(&s, "req-1", Some("permission"), &["Allow", "Always allow this type of action", "Deny"]));
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-1", Choice::AllowAlways, None).await,
            Err(ApproveError::Invalid(super::super::answer::NOT_OFFERED))
        );
        assert!(rx.try_recv().is_err(), "the question is still waiting");
        assert!(r.host.pending.take_browser("req-1").is_some());
    }

    fn must_approve_ask(session: &str, request_id: &str) -> Value {
        let mut frame = ask(session, request_id, Some("permission"), &["Allow", "Deny"]);
        frame["payload"]["params"]["must_approve"] = json!(true);
        frame
    }

    fn let_muse_approve(r: &Rig, on: bool) {
        r.host.pairings.lock().unwrap()[0].muse_may_approve_guarded = on;
    }

    #[tokio::test]
    async fn the_agent_cannot_answer_a_desktop_only_approval_but_the_desktop_can() {
        let r = rig();
        let (id, s) = r.start().await;
        let waiting = waiting_request(&r, "req-d", &s);
        r.engine.observe(&must_approve_ask(&s, "req-d"));
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-d", Choice::Allow, None).await,
            Err(ApproveError::DesktopOnly)
        );
        assert!(r.engine.approve(Answerer::Desktop, &id, "req-d", Choice::Allow, None).await.is_ok());
        assert_eq!(waiting.await.unwrap().result.unwrap()["answer"], "Allow");
    }

    #[tokio::test]
    async fn with_the_toggle_on_the_agent_answers_first_and_the_desktop_finds_nothing() {
        let r = rig();
        let_muse_approve(&r, true);
        let (id, s) = r.start().await;
        let waiting = waiting_request(&r, "req-g", &s);
        r.engine.observe(&must_approve_ask(&s, "req-g"));
        let item = r.engine.wait(CHANNEL, "m1", &id, Duration::from_secs(1), None).await.unwrap().state;
        assert_eq!(item["approvals"][0]["choices"], json!(["allow", "deny"]));
        assert!(item["approvals"][0].get("desktop_only").is_none());
        assert!(matches!(
            r.engine.approve(Answerer::Desktop, &id, "req-g", Choice::AllowAlways, None).await,
            Err(ApproveError::Invalid(_))
        ), "no Always allow for a must-approve tool, from either side");
        assert!(r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-g", Choice::Allow, None).await.is_ok());
        assert_eq!(waiting.await.unwrap().result.unwrap()["answer"], "Allow");
        assert_eq!(
            r.engine.approve(Answerer::Desktop, &id, "req-g", Choice::Deny, None).await,
            Err(ApproveError::ApprovalNotFound)
        );
    }

    #[tokio::test]
    async fn the_toggle_is_read_when_the_agent_answers_not_when_the_question_was_asked() {
        let r = rig();
        let_muse_approve(&r, true);
        let (id, s) = r.start().await;
        let waiting = waiting_request(&r, "req-t", &s);
        r.engine.observe(&must_approve_ask(&s, "req-t"));
        // Turned off while pending: the agent is refused and the wait says so.
        let_muse_approve(&r, false);
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-t", Choice::Deny, None).await,
            Err(ApproveError::DesktopOnly)
        );
        let off = r.engine.wait(CHANNEL, "m1", &id, Duration::from_secs(1), None).await.unwrap().state;
        assert_eq!(off["approvals"][0]["desktop_only"], true);
        assert_eq!(off["approvals"][0]["choices"], json!([]));
        // Turned on again: the agent's answer goes through.
        let_muse_approve(&r, true);
        assert!(r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-t", Choice::Deny, None).await.is_ok());
        assert_eq!(waiting.await.unwrap().result.unwrap()["answer"], "Deny");
    }

    #[tokio::test]
    async fn an_unknown_pairing_never_lets_the_agent_answer() {
        // The store unreadable or the row gone reads as off, never on.
        let r = rig();
        let (id, s) = r.start().await;
        let _waiting = waiting_request(&r, "req-u", &s);
        r.engine.observe(&must_approve_ask(&s, "req-u"));
        r.host.pairings.lock().unwrap().clear();
        assert_eq!(
            r.engine.approve(Answerer::Agent(CHANNEL.into()), &id, "req-u", Choice::Allow, None).await,
            Err(ApproveError::DesktopOnly)
        );
    }
}
