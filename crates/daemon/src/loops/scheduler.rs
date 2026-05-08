//! Trigger scheduler — owns time-wheel and (Phase 11+) EventBus subscriptions
//! that emit `LoopFireRequest` into the dispatcher channel.

use crate::loops::types::LoopId;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Message published when a trigger fires for a loop.
#[derive(Debug, Clone)]
pub struct LoopFireRequest {
    pub loop_id: LoopId,
    pub fire_reason: String,
}

#[derive(Default, Clone)]
pub struct TriggerScheduler {
    handles: Arc<RwLock<HashMap<String, JoinHandle<()>>>>,
    cancels: Arc<RwLock<HashMap<String, CancellationToken>>>,
}

impl TriggerScheduler {
    pub fn new() -> Self { Self::default() }

    /// Schedule a recurring time-based fire. Returns a subscription id.
    /// Pass the id to `unsubscribe` to stop the schedule.
    pub fn schedule_time(
        &self,
        loop_id: LoopId,
        every: Duration,
        sink: mpsc::Sender<LoopFireRequest>,
    ) -> String {
        let sub_id = format!("time-{}-{}", loop_id.as_ref(), uuid::Uuid::new_v4());
        let cancel = CancellationToken::new();
        self.cancels
            .write()
            .expect("scheduler poisoned")
            .insert(sub_id.clone(), cancel.clone());
        let id = loop_id.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(every) => {
                        if sink.send(LoopFireRequest {
                            loop_id: id.clone(),
                            fire_reason: "time".into(),
                        }).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        self.handles
            .write()
            .expect("scheduler poisoned")
            .insert(sub_id.clone(), handle);
        sub_id
    }

    /// Unsubscribe a previously-issued subscription. Idempotent — no-op if id is unknown.
    pub fn unsubscribe(&self, sub_id: &str) {
        if let Some(c) = self.cancels.write().expect("scheduler poisoned").remove(sub_id) {
            c.cancel();
        }
        if let Some(h) = self.handles.write().expect("scheduler poisoned").remove(sub_id) {
            h.abort();
        }
    }

    /// Number of active subscriptions; useful for tests and shutdown bookkeeping.
    pub fn active_count(&self) -> usize {
        self.cancels.read().expect("scheduler poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn fires_every_interval() {
        let sched = TriggerScheduler::new();
        let (tx, mut rx) = mpsc::channel(8);
        let _sub = sched.schedule_time(
            LoopId("abc".into()),
            Duration::from_secs(60),
            tx,
        );

        // No fire before the interval has elapsed.
        assert!(rx.try_recv().is_err());

        // First fire just after t=60s.
        tokio::time::advance(Duration::from_secs(61)).await;
        let r = rx.recv().await.expect("first fire");
        assert_eq!(r.loop_id.as_ref(), "abc");
        assert_eq!(r.fire_reason, "time");

        // Second fire 60s later.
        tokio::time::advance(Duration::from_secs(60)).await;
        let r = rx.recv().await.expect("second fire");
        assert_eq!(r.loop_id.as_ref(), "abc");
    }

    #[tokio::test(start_paused = true)]
    async fn unsubscribe_stops_fires() {
        let sched = TriggerScheduler::new();
        let (tx, mut rx) = mpsc::channel(8);
        let sub = sched.schedule_time(
            LoopId("a".into()),
            Duration::from_secs(60),
            tx,
        );
        assert_eq!(sched.active_count(), 1);

        sched.unsubscribe(&sub);
        assert_eq!(sched.active_count(), 0);

        // Give the spawned task a chance to notice the cancel.
        tokio::time::advance(Duration::from_secs(120)).await;
        for _ in 0..10 { tokio::task::yield_now().await; }

        assert!(rx.try_recv().is_err(), "no fires after unsubscribe");
    }

    #[tokio::test(start_paused = true)]
    async fn unsubscribe_unknown_id_is_noop() {
        let sched = TriggerScheduler::new();
        sched.unsubscribe("does-not-exist"); // must not panic
        assert_eq!(sched.active_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn two_loops_get_independent_subscriptions() {
        let sched = TriggerScheduler::new();
        let (tx, mut rx) = mpsc::channel(16);
        let sub_a = sched.schedule_time(LoopId("a".into()), Duration::from_secs(60), tx.clone());
        let sub_b = sched.schedule_time(LoopId("b".into()), Duration::from_secs(120), tx.clone());
        assert_ne!(sub_a, sub_b);
        assert_eq!(sched.active_count(), 2);

        // After 65s only "a" should fire.
        tokio::time::advance(Duration::from_secs(65)).await;
        let r = rx.recv().await.unwrap();
        assert_eq!(r.loop_id.as_ref(), "a");
        assert!(rx.try_recv().is_err(), "b should not have fired yet");

        // 60s more → "a" fires again, and "b" fires for first time at 125s.
        tokio::time::advance(Duration::from_secs(60)).await;
        let mut got = std::collections::HashSet::new();
        for _ in 0..2 {
            got.insert(rx.recv().await.unwrap().loop_id.0);
        }
        assert!(got.contains("a"));
        assert!(got.contains("b"));
    }
}
