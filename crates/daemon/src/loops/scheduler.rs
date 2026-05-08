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
