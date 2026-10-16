//! What the daemon is waiting on the browser for (M3 §3.8, §5.1).
//!
//! Two registries used to be bare maps inside `server.rs`: browser requests by
//! `request_id` (a permission dialog, the model's own question, or any other
//! browser tool call) and plan confirmations by session. Both had the same two
//! faults, and both are fixed here:
//!
//! - **A dead entry left its dialog up.** The sweeper dropped requests older
//!   than ten minutes — the waiting turn read that as a refusal — but nothing
//!   told the desktop, so the dialog stayed and pressing it answered nothing.
//!   Every entry now records the connection its question went to, and
//!   whatever settles it announces that there.
//! - **Stopping a turn did not end its wait.** A turn parked on a question or
//!   a plan polls neither the interrupt flag nor the stream's cancellation
//!   token, so the stop button did nothing until the 24-hour timeout.
//!   [`Pending::release_session`] settles everything a session waits on.
//!
//! Each browser entry carries its own deadline, fixed when it is registered: a
//! question in a task a paired agent delegated is held thirty minutes, any
//! other ten ([`hold_for`]). The sweeper only compares deadlines; nothing else
//! keeps time, so no two clocks can disagree about when a question expired.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use nevoflux_protocol::{BrowserToolError, PlanResponse};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::wasm::services::BrowserResponse;

/// How long a question outside a delegated task waits for an answer.
pub const LOCAL_HOLD: Duration = Duration::from_secs(600);

/// `reason` on a resolved announcement: nobody answered before the deadline.
pub const REASON_TIMEOUT: &str = "timeout";
/// `reason` on a resolved announcement: the turn that asked was stopped, or
/// stopped waiting.
pub const REASON_STOPPED: &str = "stopped";

/// A browser request still waiting for its answer.
pub struct PendingBrowserRequest {
    /// The session that asked.
    pub session_id: String,
    /// The connection the request went to: where its dialog is showing.
    pub proxy_id: String,
    pub client_identity: Vec<u8>,
    /// When it is refused if nobody has answered it.
    pub deadline: Instant,
    pub sender: oneshot::Sender<BrowserResponse>,
}

/// A plan proposal waiting to be confirmed or cancelled.
pub struct PendingPlan {
    pub sender: oneshot::Sender<PlanResponse>,
    /// The connection the proposal went to.
    pub proxy_id: String,
    pub client_identity: Vec<u8>,
}

/// A frame saying something was settled, and the connection it goes to.
#[derive(Debug, Clone, PartialEq)]
pub struct Announcement {
    pub client_identity: Vec<u8>,
    pub proxy_id: String,
    pub frame: Value,
}

/// Everything the daemon is waiting on the browser for.
#[derive(Default)]
pub struct Pending {
    browser: Mutex<HashMap<String, PendingBrowserRequest>>,
    plans: Mutex<HashMap<String, PendingPlan>>,
}

/// Every critical section here is a map edit with no await and no IO in it.
fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Pending {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_browser(&self, request_id: String, entry: PendingBrowserRequest) {
        guard(&self.browser).insert(request_id, entry);
    }

    /// Take a request out to answer it. `None` when it was already answered,
    /// expired or released: exactly one answer ever reaches the waiting turn.
    pub fn take_browser(&self, request_id: &str) -> Option<PendingBrowserRequest> {
        guard(&self.browser).remove(request_id)
    }

    pub fn browser_len(&self) -> usize {
        guard(&self.browser).len()
    }

    /// Settle every request past its deadline (refused) or whose waiter is
    /// gone, and say so to the connection showing each one.
    pub fn expire(&self, now: Instant) -> Vec<Announcement> {
        let due: Vec<(String, PendingBrowserRequest)> = {
            let mut map = guard(&self.browser);
            let ids: Vec<String> = map
                .iter()
                .filter(|(_, e)| e.sender.is_closed() || now >= e.deadline)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| map.remove(&id).map(|e| (id, e)))
                .collect()
        };
        due.into_iter()
            .map(|(id, entry)| {
                let (reason, why) = if entry.sender.is_closed() {
                    (REASON_STOPPED, "the turn stopped waiting")
                } else {
                    (REASON_TIMEOUT, "nobody answered in time")
                };
                settle_browser(&id, entry, reason, why)
            })
            .collect()
    }

    pub fn insert_plan(&self, session_id: &str, plan: PendingPlan) {
        guard(&self.plans).insert(session_id.to_string(), plan);
    }

    /// Take a session's plan out to answer it; `None` when nothing waits.
    pub fn take_plan(&self, session_id: &str) -> Option<PendingPlan> {
        guard(&self.plans).remove(session_id)
    }

    /// A plan nobody answered by its hold: cancelled, and announced as a
    /// timeout. `None` when it was already answered or released.
    pub fn time_out_plan(&self, session_id: &str) -> Option<Announcement> {
        self.take_plan(session_id)
            .map(|plan| settle_plan(session_id, plan, REASON_TIMEOUT))
    }

    /// The chat turn's wait for `session_id`'s plan: its answer (`None` when
    /// the answering side went away) and, when `hold` passed first, the plan
    /// cancelled here and the announcement for the connection showing it. A
    /// delegated plan is held thirty minutes; a local one (`hold: None`)
    /// waits however long it takes.
    pub async fn await_plan(
        &self,
        session_id: &str,
        rx: oneshot::Receiver<PlanResponse>,
        hold: Option<Duration>,
    ) -> (Option<PlanResponse>, Option<Announcement>) {
        match wait_plan(rx, hold).await {
            PlanWait::Answered(response) => (Some(response), None),
            PlanWait::TimedOut => (
                Some(PlanResponse::Cancelled),
                self.time_out_plan(session_id),
            ),
            PlanWait::Dropped => (None, None),
        }
    }

    /// Settle everything `session_id` is waiting on, because its turn was
    /// stopped (M3 §5.1): each question refused, its plan cancelled, and the
    /// connection showing each dialog told it is gone. A stopped turn parked
    /// on a question or a plan polls neither the interrupt flag nor the stream
    /// token, so this is the only thing that ends its wait.
    pub fn release_session(&self, session_id: &str) -> Vec<Announcement> {
        let taken: Vec<(String, PendingBrowserRequest)> = {
            let mut map = guard(&self.browser);
            let ids: Vec<String> = map
                .iter()
                .filter(|(_, e)| e.session_id == session_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| map.remove(&id).map(|e| (id, e)))
                .collect()
        };
        let mut out: Vec<Announcement> = taken
            .into_iter()
            .map(|(id, entry)| settle_browser(&id, entry, REASON_STOPPED, "the turn was stopped"))
            .collect();
        if let Some(plan) = self.take_plan(session_id) {
            out.push(settle_plan(session_id, plan, REASON_STOPPED));
        }
        out
    }
}

/// How a wait for a plan answer ended.
#[derive(Debug, PartialEq)]
pub enum PlanWait {
    Answered(PlanResponse),
    /// `hold` passed with no answer.
    TimedOut,
    /// The answering side went away.
    Dropped,
}

/// Wait for a plan answer. `None` waits as long as it takes — a local plan
/// keeps its behaviour; a delegated one is held thirty minutes (`Pending::await_plan`). A
/// stop always ends the wait, through [`Pending::release_session`].
pub async fn wait_plan(rx: oneshot::Receiver<PlanResponse>, hold: Option<Duration>) -> PlanWait {
    match hold {
        None => match rx.await {
            Ok(response) => PlanWait::Answered(response),
            Err(_) => PlanWait::Dropped,
        },
        Some(hold) => match tokio::time::timeout(hold, rx).await {
            Ok(Ok(response)) => PlanWait::Answered(response),
            Ok(Err(_)) => PlanWait::Dropped,
            Err(_) => PlanWait::TimedOut,
        },
    }
}

/// Cancel a plan already removed from the registry and build the announcement
/// for the connection showing it. The plan counterpart of [`settle_browser`].
fn settle_plan(session_id: &str, plan: PendingPlan, reason: &str) -> Announcement {
    let announcement = Announcement {
        client_identity: plan.client_identity,
        proxy_id: plan.proxy_id,
        frame: plan_resolved(session_id, &PlanResponse::Cancelled, Some(reason)),
    };
    let _ = plan.sender.send(PlanResponse::Cancelled);
    announcement
}

/// Refuse a browser request that has already been removed from the registry
/// and build the announcement for the connection showing its dialog. The one
/// place a request is settled without an answer, shared by the sweeper and
/// by releasing a stopped session.
fn settle_browser(
    request_id: &str,
    entry: PendingBrowserRequest,
    reason: &str,
    why: &str,
) -> Announcement {
    let announcement = Announcement {
        client_identity: entry.client_identity,
        proxy_id: entry.proxy_id,
        frame: browser_resolved(request_id, &entry.session_id, Some(reason)),
    };
    let _ = entry.sender.send(withdrawn(request_id, why));
    announcement
}

/// The refusal a waiter gets when its question will never be answered.
pub fn withdrawn(request_id: &str, why: &str) -> BrowserResponse {
    BrowserResponse {
        request_id: request_id.to_string(),
        success: false,
        result: None,
        error: Some(BrowserToolError {
            code: -1,
            message: why.to_string(),
            recoverable: false,
        }),
    }
}

/// `browser_tool_resolved`: the question `request_id` is settled. `reason` is
/// present only when nobody answered it (`timeout`, `stopped`).
pub fn browser_resolved(request_id: &str, session_id: &str, reason: Option<&str>) -> Value {
    let mut payload = json!({ "request_id": request_id, "session_id": session_id });
    if let Some(reason) = reason {
        payload["reason"] = json!(reason);
    }
    json!({ "type": "browser_tool_resolved", "payload": payload })
}

/// `plan_resolved`: the session's plan is settled with `response`.
pub fn plan_resolved(session_id: &str, response: &PlanResponse, reason: Option<&str>) -> Value {
    let mut payload = json!({ "session_id": session_id, "response": response });
    if let Some(reason) = reason {
        payload["reason"] = json!(reason);
    }
    json!({ "type": "plan_resolved", "payload": payload })
}

/// How long a question from `session_id` is held: thirty minutes while a
/// delegated task runs in that session (M3 §3.8), ten otherwise.
pub fn hold_for(session_id: &str) -> Duration {
    crate::remote::task::hold_for(session_id).unwrap_or(LOCAL_HOLD)
}

/// The daemon's one set of registries. The message loop, the sweeper and the
/// task engine all settle entries here, which is what makes "first answer
/// wins" true across them.
pub fn shared() -> Arc<Pending> {
    static SHARED: OnceLock<Arc<Pending>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(Pending::new())).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        session: &str,
        deadline: Instant,
    ) -> (PendingBrowserRequest, oneshot::Receiver<BrowserResponse>) {
        let (tx, rx) = oneshot::channel();
        (
            PendingBrowserRequest {
                session_id: session.into(),
                proxy_id: "sidebar-1".into(),
                client_identity: b"sidebar-1".to_vec(),
                deadline,
                sender: tx,
            },
            rx,
        )
    }

    #[test]
    fn an_entry_past_its_deadline_is_refused_and_announced() {
        let p = Pending::new();
        let now = Instant::now();
        let (e, mut rx) = entry("s1", now + LOCAL_HOLD);
        p.insert_browser("r1".into(), e);
        assert!(
            p.expire(now + LOCAL_HOLD - Duration::from_secs(1)).is_empty(),
            "not due yet"
        );
        let out = p.expire(now + LOCAL_HOLD);
        assert_eq!(
            out,
            vec![Announcement {
                client_identity: b"sidebar-1".to_vec(),
                proxy_id: "sidebar-1".into(),
                frame: json!({"type": "browser_tool_resolved",
                              "payload": {"request_id": "r1", "session_id": "s1", "reason": "timeout"}}),
            }]
        );
        let answer = rx.try_recv().expect("the waiting turn is answered, not left to time out");
        assert!(!answer.success);
        assert_eq!(p.browser_len(), 0);
    }

    #[test]
    fn an_abandoned_entry_is_announced_as_stopped() {
        let p = Pending::new();
        let now = Instant::now();
        let (e, rx) = entry("s1", now + LOCAL_HOLD);
        p.insert_browser("r1".into(), e);
        drop(rx);
        let out = p.expire(now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].frame["payload"]["reason"], "stopped");
    }

    #[test]
    fn every_entry_keeps_its_own_deadline() {
        let p = Pending::new();
        let now = Instant::now();
        let (local, _keep_local) = entry("local", now + LOCAL_HOLD);
        let (delegated, _keep_delegated) = entry("delegated", now + Duration::from_secs(1800));
        p.insert_browser("short".into(), local);
        p.insert_browser("long".into(), delegated);
        let first = p.expire(now + Duration::from_secs(601));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].frame["payload"]["request_id"], "short");
        assert_eq!(p.browser_len(), 1);
        let second = p.expire(now + Duration::from_secs(1800));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].frame["payload"]["request_id"], "long");
    }

    #[test]
    fn an_entry_is_taken_once() {
        let p = Pending::new();
        let (e, _rx) = entry("s1", Instant::now() + LOCAL_HOLD);
        p.insert_browser("r1".into(), e);
        assert!(p.take_browser("r1").is_some());
        assert!(p.take_browser("r1").is_none(), "a second answer finds nothing");
    }

    #[test]
    fn a_resolved_frame_names_a_reason_only_when_there_is_one() {
        assert_eq!(
            browser_resolved("r1", "s1", None),
            json!({"type": "browser_tool_resolved", "payload": {"request_id": "r1", "session_id": "s1"}})
        );
        assert_eq!(
            plan_resolved("s1", &PlanResponse::Confirmed, None),
            json!({"type": "plan_resolved", "payload": {"session_id": "s1", "response": "confirmed"}})
        );
        assert_eq!(
            plan_resolved("s1", &PlanResponse::Cancelled, Some(REASON_TIMEOUT))["payload"]["reason"],
            "timeout"
        );
    }

    #[test]
    fn a_question_outside_a_delegated_task_is_held_ten_minutes() {
        assert_eq!(LOCAL_HOLD, Duration::from_secs(600));
        assert_eq!(hold_for("any-session"), LOCAL_HOLD);
    }

    fn plan(session: &str) -> (PendingPlan, oneshot::Receiver<PlanResponse>) {
        let _ = session;
        let (tx, rx) = oneshot::channel();
        (
            PendingPlan {
                sender: tx,
                proxy_id: "sidebar-1".into(),
                client_identity: b"sidebar-1".to_vec(),
            },
            rx,
        )
    }

    #[test]
    fn stopping_a_session_refuses_its_questions_and_announces_them() {
        let p = Pending::new();
        let far = Instant::now() + LOCAL_HOLD;
        let (mine, mut mine_rx) = entry("s1", far);
        let (other, mut other_rx) = entry("s2", far);
        p.insert_browser("r1".into(), mine);
        p.insert_browser("r2".into(), other);
        let out = p.release_session("s1");
        assert_eq!(
            out,
            vec![Announcement {
                client_identity: b"sidebar-1".to_vec(),
                proxy_id: "sidebar-1".into(),
                frame: json!({"type": "browser_tool_resolved",
                              "payload": {"request_id": "r1", "session_id": "s1", "reason": "stopped"}}),
            }]
        );
        assert!(!mine_rx.try_recv().expect("answered").success);
        assert!(other_rx.try_recv().is_err(), "another session's question is untouched");
        assert!(p.take_browser("r2").is_some());
    }

    #[test]
    fn stopping_a_session_cancels_its_plan_and_announces_it() {
        let p = Pending::new();
        let (pl, mut rx) = plan("s1");
        p.insert_plan("s1", pl);
        let out = p.release_session("s1");
        assert_eq!(
            out,
            vec![Announcement {
                client_identity: b"sidebar-1".to_vec(),
                proxy_id: "sidebar-1".into(),
                frame: json!({"type": "plan_resolved",
                              "payload": {"session_id": "s1", "response": "cancelled", "reason": "stopped"}}),
            }]
        );
        assert_eq!(rx.try_recv().unwrap(), PlanResponse::Cancelled);
        assert!(p.release_session("s1").is_empty(), "nothing is left to release");
    }

    #[test]
    fn a_plan_past_its_hold_is_cancelled_and_announced_as_a_timeout() {
        let p = Pending::new();
        let (pl, mut rx) = plan("s1");
        p.insert_plan("s1", pl);
        let a = p.time_out_plan("s1").expect("announced");
        assert_eq!(a.frame["payload"]["reason"], "timeout");
        assert_eq!(rx.try_recv().unwrap(), PlanResponse::Cancelled);
        assert!(p.time_out_plan("s1").is_none());
    }

    #[tokio::test]
    async fn a_turn_waiting_on_its_plan_ends_when_the_session_is_stopped() {
        let p = Arc::new(Pending::new());
        let (pl, rx) = plan("s1");
        p.insert_plan("s1", pl);
        let waiting = tokio::spawn(wait_plan(rx, None));
        tokio::task::yield_now().await;
        p.release_session("s1");
        assert_eq!(
            waiting.await.unwrap(),
            PlanWait::Answered(PlanResponse::Cancelled)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_plan_without_a_hold_waits_however_long_it_takes() {
        let (tx, rx) = oneshot::channel();
        let waiting = tokio::spawn(wait_plan(rx, None));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(48 * 3600)).await;
        assert!(!waiting.is_finished());
        tx.send(PlanResponse::Confirmed).unwrap();
        assert_eq!(
            waiting.await.unwrap(),
            PlanWait::Answered(PlanResponse::Confirmed)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_plan_with_a_hold_times_out_at_it() {
        let (_tx, rx) = oneshot::channel::<PlanResponse>();
        let waiting = tokio::spawn(wait_plan(rx, Some(Duration::from_secs(1800))));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1799)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(waiting.await.unwrap(), PlanWait::TimedOut);
    }

    #[tokio::test]
    async fn a_plan_whose_answerer_vanished_reports_dropped() {
        let (tx, rx) = oneshot::channel::<PlanResponse>();
        drop(tx);
        assert_eq!(wait_plan(rx, None).await, PlanWait::Dropped);
    }

    #[tokio::test(start_paused = true)]
    async fn a_delegated_plan_nobody_answers_is_cancelled_and_announced_at_its_hold() {
        let p = Arc::new(Pending::new());
        let (pl, rx) = plan("s1");
        p.insert_plan("s1", pl);
        let waiter = p.clone();
        let waiting = tokio::spawn(async move {
            waiter
                .await_plan("s1", rx, Some(Duration::from_secs(1800)))
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1799)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        tokio::time::advance(Duration::from_secs(2)).await;
        let (answer, announced) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the hold ended the wait")
            .unwrap();
        assert_eq!(answer, Some(PlanResponse::Cancelled));
        assert_eq!(
            announced,
            Some(Announcement {
                client_identity: b"sidebar-1".to_vec(),
                proxy_id: "sidebar-1".into(),
                frame: json!({"type": "plan_resolved",
                              "payload": {"session_id": "s1", "response": "cancelled", "reason": "timeout"}}),
            })
        );
        assert!(p.take_plan("s1").is_none(), "nothing is left to answer");
    }

    #[tokio::test]
    async fn an_answered_plan_announces_nothing_from_the_wait() {
        let p = Pending::new();
        let (pl, rx) = plan("s1");
        p.insert_plan("s1", pl);
        let taken = p.take_plan("s1").unwrap();
        taken.sender.send(PlanResponse::Confirmed).unwrap();
        assert_eq!(
            p.await_plan("s1", rx, Some(Duration::from_secs(1800))).await,
            (Some(PlanResponse::Confirmed), None)
        );
    }
}
