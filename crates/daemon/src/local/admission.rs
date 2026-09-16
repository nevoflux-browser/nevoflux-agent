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
//! `background_scopes_priority_inline_but_not_across_a_bare_spawn` below,
//! and the two `tokio::spawn` call sites this task wires in `server.rs`
//! (`consolidate_category`, `extract_session_memories`).
//!
//! ## Test isolation
//!
//! [`admission`] is a process-wide `OnceLock` singleton -- production
//! wiring only (`crate::wasm::local_llm::run_admitted` is its one caller,
//! via `endpoint_for_priority` and the two `select!`s in
//! `execute_local_chat`/`stream_local`). Unit tests here construct their
//! own [`Admission::new`] instance
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
    /// The whole KV-cache token budget shared across every in-flight
    /// request. Lives here, not on `Inner`, so [`Admission::set_capacity_tokens`]
    /// can change it under the same lock every admission decision already
    /// reads `in_flight` under -- no second synchronisation domain, no
    /// capacity-vs-usage torn read.
    capacity_tokens: u32,
    /// Per-priority cap on how many `acquire` calls may wait to be admitted
    /// at once before further arrivals get [`AdmissionError::QueueFull`].
    queue_depth: usize,
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
/// descending id order, until their combined tokens -- together with any
/// P1 permits already cancelled but not yet released -- would (once
/// actually released) cover `need`, or until there are none left to
/// cancel.
///
/// Only *signals* cancellation; the tokens themselves are freed later, when
/// each preempted caller notices [`Permit::cancel`] and drops its `Permit`.
/// `need` is nominally recomputed by the caller from `in_flight_tokens` on
/// every wakeup -- and that still counts an already-cancelled-but-
/// unreleased permit's tokens as "in flight", so its shortfall looks
/// unchanged until the caller actually drops it. This function nets out
/// exactly that (`pending`, below) before choosing further victims: without
/// it, every unrelated wakeup while a preempted permit is still unreleased
/// (another `Permit` drop anywhere calls `notify_waiters()`, same as a
/// successful admission or a given-up `TicketGuard` does) would see the
/// same apparent shortfall, find the already-cancelled entry excluded from
/// the candidate list, and cancel a FRESH tranche on top of it. With the
/// netting, a repeated call against unchanged state cancels nothing further
/// -- which is what makes this genuinely idempotent, not merely documented
/// as such.
fn preempt_newest_background(st: &mut State, need: u32) {
    let pending: u32 = st
        .in_flight
        .iter()
        .filter(|e| e.prio == Priority::Background && e.cancel.is_cancelled())
        .fold(0u32, |acc, e| acc.saturating_add(e.tokens));
    let mut need = need.saturating_sub(pending);
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
    /// [`AdmissionError::QueueFull`]. Both are runtime-adjustable
    /// afterward -- see [`Admission::set_capacity_tokens`] /
    /// [`Admission::set_queue_depth`] -- since neither is really a boot-time
    /// constant: the engine supervisor only learns the real `n_ctx` after
    /// launch, a backend downgrade or `local.set_config` can change it
    /// again, and a model switch is an unload/reload with a different one.
    pub fn new(capacity_tokens: u32, queue_depth: usize) -> Self {
        Admission {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    capacity_tokens,
                    queue_depth,
                    ..State::default()
                }),
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

    /// Change the shared KV-cache token budget at runtime -- e.g. once the
    /// engine supervisor learns the real `n_ctx` from `/props` after
    /// launch, on a backend downgrade that re-runs the fit, or after a
    /// `local.set_config` resize.
    ///
    /// Never evicts to enforce a shrink ("drain, don't evict"): existing
    /// permits, including interactive ones, are left exactly as they are --
    /// cancelling an interactive permit to force compliance would kill the
    /// user's live turn, and preemption is authorised only in service of a
    /// *waiting* interactive request, never to enforce a capacity change by
    /// itself. An over-subscribed budget simply admits nothing new until
    /// enough releases bring it back under the new capacity on their own.
    ///
    /// Wakes every waiter so a queued interactive request can immediately
    /// notice a capacity *increase* rather than sleeping until the next
    /// unrelated release, and so it can immediately re-evaluate (and return
    /// [`AdmissionError::TooLarge`], rather than hang forever at the FIFO
    /// head -- P0 has no timeout by spec) if a *shrink* just made its
    /// request permanently unsatisfiable.
    pub fn set_capacity_tokens(&self, tokens: u32) {
        {
            let mut st = self.inner.lock();
            st.capacity_tokens = tokens;
        }
        self.inner.notify.notify_waiters();
    }

    /// Change the per-priority waiting-queue depth cap at runtime. Only
    /// ever affects new arrivals -- a shrink never evicts a request that is
    /// already queued.
    pub fn set_queue_depth(&self, depth: usize) {
        {
            let mut st = self.inner.lock();
            st.queue_depth = depth;
        }
        self.inner.notify.notify_waiters();
    }

    /// Acquire an admission slot for `tokens`, at priority `prio`, given
    /// whether the engine is currently ready (`engine_ready` gates only
    /// [`Priority::Background`] -- see [`AdmissionError::Deferred`]).
    ///
    /// Resolves once admitted; an interactive request waits as long as it
    /// takes (FIFO, no timeout) rather than ever returning a transient
    /// error, short of [`AdmissionError::TooLarge`] or
    /// [`AdmissionError::QueueFull`] (both permanent for this call --
    /// though `TooLarge` is re-tested on every wakeup while queued, in case
    /// a capacity shrink makes an already-queued request newly too large).
    pub async fn acquire(
        &self,
        prio: Priority,
        tokens: u32,
        engine_ready: bool,
    ) -> Result<Permit, AdmissionError> {
        if tokens > self.inner.lock().capacity_tokens {
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
            if st.p0_queue.len() >= st.queue_depth {
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
                // Re-tested on every wakeup, regardless of queue position:
                // a capacity shrink (`set_capacity_tokens`) can make an
                // already-queued request permanently unsatisfiable, and P0
                // has no timeout by spec -- without this, such a request
                // would hang forever at (or behind) the FIFO head, blocking
                // every interactive arrival behind it too.
                if tokens > st.capacity_tokens {
                    return Err(AdmissionError::TooLarge);
                }
                if st.p0_queue.front() == Some(&ticket) {
                    let used = in_flight_tokens(&st);
                    if used.saturating_add(tokens) <= st.capacity_tokens {
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
                    let need = used.saturating_add(tokens) - st.capacity_tokens;
                    preempt_newest_background(&mut st, need);
                }
            }
            notified.await;
        }
    }

    async fn acquire_background(&self, tokens: u32) -> Result<Permit, AdmissionError> {
        {
            let mut st = self.inner.lock();
            if st.waiting_p1 >= st.queue_depth {
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
                // See the matching comment in `acquire_interactive` -- a
                // capacity shrink can make a queued background request
                // newly impossible too. Background has no FIFO head to
                // guard, so this can't block anyone else, but it still
                // must not wait forever for a budget that will never come.
                if tokens > st.capacity_tokens {
                    return Err(AdmissionError::TooLarge);
                }
                let no_p0_waiting_or_in_flight = st.p0_queue.is_empty()
                    && !st.in_flight.iter().any(|e| e.prio == Priority::Interactive);
                let used = in_flight_tokens(&st);
                if no_p0_waiting_or_in_flight && used.saturating_add(tokens) <= st.capacity_tokens {
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
/// The `OnceLock` here is for *identity* only -- every live [`Permit`]'s
/// `Arc<Inner>` and every `&'static Admission` handle must keep pointing at
/// the same instance, so this can only ever be constructed once. The
/// *budget* it starts with is not the real one: [`crate::local::config::CTX_FLOOR`]
/// is the minimum context size any local install is allowed to run at (real
/// installs may run larger), and `queue_depth` applies the plan's
/// `max(16*parallel, 64)` at [`crate::local::config::LocalConfig`]'s
/// default `parallel` (2) -- both are only known once an engine is actually
/// installed and launched, which this module has no dependency on. Task
/// 2.9's engine supervisor is expected to call
/// [`Admission::set_capacity_tokens`] with the real `n_ctx` (from
/// `self_check`/`/props`) right after launch, and again on every
/// restart/downgrade/model-switch; Task 2.10 calls it again after a
/// successful `local.set_config`. Until the first such call, admission
/// conservatively meters against the floor -- under-admitting is safe,
/// over-admitting is what makes the engine kill colliding slots.
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
    async fn a_second_wakeup_before_the_preempted_permit_releases_does_not_over_cancel() {
        let a = Admission::new(300, 4);
        let old_a = a.acquire(Priority::Background, 100, true).await.unwrap();
        let old_b = a.acquire(Priority::Background, 100, true).await.unwrap();
        let newest_c = a.acquire(Priority::Background, 50, true).await.unwrap();
        assert_eq!(a.in_flight(), 3); // used = 250

        let a2 = a.clone();
        let p0_task =
            tokio::spawn(async move { a2.acquire(Priority::Interactive, 100, true).await });
        settle().await;
        assert!(
            newest_c.cancel.is_cancelled(),
            "the newest permit alone (50) covers the 50-token shortfall"
        );
        assert!(!old_b.cancel.is_cancelled());
        assert!(!old_a.cancel.is_cancelled());

        // Trigger a SECOND wakeup of the P0 head while `newest_c` is still
        // held, unrelated to it: a second interactive arrival that gives up
        // before winning admission. `TicketGuard::drop` calls
        // `notify_waiters()`, same as a `Permit` release does -- before the
        // fix, this re-ran preemption against the same unchanged shortfall
        // (the cancelled-but-unreleased `newest_c` still counted as "in
        // flight") and cancelled a fresh victim (`old_b`) on top.
        let a3 = a.clone();
        let extra = tokio::spawn(async move { a3.acquire(Priority::Interactive, 1, true).await });
        settle().await;
        extra.abort();
        settle().await;

        assert!(
            !old_b.cancel.is_cancelled(),
            "a second wakeup while the first preempted permit is still unreleased must not \
             cancel a second victim -- the newest permit's tokens already cover the shortfall \
             once actually released"
        );
        assert!(!old_a.cancel.is_cancelled());

        drop(newest_c);
        let p0 = p0_task
            .await
            .unwrap()
            .expect("admits once the preempted permit actually releases");
        assert_eq!(a.in_flight(), 3); // old_a + old_b + p0
        drop(old_a);
        drop(old_b);
        drop(p0);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_arrival_preempts_two_background_permits_when_the_newest_alone_is_not_enough(
    ) {
        let a = Admission::new(300, 4);
        let bg_old = a.acquire(Priority::Background, 100, true).await.unwrap();
        let bg_mid = a.acquire(Priority::Background, 80, true).await.unwrap();
        let bg_new = a.acquire(Priority::Background, 30, true).await.unwrap();
        assert_eq!(a.in_flight(), 3);

        let a2 = a.clone();
        let p0_task =
            tokio::spawn(async move { a2.acquire(Priority::Interactive, 140, true).await });
        settle().await;

        assert!(
            bg_new.cancel.is_cancelled(),
            "the newest permit must be preempted first"
        );
        assert!(
            bg_mid.cancel.is_cancelled(),
            "the newest alone (30) cannot cover the 50-token shortfall (used 210 + 140 - 300), \
             so the second-newest must also be preempted"
        );
        assert!(
            !bg_old.cancel.is_cancelled(),
            "the oldest permit must be left alone once the two newer ones together free \
             enough budget"
        );

        drop(bg_new);
        drop(bg_mid);
        let p0 = p0_task
            .await
            .unwrap()
            .expect("admits once both preempted permits are actually released");
        assert_eq!(a.in_flight(), 2); // bg_old + p0
        drop(bg_old);
        drop(p0);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_arrival_waits_for_other_interactive_to_drain_after_exhausting_all_background_victims(
    ) {
        let a = Admission::new(300, 4);
        // p1_b must be acquired BEFORE p0_a: background admission requires
        // no interactive request in flight, so acquiring it after would
        // hang forever (see `background_waits_while_interactive_is_in_flight`).
        // Interactive admission has no such restriction the other way.
        let p1_b = a.acquire(Priority::Background, 50, true).await.unwrap();
        let p0_a = a.acquire(Priority::Interactive, 200, true).await.unwrap();

        let a2 = a.clone();
        let p0_c_task =
            tokio::spawn(async move { a2.acquire(Priority::Interactive, 150, true).await });
        settle().await;

        assert!(
            p1_b.cancel.is_cancelled(),
            "the only background permit must be cancelled, even though cancelling it alone \
             cannot free enough budget"
        );
        assert!(
            !p0_c_task.is_finished(),
            "must keep waiting -- cancelling every P1 still leaves it short, and P0 is never \
             preempted for P0"
        );

        drop(p1_b);
        settle().await;
        assert!(
            !p0_c_task.is_finished(),
            "still short: only the background permit's 50 tokens freed, the other interactive \
             permit's 200 still holds the rest"
        );

        drop(p0_a);
        let p0_c = p0_c_task
            .await
            .unwrap()
            .expect("admitted once the other interactive permit also drains -- not deadlocked");
        drop(p0_c);
    }

    #[tokio::test(start_paused = true)]
    async fn background_queue_full_is_rejected_without_disturbing_the_existing_waiter() {
        let a = Admission::new(10, 1);
        let held = a.acquire(Priority::Background, 10, true).await.unwrap();

        let a2 = a.clone();
        let waiter = tokio::spawn(async move { a2.acquire(Priority::Background, 10, true).await });
        settle().await;

        let err = a.acquire(Priority::Background, 10, true).await.unwrap_err();
        assert_eq!(err, AdmissionError::QueueFull);

        drop(held);
        waiter
            .await
            .unwrap()
            .expect("the original waiter is unaffected by the later rejected arrival");
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
    async fn shrinking_capacity_below_current_usage_drains_rather_than_evicts() {
        let a = Admission::new(100, 4);
        let held = a.acquire(Priority::Interactive, 100, true).await.unwrap();

        a.set_capacity_tokens(10); // now way over-subscribed

        assert_eq!(
            a.in_flight(),
            1,
            "an existing permit must never be evicted by a shrink"
        );
        assert!(
            !held.cancel.is_cancelled(),
            "a shrink must never cancel an interactive permit to force compliance"
        );

        drop(held);
        assert_eq!(a.in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_capacity_shrink_below_a_queued_requests_size_resolves_it_as_too_large_instead_of_hanging(
    ) {
        let a = Admission::new(100, 4);
        let held = a.acquire(Priority::Interactive, 100, true).await.unwrap();

        let a2 = a.clone();
        let waiter = tokio::spawn(async move { a2.acquire(Priority::Interactive, 80, true).await });
        settle().await;
        assert!(!waiter.is_finished());

        // Shrink capacity below the queued request's size: it can now
        // never be admitted no matter how long it waits, and P0 has no
        // timeout by spec -- without a re-check inside the wait loop this
        // would hang forever at the FIFO head instead of resolving.
        a.set_capacity_tokens(50);
        settle().await;

        let err = waiter.await.unwrap().unwrap_err();
        assert_eq!(err, AdmissionError::TooLarge);

        drop(held); // still counted against the OLD capacity -- drain, don't evict
        assert_eq!(a.in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn increasing_capacity_wakes_a_waiting_interactive_request_immediately() {
        // Capacity (90) must comfortably exceed the waiter's own request
        // (80) -- this test is about waiting on current USAGE, not tripping
        // the entry-level `tokens > capacity` check.
        let a = Admission::new(90, 4);
        let held = a.acquire(Priority::Interactive, 50, true).await.unwrap();

        let a2 = a.clone();
        let waiter = tokio::spawn(async move { a2.acquire(Priority::Interactive, 80, true).await });
        settle().await;
        assert!(!waiter.is_finished());

        // Grows enough for both, without `held` ever releasing -- the
        // waiter must notice via `notify_waiters()`, not by sleeping until
        // some unrelated release happens to wake it.
        a.set_capacity_tokens(200);
        settle().await;

        let permit = waiter
            .await
            .unwrap()
            .expect("admitted once capacity grows enough, without waiting on `held`");
        drop(held);
        drop(permit);
    }

    #[tokio::test(start_paused = true)]
    async fn shrinking_queue_depth_only_refuses_new_arrivals_not_existing_waiters() {
        let a = Admission::new(10, 2);
        let held = a.acquire(Priority::Interactive, 10, true).await.unwrap();

        let a2 = a.clone();
        let waiter = tokio::spawn(async move { a2.acquire(Priority::Interactive, 10, true).await });
        settle().await;
        assert!(!waiter.is_finished());

        a.set_queue_depth(1); // already at/over depth for a NEW arrival now

        let err = a
            .acquire(Priority::Interactive, 10, true)
            .await
            .unwrap_err();
        assert_eq!(err, AdmissionError::QueueFull);

        drop(held);
        waiter
            .await
            .unwrap()
            .expect("the existing waiter, queued before the shrink, is unaffected by it");
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
