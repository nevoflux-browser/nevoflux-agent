//! EventBus emitters for /loop (spec §11).
//!
//! All loop events publish under the `system:loop:*` topic prefix and
//! always include `{session_id, loop_id}` in their payload so subscribers
//! can filter without relying on topic-level uniqueness.
//!
//! Iteration-start/end and scratchpad events use Ephemeral delivery
//! (transient, in-flight signaling). State changes use Sticky so a sidebar
//! reopening on the same session sees the current state immediately.

use crate::event_bus::types::{BusEvent, PublisherIdentity};
use crate::event_bus::EventBus;
use crate::loops::types::LoopId;
use serde_json::json;
use std::sync::Arc;

/// Helper around an EventBus handle. If `bus` is `None`, every method is a no-op.
/// LoopManager holds an `Option<Arc<EventBus>>` so unit tests that don't wire
/// a bus still work.
pub struct LoopEvents {
    bus: Option<Arc<EventBus>>,
}

impl LoopEvents {
    pub fn new(bus: Option<Arc<EventBus>>) -> Self {
        Self { bus }
    }

    pub fn from_arc(bus: Arc<EventBus>) -> Self {
        Self { bus: Some(bus) }
    }

    pub async fn created(
        &self,
        session_id: &str,
        id: &LoopId,
        trigger_expr: &str,
        prompt_text: Option<&str>,
        wrapped_skill: Option<&str>,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        // STICKY (not ephemeral): the EventBus only replays sticky events to new
        // subscribers. A loop still in its initial `Pending` state has emitted
        // ONLY this `created` event (no `state_changed` yet), so if it were
        // ephemeral a fresh subscriber — e.g. the maximized Loop Jobs panel,
        // which loads as a brand-new page with an empty `ctx.loops` — would never
        // learn the loop exists and render an empty panel. Sticky makes `created`
        // replay on subscribe, mirroring `state_changed` below and the schedule
        // events. Terminal loops stay consistent: the cancel/fail paths emit a
        // sticky `state_changed` → terminal that supersedes this on replay.
        let _ = bus
            .publish(BusEvent::sticky(
                "system:loop:created",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "trigger_expr": trigger_expr,
                    "prompt_text": prompt_text,
                    "wrapped_skill": wrapped_skill,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    pub async fn state_changed(
        &self,
        session_id: &str,
        id: &LoopId,
        new_state: &str,
        prev_state: &str,
        reason: Option<&str>,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::sticky(
                "system:loop:state_changed",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "new_state": new_state,
                    "prev_state": prev_state,
                    "reason": reason,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    pub async fn iteration_start(
        &self,
        session_id: &str,
        id: &LoopId,
        sequence: i64,
        started_at: i64,
        fire_reason: &str,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:loop:iteration_start",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "sequence_number": sequence,
                    "started_at": started_at,
                    "fire_reason": fire_reason,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    pub async fn iteration_end(
        &self,
        session_id: &str,
        id: &LoopId,
        sequence: i64,
        ended_at: i64,
        status: &str,
        tool_calls_summary: serde_json::Value,
        final_text: Option<&str>,
        verify_passed: Option<bool>,
        verify_reason: Option<&str>,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        // Cap final_text at 4KB to avoid bloating event payloads when an
        // iteration produces a long response. Sidebar rendering is for
        // quick inspection, not full transcripts — anything longer can
        // be moved to scratchpad explicitly.
        let final_text_capped: Option<String> = final_text.map(|s| {
            if s.len() > 4096 {
                s.chars().take(4096).collect()
            } else {
                s.to_string()
            }
        });
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:loop:iteration_end",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "sequence_number": sequence,
                    "ended_at": ended_at,
                    "status": status,
                    "tool_calls_summary": tool_calls_summary,
                    "final_text": final_text_capped,
                    "verify_passed": verify_passed,
                    "verify_reason": verify_reason,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    pub async fn scratchpad_changed(&self, session_id: &str, id: &LoopId, content: &str) {
        let Some(bus) = &self.bus else {
            return;
        };
        let preview: String = content.chars().take(120).collect();
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:loop:scratchpad_changed",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "preview": preview,
                    "bytes": content.len(),
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    pub async fn trigger_dropped(&self, session_id: &str, id: &LoopId, skipped_count: i64) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:loop:trigger_dropped",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "skipped_count": skipped_count,
                    "reason": "concurrent_iteration",
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// Emitted when a deterministic gate (W3 §gate) suppresses an iteration
    /// for a trigger fire — distinct from `trigger_dropped` (concurrent-run
    /// coalescing). Both bump `skipped_triggers`; this one carries the fire
    /// reason instead of a coalesced-count so sidebar consumers can tell
    /// "gate said no" apart from "loop was busy".
    pub async fn skipped(&self, session_id: &str, id: &LoopId, fire_reason: &str) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:loop:skipped",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "fire_reason": fire_reason,
                    "reason": "gate",
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// Emitted when the `/loop evolve` meta-pass (W4) produces a
    /// self-improvement proposal for a loop. Carries the same fields a
    /// sidebar accept/reject UI needs to render the diff without a
    /// follow-up fetch.
    pub async fn proposal(
        &self,
        session_id: &str,
        id: &LoopId,
        p: &nevoflux_storage::repositories::LoopProposal,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:loop:proposal",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "id": p.id,
                    "rationale": p.rationale,
                    "proposed_prompt_text": p.proposed_prompt_text,
                    "proposed_gate_spec": p.proposed_gate_spec,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    /// Emitted when a human accepts or rejects a pending `/loop evolve`
    /// proposal (W4 task 3), via `loop_proposal_respond`. Distinct from
    /// `proposal` (which announces a new pending proposal) — this announces
    /// its resolution, so a sidebar can clear the pending-review affordance
    /// and, on accept, reflect the loop's new prompt_text/gate_spec.
    pub async fn proposal_resolved(
        &self,
        session_id: &str,
        id: &LoopId,
        proposal_id: &str,
        accepted: bool,
    ) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:loop:proposal_resolved",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "proposal_id": proposal_id,
                    "accepted": accepted,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }

    pub async fn cancelled(&self, session_id: &str, id: &LoopId, by: &str, force: bool) {
        let Some(bus) = &self.bus else {
            return;
        };
        let _ = bus
            .publish(BusEvent::ephemeral(
                "system:loop:cancelled",
                json!({
                    "session_id": session_id,
                    "loop_id": id.as_ref(),
                    "cancelled_by": by,
                    "force": force,
                }),
                PublisherIdentity::Internal,
            ))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_op_when_bus_is_none() {
        // Just exercises the early-return branches; should not panic.
        let evts = LoopEvents::new(None);
        let id = LoopId("abc".into());
        evts.created("s1", &id, "time:5m", None, None).await;
        evts.state_changed("s1", &id, "running", "pending", None)
            .await;
        evts.iteration_start("s1", &id, 1, 100, "time").await;
        evts.iteration_end(
            "s1",
            &id,
            1,
            110,
            "ok",
            serde_json::json!([]),
            None,
            None,
            None,
        )
        .await;
        evts.scratchpad_changed("s1", &id, "k=v").await;
        evts.trigger_dropped("s1", &id, 1).await;
        evts.skipped("s1", &id, "time").await;
        evts.cancelled("s1", &id, "user", false).await;
        let p = nevoflux_storage::repositories::LoopProposal {
            id: "prop-1".into(),
            loop_id: "abc".into(),
            created_at: 1,
            rationale: "healthy".into(),
            proposed_prompt_text: None,
            proposed_gate_spec: None,
            status: "pending".into(),
        };
        evts.proposal("s1", &id, &p).await;
        evts.proposal_resolved("s1", &id, "prop-1", true).await;
    }

    /// Regression: `system:loop:created` MUST be sticky so a subscriber that
    /// joins AFTER the loop was created still learns it exists. This is the
    /// maximized Loop Jobs panel: maximizing opens a brand-new page that
    /// subscribes fresh (`replay_sticky=true`) with an empty `ctx.loops`. If
    /// `created` were ephemeral (the bug), a loop still in `Pending` — which has
    /// emitted only `created`, no `state_changed` yet — would never be replayed
    /// and the panel would be empty.
    #[tokio::test]
    async fn created_is_sticky_so_late_subscribers_see_the_loop() {
        use crate::event_bus::types::{
            BackpressurePolicy, Delivery, SubscriberIdentity, TopicPattern,
        };
        use std::sync::Arc;

        let bus = Arc::new(EventBus::new());
        let evts = LoopEvents::from_arc(bus.clone());
        let id = LoopId("loop-xyz".into());

        // Loop is created BEFORE anyone subscribes (docked sidebar was the only
        // live listener; it then goes away when the user maximizes).
        evts.created("sess-1", &id, "time:2m", Some("drink water"), None)
            .await;

        // A NEW subscriber joins afterwards (the maximized panel) and asks for
        // sticky replay — exactly what the sidebar does.
        let mut handle = bus
            .subscribe_with_options(
                TopicPattern::double_wildcard("system:loop"),
                SubscriberIdentity::Internal,
                BackpressurePolicy::DropNewest,
                32,
                true, // replay_sticky
            )
            .expect("subscribe should succeed");

        let ev = handle
            .rx
            .try_recv()
            .expect("late subscriber must receive the replayed created event");
        assert_eq!(ev.topic, "system:loop:created");
        assert_eq!(ev.delivery, Delivery::Sticky, "created must be sticky");
        assert_eq!(ev.payload["loop_id"], serde_json::json!("loop-xyz"));
        assert_eq!(ev.payload["session_id"], serde_json::json!("sess-1"));
    }
}
