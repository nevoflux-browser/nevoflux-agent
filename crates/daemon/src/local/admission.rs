//! Token-budget admission controller for the local engine's parallel slots.
//!
//! The local engine runs with a finite `parallel` slot count and a finite
//! KV-cache token budget (v3 decision D7, §17). This module decides which
//! requests get to consume that budget right now:
//!
//! - **Interactive (P0)** requests -- the user's own chat turn -- queue
//!   FIFO with no timeout and are never refused for the engine being
//!   unready. If the budget is too tight to admit a waiting P0 request, it
//!   preempts the newest running **Background (P1)** permits (memory
//!   extraction, knowledge consolidation, ...) instead of waiting behind
//!   them.
//! - **Background (P1)** requests are refused outright
//!   ([`AdmissionError::Deferred`]) while the engine isn't ready, are only
//!   ever admitted when no P0 request is waiting or in flight, and may be
//!   preempted mid-flight by a P0 arrival -- see [`Permit::cancel`].
//!
//! [`Priority`] is carried via the ambient [`PRIORITY`] task-local rather
//! than threaded through every call, since the priority a request runs at
//! is a property of *what kind of work the current task is doing*
//! (interactive turn vs. background job), not of any one function's
//! arguments. [`background`] is how a background job opts in; a plain
//! `tokio::spawn` does **not** inherit the caller's task-local (task-locals
//! are per-task, and `spawn` starts a brand new task) -- a caller that
//! spawns background work must wrap the *spawned future's own body* in
//! [`background`], not just call it around the `spawn` site itself. See
//! `background_scopes_priority_for_the_call_but_not_across_spawn` below,
//! and the two `tokio::spawn` call sites this task wires in `server.rs`
//! (`consolidate_category`, `extract_session_memories`).
//!
//! ## Test isolation
//!
//! [`admission`] is a process-wide `OnceLock` singleton -- production
//! wiring only ([`crate::wasm::local_llm::admission_hook`] is its one
//! caller). Unit tests here construct their own [`Admission::new`] instance
//! and exercise that directly instead, so they can never race each other
//! (or any other test in the crate) over shared global state -- this
//! module deliberately does not add a test-serialization mutex, because
//! there is nothing shared left for one to protect.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::wasm::llm::LlmChatRequest;

/// Request priority for admission into the local engine's parallel slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// The user's own chat turn. Never waits behind background work, and
    /// preempts running background work when the token budget is short.
    Interactive,
    /// Background work (memory extraction, knowledge consolidation, ...).
    /// Deferred while the engine isn't ready, admitted only when no
    /// interactive request is waiting or in flight, and preemptible.
    Background,
}

tokio::task_local! {
    /// The priority the CURRENT TASK's local-engine calls run at. Set only
    /// inside [`background`]'s scope; unset (the ambient default) reads as
    /// [`Priority::Interactive`] via [`current_priority`].
    pub static PRIORITY: Priority;
}

/// The calling task's priority: [`Priority::Interactive`] unless running
/// inside [`background`]'s scope.
pub fn current_priority() -> Priority {
    PRIORITY.try_with(|p| *p).unwrap_or(Priority::Interactive)
}

/// Run `f` with [`PRIORITY`] set to [`Priority::Background`] for its
/// duration, and for anything it `.await`s inline. Does **not** extend to
/// anything `f` itself hands off to `tokio::spawn` -- that spawned future
/// needs its own `background(...)` wrapper around its body to see
/// [`Priority::Background`] too (task-locals don't cross a `spawn`
/// boundary; see the module docs).
pub async fn background<F: Future>(f: F) -> F::Output {
    PRIORITY.scope(Priority::Background, f).await
}

/// Estimate a request's token footprint for admission purposes:
/// `(system + messages + tools JSON bytes) / 3 + max_tokens.unwrap_or(2048)`.
///
/// The `/3` chars-per-token approximation is deliberately crude -- this
/// gates a coarse token *budget*, not a billing calculation, and the local
/// engine's own tokenizer isn't available to this process without an HTTP
/// round-trip it hasn't made yet.
pub fn estimate_request_tokens(req: &LlmChatRequest) -> u32 {
    let system_bytes = req.system.as_deref().map(str::len).unwrap_or(0);
    let messages_bytes = serde_json::to_string(&req.messages)
        .map(|s| s.len())
        .unwrap_or(0);
    let tools_bytes = req
        .tools
        .as_ref()
        .and_then(|t| serde_json::to_string(t).ok())
        .map(|s| s.len())
        .unwrap_or(0);
    let prompt_bytes = system_bytes + messages_bytes + tools_bytes;
    let prompt_tokens = u32::try_from(prompt_bytes / 3).unwrap_or(u32::MAX);
    let max_tokens = req.max_tokens.unwrap_or(2048);
    prompt_tokens.saturating_add(max_tokens)
}

/// Something [`Admission::acquire`] could not do.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum AdmissionError {
    /// A background (P1) request while the engine isn't ready yet.
    #[error("background work deferred: engine not ready")]
    Deferred,
    /// The request's estimated tokens exceed the whole context pool -- no
    /// amount of waiting or preemption would ever admit it.
    #[error("request larger than the context pool")]
    TooLarge,
    /// Too many requests are already waiting to be admitted.
    #[error("queue full")]
    QueueFull,
    /// A background (P1) permit was cancelled to make room for a waiting
    /// interactive (P0) request.
    #[error("preempted by an interactive request")]
    Preempted,
}

/// One admitted request's slot in [`Admission`]'s token budget.
///
/// Releases its tokens back to the budget when dropped, on every exit path
/// alike (success, error, or the caller noticing [`Permit::cancel`] fire
/// and giving up) -- there is no separate explicit "release" call.
pub struct Permit {
    id: u64,
    inner: Arc<Inner>,
    /// Fires when [`Admission`] preempts this permit to admit a waiting
    /// interactive request that the token budget was otherwise too short
    /// for. A caller holding a `Permit` for the duration of its HTTP call
    /// must `select!` this against that call; on firing, drop the response
    /// and return [`AdmissionError::Preempted`] (dropping this `Permit` as
    /// part of that unwind releases its tokens like any other exit).
    pub cancel: CancellationToken,
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permit")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.inner.release(self.id);
    }
}

#[derive(Debug)]
struct Entry {
    id: u64,
    prio: Priority,
    tokens: u32,
    cancel: CancellationToken,
}

#[derive(Default)]
struct State {
    next_id: u64,
    next_ticket: u64,
    in_flight: Vec<Entry>,
    /// Tickets of interactive (P0) `acquire` calls waiting to be admitted,
    /// oldest first. Only the ticket at the front may attempt admission --
    /// that, not scheduling luck, is what makes P0 admission strictly FIFO.
    p0_queue: VecDeque<u64>,
    /// Count of background (P1) `acquire` calls currently waiting (not yet
    /// admitted). P1 has no ordering requirement among itself, so this is
    /// just a count against `queue_depth`, not a queue.
    waiting_p1: usize,
}

fn in_flight_tokens(st: &State) -> u32 {
    st.in_flight
        .iter()
        .fold(0u32, |acc, e| acc.saturating_add(e.tokens))
}

/// Cancel the newest not-yet-cancelled background (P1) permits, in
/// descending id order, until their combined tokens would (once actually
/// released) cover `need` -- or until there are none left to cancel.
///
/// Only *signals* cancellation; the tokens themselves are freed later, when
/// each preempted caller notices [`Permit::cancel`] and drops its `Permit`.
/// Idempotent across repeated calls with the same still-short state
/// (already-cancelled entries are skipped), so the acquire loop can call
/// this on every iteration it remains short without over-cancelling.
fn preempt_newest_background(st: &mut State, mut need: u32) {
    if need == 0 {
        return;
    }
    let mut candidates: Vec<u64> = st
        .in_flight
        .iter()
        .filter(|e| e.prio == Priority::Background && !e.cancel.is_cancelled())
        .map(|e| e.id)
        .collect();
    candidates.sort_unstable_by(|a, b| b.cmp(a)); // newest (highest id) first
    for id in candidates {
        if need == 0 {
            break;
        }
        if let Some(e) = st.in_flight.iter().find(|e| e.id == id) {
            e.cancel.cancel();
            need = need.saturating_sub(e.tokens);
        }
    }
}

struct Inner {
    capacity_tokens: u32,
    queue_depth: usize,
    state: Mutex<State>,
    notify: Notify,
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn release(&self, id: u64) {
        {
            let mut st = self.lock();
            st.in_flight.retain(|e| e.id != id);
        }
        self.notify.notify_waiters();
    }
}

/// Token-budget admission controller. See the module docs.
///
/// Cheaply cloneable (internally an `Arc` handle over shared state) so
/// tests can share one instance across `tokio::spawn`ed tasks without
/// wrapping it themselves; the production singleton ([`admission`]) is
/// reached through a `&'static` reference instead, for which this doesn't
/// matter either way.
#[derive(Clone)]
pub struct Admission {
    inner: Arc<Inner>,
}

impl Admission {
    /// `capacity_tokens` is the whole KV-cache token budget shared across
    /// every in-flight request; `queue_depth` caps how many requests of a
    /// given priority may wait to be admitted at once (the plan's
    /// `max(16*parallel, 64)`) before further arrivals get
    /// [`AdmissionError::QueueFull`].
    pub fn new(capacity_tokens: u32, queue_depth: usize) -> Self {
        Admission {
            inner: Arc::new(Inner {
                capacity_tokens,
                queue_depth,
                state: Mutex::new(State::default()),
                notify: Notify::new(),
            }),
        }
    }

    /// Count of currently in-flight (admitted, not yet released) requests
    /// of any priority.
    pub fn in_flight(&self) -> usize {
        self.inner.lock().in_flight.len()
    }

    /// Whether any interactive (P0) request is in flight or waiting to be
    /// admitted. Background (P1) activity never counts, by design: this is
    /// for the engine's idle-unload timer, and a busy background job alone
    /// must not keep the engine loaded against a user who has walked away.
    pub fn has_interactive_activity(&self) -> bool {
        let st = self.inner.lock();
        !st.p0_queue.is_empty() || st.in_flight.iter().any(|e| e.prio == Priority::Interactive)
    }

    /// Acquire an admission slot for `tokens`, at priority `prio`, given
    /// whether the engine is currently ready (`engine_ready` gates only
    /// [`Priority::Background`] -- see [`AdmissionError::Deferred`]).
    ///
    /// Resolves once admitted; an interactive request waits as long as it
    /// takes (FIFO, no timeout) rather than ever returning a transient
    /// error, short of [`AdmissionError::TooLarge`] or
    /// [`AdmissionError::QueueFull`] (both permanent for this call).
    pub async fn acquire(
        &self,
        prio: Priority,
        tokens: u32,
        engine_ready: bool,
    ) -> Result<Permit, AdmissionError> {
        if tokens > self.inner.capacity_tokens {
            return Err(AdmissionError::TooLarge);
        }
        if prio == Priority::Background && !engine_ready {
            return Err(AdmissionError::Deferred);
        }

        match prio {
            Priority::Interactive => self.acquire_interactive(tokens).await,
            Priority::Background => self.acquire_background(tokens).await,
        }
    }

    async fn acquire_interactive(&self, tokens: u32) -> Result<Permit, AdmissionError> {
        let ticket = {
            let mut st = self.inner.lock();
            if st.p0_queue.len() >= self.inner.queue_depth {
                return Err(AdmissionError::QueueFull);
            }
            let ticket = st.next_ticket;
            st.next_ticket += 1;
            st.p0_queue.push_back(ticket);
            ticket
        };

        // Guarantees the ticket is removed from the queue on every exit --
        // including this future being dropped before it wins admission --
        // so a caller that gives up (e.g. its own outer timeout) can never
        // leave a phantom ticket permanently blocking the FIFO head.
        struct TicketGuard<'a> {
            inner: &'a Inner,
            ticket: u64,
            admitted: bool,
        }
        impl Drop for TicketGuard<'_> {
            fn drop(&mut self) {
                if !self.admitted {
                    {
                        let mut st = self.inner.lock();
                        st.p0_queue.retain(|t| *t != self.ticket);
                    }
                    self.inner.notify.notify_waiters();
                }
            }
        }
        let mut guard = TicketGuard {
            inner: &self.inner,
            ticket,
            admitted: false,
        };

        loop {
            // Constructed BEFORE inspecting state, not after: `Notify`
            // captures the current notification epoch at construction, so
            // a `notify_waiters()` racing in between this line and the
            // `.await` below is still observed (the first `poll` sees the
            // epoch has moved and resolves immediately) rather than lost.
            let notified = self.inner.notify.notified();
            {
                let mut st = self.inner.lock();
                if st.p0_queue.front() == Some(&ticket) {
                    let used = in_flight_tokens(&st);
                    if used.saturating_add(tokens) <= self.inner.capacity_tokens {
                        st.p0_queue.pop_front();
                        let id = st.next_id;
                        st.next_id += 1;
                        let cancel = CancellationToken::new();
                        st.in_flight.push(Entry {
                            id,
                            prio: Priority::Interactive,
                            tokens,
                            cancel: cancel.clone(),
                        });
                        guard.admitted = true;
                        drop(st);
                        // The queue head moved (or emptied) -- wake the
                        // next-in-line P0 ticket-holder and any P1 waiter
                        // whose "no P0 waiting" condition may now hold.
                        self.inner.notify.notify_waiters();
                        return Ok(Permit {
                            id,
                            inner: Arc::clone(&self.inner),
                            cancel,
                        });
                    }
                    let need = used.saturating_add(tokens) - self.inner.capacity_tokens;
                    preempt_newest_background(&mut st, need);
                }
            }
            notified.await;
        }
    }

    async fn acquire_background(&self, tokens: u32) -> Result<Permit, AdmissionError> {
        {
            let mut st = self.inner.lock();
            if st.waiting_p1 >= self.inner.queue_depth {
                return Err(AdmissionError::QueueFull);
            }
            st.waiting_p1 += 1;
        }

        struct WaitGuard<'a> {
            inner: &'a Inner,
        }
        impl Drop for WaitGuard<'_> {
            fn drop(&mut self) {
                let mut st = self.inner.lock();
                st.waiting_p1 = st.waiting_p1.saturating_sub(1);
            }
        }
        let _guard = WaitGuard { inner: &self.inner };

        loop {
            // See the matching comment in `acquire_interactive`.
            let notified = self.inner.notify.notified();
            {
                let mut st = self.inner.lock();
                let no_p0_waiting_or_in_flight = st.p0_queue.is_empty()
                    && !st.in_flight.iter().any(|e| e.prio == Priority::Interactive);
                let used = in_flight_tokens(&st);
                if no_p0_waiting_or_in_flight
                    && used.saturating_add(tokens) <= self.inner.capacity_tokens
                {
                    let id = st.next_id;
                    st.next_id += 1;
                    let cancel = CancellationToken::new();
                    st.in_flight.push(Entry {
                        id,
                        prio: Priority::Background,
                        tokens,
                        cancel: cancel.clone(),
                    });
                    return Ok(Permit {
                        id,
                        inner: Arc::clone(&self.inner),
                        cancel,
                    });
                }
            }
            notified.await;
        }
    }
}

/// Process-wide [`Admission`] singleton for the local engine.
///
/// `capacity_tokens`/`queue_depth` here are a conservative placeholder,
/// not a real reading of the running engine: [`crate::local::config::CTX_FLOOR`]
/// is the minimum context size any local install is allowed to run at
/// (real installs may run larger), and `queue_depth` applies the plan's
/// `max(16*parallel, 64)` at [`crate::local::config::LocalConfig`]'s
/// default `parallel` (2). Both real values are only known once an engine
/// is actually installed and launched -- this module has no dependency on
/// that (Task 2.9's engine supervisor does) and a `OnceLock` can only be
/// initialized once, so reconciling this with the real per-install numbers
/// is left to whoever wires the supervisor up to this singleton.
pub fn admission() -> &'static Admission {
    static ADMISSION: OnceLock<Admission> = OnceLock::new();
    ADMISSION.get_or_init(|| {
        let default_parallel = crate::local::config::LocalConfig::default().parallel;
        let queue_depth = (16 * default_parallel).max(64) as usize;
        Admission::new(crate::local::config::CTX_FLOOR, queue_depth)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::llm::LlmMessage;
    use std::time::Duration;

    /// Let already-spawned tasks progress under paused tokio time without
    /// costing any real wall-clock time -- a bare `yield_now()` isn't
    /// always enough to let a task register as a `Notify` waiter and then
    /// actually park, so this advances virtual time a tick instead.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    #[tokio::test]
    async fn too_large_request_is_rejected_and_never_enters_the_queue() {
        let a = Admission::new(1000, 4);
        let err = a
            .acquire(Priority::Interactive, 1001, true)
            .await
            .unwrap_err();
        assert_eq!(err, AdmissionError::TooLarge);
        assert_eq!(a.in_flight(), 0);
        assert!(!a.has_interactive_activity());
    }

    #[tokio::test]
    async fn background_is_deferred_when_engine_not_ready() {
        let a = Admission::new(1000, 4);
        let err = a
            .acquire(Priority::Background, 100, false)
            .await
            .unwrap_err();
        assert_eq!(err, AdmissionError::Deferred);
        assert_eq!(a.in_flight(), 0);
    }

    #[tokio::test]
    async fn permit_releases_its_tokens_on_drop() {
        let a = Admission::new(100, 4);
        let p = a.acquire(Priority::Interactive, 100, true).await.unwrap();
        assert_eq!(a.in_flight(), 1);
        drop(p);
        assert_eq!(a.in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn background_waits_while_interactive_is_in_flight() {
        let a = Admission::new(1000, 4);
        let p0 = a.acquire(Priority::Interactive, 100, true).await.unwrap();
        assert!(a.has_interactive_activity());

        let a2 = a.clone();
        let p1_task =
            tokio::spawn(async move { a2.acquire(Priority::Background, 100, true).await });
        settle().await;
        assert!(
            !p1_task.is_finished(),
            "background must not be admitted while interactive is in flight, even though \
             there is plenty of budget for both"
        );

        drop(p0);
        let p1 = p1_task
            .await
            .unwrap()
            .expect("background admits once interactive drains");
        assert_eq!(a.in_flight(), 1);
        assert!(!a.has_interactive_activity());
        drop(p1);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_admission_is_strictly_fifo() {
        let a = Admission::new(100, 4);
        let first = a.acquire(Priority::Interactive, 100, true).await.unwrap();

        let a2 = a.clone();
        let second_task =
            tokio::spawn(async move { a2.acquire(Priority::Interactive, 100, true).await });
        settle().await;

        let a3 = a.clone();
        let third_task =
            tokio::spawn(async move { a3.acquire(Priority::Interactive, 100, true).await });
        settle().await;

        assert!(!second_task.is_finished());
        assert!(!third_task.is_finished());
        assert!(a.has_interactive_activity());

        // Freeing the budget must admit the SECOND arrival, not the third,
        // regardless of any scheduling noise between the two waiting tasks.
        drop(first);
        settle().await;
        assert!(
            !third_task.is_finished(),
            "the third arrival must not jump the FIFO queue ahead of the second"
        );
        let second = second_task
            .await
            .unwrap()
            .expect("second arrival admitted once budget frees");

        drop(second);
        let third = third_task
            .await
            .unwrap()
            .expect("third arrival admitted once the second releases");
        drop(third);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_arrival_preempts_only_the_newest_background_permit_needed_to_fit() {
        let a = Admission::new(300, 4);
        let old_bg = a.acquire(Priority::Background, 100, true).await.unwrap();
        let new_bg = a.acquire(Priority::Background, 150, true).await.unwrap();
        assert_eq!(a.in_flight(), 2);

        let a2 = a.clone();
        let p0_task =
            tokio::spawn(async move { a2.acquire(Priority::Interactive, 100, true).await });
        settle().await;

        assert!(
            new_bg.cancel.is_cancelled(),
            "the newest background permit must be preempted to free enough budget"
        );
        assert!(
            !old_bg.cancel.is_cancelled(),
            "the older background permit must be left alone once cancelling the newer \
             one alone frees enough budget"
        );
        assert!(
            !p0_task.is_finished(),
            "the interactive request must still wait for the preempted permit to actually \
             be dropped -- cancelling only signals, it doesn't free tokens by itself"
        );

        // The preempted caller notices `cancel` and gives up.
        drop(new_bg);
        let p0 = p0_task
            .await
            .unwrap()
            .expect("interactive admits once the preempted permit's tokens are released");
        assert_eq!(a.in_flight(), 2); // old_bg + p0
        drop(old_bg);
        drop(p0);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_queue_full_is_rejected_without_disturbing_the_existing_waiter() {
        let a = Admission::new(10, 1);
        let held = a.acquire(Priority::Interactive, 10, true).await.unwrap();

        let a2 = a.clone();
        let waiter = tokio::spawn(async move { a2.acquire(Priority::Interactive, 10, true).await });
        settle().await;

        let err = a
            .acquire(Priority::Interactive, 10, true)
            .await
            .unwrap_err();
        assert_eq!(err, AdmissionError::QueueFull);

        drop(held);
        waiter
            .await
            .unwrap()
            .expect("the original waiter is unaffected by the later rejected arrival");
    }

    #[tokio::test]
    async fn has_interactive_activity_ignores_background_entirely() {
        let a = Admission::new(1000, 4);
        assert!(!a.has_interactive_activity());

        let p1 = a.acquire(Priority::Background, 100, true).await.unwrap();
        assert!(
            !a.has_interactive_activity(),
            "background alone must not count as interactive activity"
        );

        let p0 = a.acquire(Priority::Interactive, 100, true).await.unwrap();
        assert!(a.has_interactive_activity());

        drop(p0);
        assert!(!a.has_interactive_activity());
        drop(p1);
    }

    #[tokio::test]
    async fn current_priority_defaults_to_interactive() {
        assert_eq!(current_priority(), Priority::Interactive);
    }

    /// Guards the exact subtlety this task was warned about: a
    /// `tokio::task_local!` does not cross a `tokio::spawn` boundary on its
    /// own. `background(...)` must wrap the SPAWNED future's own body to
    /// have any effect inside it.
    #[tokio::test]
    async fn background_scopes_priority_inline_but_not_across_a_bare_spawn() {
        let seen = background(async { current_priority() }).await;
        assert_eq!(seen, Priority::Background);

        let seen_in_bare_spawn =
            background(async { tokio::spawn(async { current_priority() }).await.unwrap() }).await;
        assert_eq!(
            seen_in_bare_spawn,
            Priority::Interactive,
            "PRIORITY must not leak into a bare tokio::spawn without an explicit wrapper"
        );

        let seen_when_wrapped_correctly = tokio::spawn(background(async { current_priority() }))
            .await
            .unwrap();
        assert_eq!(seen_when_wrapped_correctly, Priority::Background);
    }

    #[test]
    fn estimate_request_tokens_falls_back_to_2048_max_tokens_when_everything_else_is_empty() {
        let req = LlmChatRequest::default();
        assert_eq!(estimate_request_tokens(&req), 2048);
    }

    #[test]
    fn estimate_request_tokens_adds_prompt_bytes_over_three_to_explicit_max_tokens() {
        let req = LlmChatRequest {
            system: Some("x".repeat(300)),
            messages: vec![LlmMessage::user("y".repeat(300))],
            max_tokens: Some(500),
            ..Default::default()
        };
        let messages_bytes = serde_json::to_string(&req.messages).unwrap().len();
        let expected_prompt_tokens = (300 + messages_bytes) as u32 / 3;

        assert_eq!(estimate_request_tokens(&req), expected_prompt_tokens + 500);
    }

    #[test]
    fn admission_singleton_is_reachable_and_stable() {
        let a = admission();
        let b = admission();
        assert!(
            std::ptr::eq(a, b),
            "admission() must return the same instance every call"
        );
    }
}
