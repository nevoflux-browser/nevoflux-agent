//! EventBus emitters for the goal engine (`system:goal:*`).
//!
//! Mirrors [`crate::schedules::events`] (Task 1.4): every payload carries a
//! fresh `event_id` (uuid v4) plus `session_id` and `goal_id` so subscribers
//! can filter and dedupe. Two topics:
//!
//! - `system:goal:state_changed` — **Sticky**. Emitted on every lifecycle
//!   transition (set / achieved / expired / cleared) *and* on every post-turn
//!   re-evaluation that leaves the goal active (so a sidebar reopening on the
//!   session sees current condition/status/turns/last_reason without waiting
//!   for the next transition).
//! - `system:goal:evaluated` — **Ephemeral**. One transient signal per
//!   evaluator verdict (`met`, `reason`, `turn`).
//!
//! `GoalManager` holds an `Option<Arc<EventBus>>` so unit tests that don't wire
//! a bus still work — every method here is a no-op when `bus` is `None`.

use crate::event_bus::types::{BusEvent, PublisherIdentity};
use crate::event_bus::EventBus;
use serde_json::json;
use std::sync::Arc;

/// Cap applied to any `reason` string before publishing, so a pathological
/// evaluator (or a long `evaluator error: ...` transport message) can't bloat
/// an event payload. The evaluator is instructed to answer with one short
/// sentence, so this is only a defensive bound.
const REASON_MAX_CHARS: usize = 1024;

fn cap_reason(reason: &str) -> String {
    if reason.chars().count() > REASON_MAX_CHARS {
        reason.chars().take(REASON_MAX_CHARS).collect()
    } else {
        reason.to_string()
    }
}

/// Helper around an EventBus handle. If `bus` is `None`, every method is a
/// no-op (used by unit tests).
pub struct GoalEvents {
    bus: Option<Arc<EventBus>>,
}

impl GoalEvents {
    pub fn new(bus: Option<Arc<EventBus>>) -> Self {
        Self { bus }
    }

    pub fn from_arc(bus: Arc<EventBus>) -> Self {
        Self { bus: Some(bus) }
    }

    /// `system:goal:state_changed` (sticky). Carries the full current snapshot
    /// so a late subscriber renders the goal without replaying history.
    #[allow(clippy::too_many_arguments)]
    pub async fn state_changed(
        &self,
        session_id: &str,
        goal_id: &str,
        status: &str,
        condition: &str,
        turns_used: i64,
        max_turns: i64,
        last_reason: Option<&str>,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::sticky(
                "system:goal:state_changed",
                json!({
                    "event_id": uuid::Uuid::new_v4().to_string(),
                    "session_id": session_id,
                    "goal_id": goal_id,
                    "status": status,
                    "condition": condition,
                    "turns_used": turns_used,
                    "max_turns": max_turns,
                    "last_reason": last_reason.map(cap_reason),
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// `system:goal:evaluated` (ephemeral). One transient signal per verdict.
    pub async fn evaluated(
        &self,
        session_id: &str,
        goal_id: &str,
        met: bool,
        reason: &str,
        turn: i64,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:goal:evaluated",
                json!({
                    "event_id": uuid::Uuid::new_v4().to_string(),
                    "session_id": session_id,
                    "goal_id": goal_id,
                    "met": met,
                    "reason": cap_reason(reason),
                    "turn": turn,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::types::{BackpressurePolicy, Delivery, SubscriberIdentity, TopicPattern};
    use std::collections::HashSet;

    #[tokio::test]
    async fn no_op_when_bus_is_none() {
        // Exercises the early-return branches; should not panic.
        let evts = GoalEvents::new(None);
        evts.state_changed("sess-1", "goal-1", "active", "cond", 0, 20, None)
            .await;
        evts.evaluated("sess-1", "goal-1", false, "not yet", 1)
            .await;
    }

    #[tokio::test]
    async fn publishes_expected_topics_and_delivery_kinds() {
        let bus = Arc::new(EventBus::new());
        let mut handle = bus
            .subscribe(
                TopicPattern::double_wildcard("system:goal"),
                SubscriberIdentity::Internal,
                BackpressurePolicy::DropNewest,
                32,
            )
            .expect("subscribe should succeed");

        let evts = GoalEvents::from_arc(bus.clone());
        evts.state_changed(
            "sess-1",
            "goal-1",
            "active",
            "the PR is merged",
            2,
            20,
            Some("still open"),
        )
        .await;
        evts.evaluated("sess-1", "goal-1", true, "PR #42 merged", 3)
            .await;

        let mut received = Vec::new();
        for _ in 0..2 {
            received.push(handle.rx.try_recv().expect("expected a buffered event"));
        }
        assert!(handle.rx.try_recv().is_err(), "no extra events expected");

        let sc = &received[0];
        assert_eq!(sc.topic, "system:goal:state_changed");
        assert_eq!(sc.delivery, Delivery::Sticky);
        assert_eq!(sc.payload["session_id"], json!("sess-1"));
        assert_eq!(sc.payload["goal_id"], json!("goal-1"));
        assert_eq!(sc.payload["status"], json!("active"));
        assert_eq!(sc.payload["turns_used"], json!(2));
        assert_eq!(sc.payload["last_reason"], json!("still open"));
        assert!(sc.payload["event_id"].is_string());

        let ev = &received[1];
        assert_eq!(ev.topic, "system:goal:evaluated");
        assert_eq!(ev.delivery, Delivery::Ephemeral);
        assert_eq!(ev.payload["met"], json!(true));
        assert_eq!(ev.payload["turn"], json!(3));
        assert!(ev.payload["event_id"].is_string());

        // event_ids are distinct.
        let ids: HashSet<_> = received
            .iter()
            .map(|e| e.payload["event_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), received.len());
    }

    #[tokio::test]
    async fn reason_is_capped() {
        let bus = Arc::new(EventBus::new());
        let mut handle = bus
            .subscribe(
                TopicPattern::double_wildcard("system:goal"),
                SubscriberIdentity::Internal,
                BackpressurePolicy::DropNewest,
                8,
            )
            .expect("subscribe should succeed");
        let evts = GoalEvents::from_arc(bus.clone());
        let long = "x".repeat(4000);
        evts.evaluated("s", "g", false, &long, 1).await;

        let ev = handle.rx.try_recv().unwrap();
        assert_eq!(ev.payload["reason"].as_str().unwrap().chars().count(), 1024);
    }
}
