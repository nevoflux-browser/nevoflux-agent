//! EventBus emitters for `/schedule` (routines-style cron + one-off jobs).
//!
//! All schedule events publish under the `system:schedule:*` topic prefix
//! and always include `{schedule_id, name}` in their payload so subscribers
//! can filter without relying on topic-level uniqueness.
//!
//! Every payload additionally carries a fresh `event_id` (uuid v4). Unlike
//! `loops::events::LoopEvents`, schedule consumers dedupe delivered events
//! by `event_id`, so every publish here must mint one — never omit it.
//!
//! `created` and transient run signals (`run_start`, `run_end`, `missed`)
//! use Ephemeral delivery. `state_changed` and `snapshot` use Sticky so a
//! sidebar reopening on the same schedule (or wanting the aggregate icon
//! state) sees the current state immediately without waiting for the next
//! transition.

use crate::event_bus::types::{BusEvent, PublisherIdentity};
use crate::event_bus::EventBus;
use nevoflux_storage::models::schedule::ScheduleRecord;
use serde_json::json;
use std::sync::Arc;

/// Cap applied to `run_end`'s `error` string before publishing, to avoid
/// bloating event payloads when a run fails with a long error/backtrace.
const RUN_END_ERROR_MAX_CHARS: usize = 1024;

/// Cap applied to `run_end`'s `final_text` before publishing (4 KB, mirroring
/// `/loop`'s `LoopIterationEndPayload`). Keeps reminder/output text small
/// enough for the event bus while still readable in the Jobs panel.
const RUN_END_FINAL_TEXT_MAX_BYTES: usize = 4096;

/// Truncate `s` to at most `max_bytes`, snapping down to the nearest UTF-8
/// char boundary so we never split a multi-byte character.
fn cap_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Helper around an EventBus handle. If `bus` is `None`, every method is a no-op.
/// ScheduleManager holds an `Option<Arc<EventBus>>` so unit tests that don't
/// wire a bus still work.
pub struct ScheduleEvents {
    bus: Option<Arc<EventBus>>,
}

impl ScheduleEvents {
    pub fn new(bus: Option<Arc<EventBus>>) -> Self {
        Self { bus }
    }

    pub fn from_arc(bus: Arc<EventBus>) -> Self {
        Self { bus: Some(bus) }
    }

    /// `system:schedule:created` (ephemeral).
    pub async fn created(&self, rec: &ScheduleRecord) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:schedule:created",
                json!({
                    "event_id": uuid::Uuid::new_v4().to_string(),
                    "schedule_id": rec.id,
                    "name": rec.name,
                    "cron_expr": rec.cron_expr,
                    "at_ts": rec.at_ts,
                    "browser_policy": rec.browser_policy,
                    "mode": rec.mode,
                    "next_fire_at": rec.next_fire_at,
                    "status": rec.status.as_str(),
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// `system:schedule:state_changed` (sticky).
    pub async fn state_changed(
        &self,
        id: &str,
        name: &str,
        new_status: &str,
        prev_status: &str,
        next_fire_at: Option<i64>,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::sticky(
                "system:schedule:state_changed",
                json!({
                    "event_id": uuid::Uuid::new_v4().to_string(),
                    "schedule_id": id,
                    "name": name,
                    "new_status": new_status,
                    "prev_status": prev_status,
                    "next_fire_at": next_fire_at,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// `system:schedule:run_start` (ephemeral).
    pub async fn run_start(
        &self,
        id: &str,
        name: &str,
        run_id: i64,
        fire_kind: &str,
        started_at: i64,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:schedule:run_start",
                json!({
                    "event_id": uuid::Uuid::new_v4().to_string(),
                    "schedule_id": id,
                    "name": name,
                    "run_id": run_id,
                    "fire_kind": fire_kind,
                    "started_at": started_at,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// `system:schedule:run_end` (ephemeral). Caps `error` at
    /// [`RUN_END_ERROR_MAX_CHARS`] characters.
    pub async fn run_end(
        &self,
        id: &str,
        name: &str,
        run_id: i64,
        status: &str,
        ended_at: i64,
        error: Option<&str>,
        final_text: Option<&str>,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let error_capped: Option<String> = error.map(|s| {
            if s.len() > RUN_END_ERROR_MAX_CHARS {
                s.chars().take(RUN_END_ERROR_MAX_CHARS).collect()
            } else {
                s.to_string()
            }
        });
        let final_text_capped: Option<String> =
            final_text.map(|s| cap_bytes(s, RUN_END_FINAL_TEXT_MAX_BYTES));
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:schedule:run_end",
                json!({
                    "event_id": uuid::Uuid::new_v4().to_string(),
                    "schedule_id": id,
                    "name": name,
                    "run_id": run_id,
                    "status": status,
                    "ended_at": ended_at,
                    "error": error_capped,
                    "final_text": final_text_capped,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// `system:schedule:missed` (ephemeral).
    pub async fn missed(&self, id: &str, name: &str, fire_was_at: i64) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:schedule:missed",
                json!({
                    "event_id": uuid::Uuid::new_v4().to_string(),
                    "schedule_id": id,
                    "name": name,
                    "fire_was_at": fire_was_at,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// `system:schedule:snapshot` (sticky). `summary` is the pre-built
    /// aggregate `{active, running, failed_recent, next_fire_at}` — callers
    /// assemble it (e.g. from a manager-wide scan) and this helper just
    /// stamps an `event_id` and publishes it verbatim.
    pub async fn snapshot(&self, summary: serde_json::Value) {
        let Some(bus) = &self.bus else {
            return;
        };
        let mut payload = summary;
        if let serde_json::Value::Object(ref mut map) = payload {
            map.insert(
                "event_id".to_string(),
                json!(uuid::Uuid::new_v4().to_string()),
            );
        } else {
            payload = json!({
                "event_id": uuid::Uuid::new_v4().to_string(),
                "summary": payload,
            });
        }
        let _ = bus
            .publish(BusEvent::sticky(
                "system:schedule:snapshot",
                payload,
                PublisherIdentity::Internal,
            ))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::types::{BackpressurePolicy, SubscriberIdentity, TopicPattern};
    use nevoflux_storage::models::schedule::ScheduleStatus;

    fn sample_record() -> ScheduleRecord {
        ScheduleRecord {
            id: "sch-1".into(),
            creator_session_id: None,
            name: "nightly-digest".into(),
            cron_expr: Some("0 6 * * *".into()),
            at_ts: None,
            prompt_text: None,
            wrapped_skill: None,
            mode: "goal".into(),
            browser_policy: "headless".into(),
            on_unavailable: None,
            headless_profile: None,
            catch_up: false,
            goal_condition: None,
            goal_max_turns: None,
            max_tokens_per_run: None,
            evaluator_model: None,
            evaluator_provider: None,
            status: ScheduleStatus::Active,
            next_fire_at: Some(1_700_000_000),
            last_run_status: None,
            last_run_at: None,
            consecutive_failures: 0,
            run_count: 0,
            created_at: 1_699_000_000,
            updated_at: 1_699_000_000,
        }
    }

    #[tokio::test]
    async fn no_op_when_bus_is_none() {
        // Just exercises the early-return branches; should not panic.
        let evts = ScheduleEvents::new(None);
        let rec = sample_record();
        evts.created(&rec).await;
        evts.state_changed("sch-1", "nightly-digest", "active", "paused", None)
            .await;
        evts.run_start("sch-1", "nightly-digest", 1, "cron", 100)
            .await;
        evts.run_end("sch-1", "nightly-digest", 1, "ok", 110, None, None)
            .await;
        evts.missed("sch-1", "nightly-digest", 90).await;
        evts.snapshot(json!({"active": 1, "running": 0, "failed_recent": 0, "next_fire_at": null}))
            .await;
    }

    /// Subscribes to `system:schedule:**` and drives every helper once,
    /// asserting topic names, `event_id` presence, and sticky/ephemeral
    /// delivery kind on the received `BusEvent`s.
    #[tokio::test]
    async fn publishes_expected_topics_and_delivery_kinds() {
        let bus = Arc::new(EventBus::new());
        let mut handle = bus
            .subscribe(
                TopicPattern::double_wildcard("system:schedule"),
                SubscriberIdentity::Internal,
                BackpressurePolicy::DropNewest,
                32,
            )
            .expect("subscribe should succeed");

        let evts = ScheduleEvents::from_arc(bus.clone());
        let rec = sample_record();

        evts.created(&rec).await;
        evts.state_changed("sch-1", "nightly-digest", "active", "paused", Some(42))
            .await;
        evts.run_start("sch-1", "nightly-digest", 7, "cron", 100)
            .await;
        evts.run_end(
            "sch-1",
            "nightly-digest",
            7,
            "error",
            110,
            Some(&"x".repeat(2000)),
            Some(&"y".repeat(5000)),
        )
        .await;
        evts.missed("sch-1", "nightly-digest", 90).await;
        evts.snapshot(json!({"active": 1, "running": 0, "failed_recent": 0, "next_fire_at": 42}))
            .await;

        let mut received = Vec::new();
        for _ in 0..6 {
            received.push(handle.rx.try_recv().expect("expected a buffered event"));
        }
        assert!(handle.rx.try_recv().is_err(), "no extra events expected");

        use crate::event_bus::types::Delivery;

        let created = &received[0];
        assert_eq!(created.topic, "system:schedule:created");
        assert_eq!(created.delivery, Delivery::Ephemeral);
        assert_eq!(created.payload["schedule_id"], json!("sch-1"));
        assert_eq!(created.payload["name"], json!("nightly-digest"));
        assert!(created.payload["event_id"].is_string());

        let state_changed = &received[1];
        assert_eq!(state_changed.topic, "system:schedule:state_changed");
        assert_eq!(state_changed.delivery, Delivery::Sticky);
        assert!(state_changed.payload["event_id"].is_string());

        let run_start = &received[2];
        assert_eq!(run_start.topic, "system:schedule:run_start");
        assert_eq!(run_start.delivery, Delivery::Ephemeral);
        assert!(run_start.payload["event_id"].is_string());

        let run_end = &received[3];
        assert_eq!(run_end.topic, "system:schedule:run_end");
        assert_eq!(run_end.delivery, Delivery::Ephemeral);
        assert!(run_end.payload["event_id"].is_string());
        let error_str = run_end.payload["error"].as_str().unwrap();
        assert_eq!(error_str.len(), 1024);
        // final_text capped at 4 KB.
        let final_text_str = run_end.payload["final_text"].as_str().unwrap();
        assert_eq!(final_text_str.len(), 4096);

        let missed = &received[4];
        assert_eq!(missed.topic, "system:schedule:missed");
        assert_eq!(missed.delivery, Delivery::Ephemeral);
        assert!(missed.payload["event_id"].is_string());

        let snapshot = &received[5];
        assert_eq!(snapshot.topic, "system:schedule:snapshot");
        assert_eq!(snapshot.delivery, Delivery::Sticky);
        assert!(snapshot.payload["event_id"].is_string());
        assert_eq!(snapshot.payload["active"], json!(1));

        // All event_ids must be distinct.
        let ids: std::collections::HashSet<_> = received
            .iter()
            .map(|e| e.payload["event_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), received.len());
    }
}
