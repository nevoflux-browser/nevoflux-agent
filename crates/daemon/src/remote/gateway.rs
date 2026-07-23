//! Remote gateway abstraction (design §9 / Q16).
//!
//! A `RemoteGateway` is one remote-access endpoint — portal is impl #1; Slack /
//! telegram are future impls. The daemon has two shared outbound event sources
//! (design §9.6): the chat stream (M2 tap of the `DaemonEnvelope` exit) and the
//! notification/activity stream (EventBus `ui:notification:*` / `system:*`).
//! Both are normalized into an [`OutboundEvent`] and fanned out to **every**
//! registered gateway via [`GatewayRegistry::fan_out`]; each gateway renders
//! each variant into its own medium (portal → toast, Slack → DM, …). notify is
//! therefore a per-gateway projection, not a shared pipe.
//!
//! Scope (design Q16 ①A): this defines the thin trait + registry + fan-out.
//! Only the outbound projection direction lives here; wiring the live M2 tap and
//! the concrete portal impl (WS + [`super::crypto`]) land in later phases.

use std::sync::Arc;

use async_trait::async_trait;
use nevoflux_protocol::DaemonEnvelope;

/// What a gateway can faithfully render (design §9.2 capability axis).
/// Orthogonal to notify projection — even a `TextOnly` gateway renders
/// notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Full conversation parity (artifact cards, tool chips, plan, browser
    /// tool round-trips) — e.g. the portal gateway.
    FullParity,
    /// Text-only degraded head (no local executor); rich frames are projected
    /// lossily — e.g. Slack / telegram.
    TextOnly,
}

/// A user-facing notification (design §9.1 `Notification`), sourced from the
/// EventBus `ui:notification:*` topic (see `crate::notify`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationEvent {
    /// Optional title; consumers default to "NevoFlux" when `None`.
    pub title: Option<String>,
    /// The notification body text.
    pub body: String,
    /// Origin tag (e.g. `"notify_user"`).
    pub source: String,
}

/// An activity/progress event (design §9.1 `Activity`), sourced from the
/// EventBus `system:goal|loop|schedule|pack:*` topics.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityEvent {
    /// The EventBus topic the activity came from.
    pub topic: String,
    /// The event payload, projected as-is for the gateway to render.
    pub payload: serde_json::Value,
}

/// A normalized outbound event fanned out to every gateway (design §9.1).
#[derive(Debug, Clone, PartialEq)]
pub enum OutboundEvent {
    /// Chat stream frame — from the M2 tap of the chat `DaemonEnvelope` exit.
    Chat(DaemonEnvelope),
    /// User notification — from EventBus `ui:notification:*`.
    Notification(NotificationEvent),
    /// Activity/progress — from EventBus `system:*`.
    Activity(ActivityEvent),
}

/// One remote-access endpoint. Impls render each [`OutboundEvent`] variant into
/// their own medium. The uplink direction (medium input → `ProxyEnvelope`
/// injected with the local `proxy_id`) is handled by the impl calling the
/// daemon's unified injection point directly, not through this trait.
#[async_trait]
pub trait RemoteGateway: Send + Sync {
    /// Stable id: `"portal" | "slack" | "telegram"`.
    fn id(&self) -> &str;
    /// What this gateway can faithfully render.
    fn capability(&self) -> Capability;
    /// Render `ev` into this gateway's medium (encrypt + send for portal;
    /// format + post for social).
    async fn project(&self, ev: &OutboundEvent);
}

/// Holds the registered gateways and fans outbound events to all of them
/// (design Q16 ②A: concurrent multi-gateway, so this is a `Vec`).
#[derive(Default, Clone)]
pub struct GatewayRegistry {
    gateways: Vec<Arc<dyn RemoteGateway>>,
}

impl GatewayRegistry {
    /// A new, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a gateway.
    pub fn register(&mut self, gateway: Arc<dyn RemoteGateway>) {
        self.gateways.push(gateway);
    }

    /// Number of registered gateways.
    pub fn len(&self) -> usize {
        self.gateways.len()
    }

    /// Whether no gateways are registered.
    pub fn is_empty(&self) -> bool {
        self.gateways.is_empty()
    }

    /// Ids of registered gateways (for D2 notifications / kick-by-gateway).
    pub fn ids(&self) -> Vec<String> {
        self.gateways.iter().map(|g| g.id().to_string()).collect()
    }

    /// Fan an outbound event to every registered gateway. Each gateway projects
    /// it into its own medium; a slow/failing gateway does not block the others'
    /// completion beyond this awaited pass (kept sequential for determinism —
    /// the live path may parallelize once transports differ in latency).
    pub async fn fan_out(&self, ev: &OutboundEvent) {
        for gw in &self.gateways {
            gw.project(ev).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every event it is asked to project.
    struct MockGateway {
        id: String,
        cap: Capability,
        seen: Arc<Mutex<Vec<OutboundEvent>>>,
    }

    #[async_trait]
    impl RemoteGateway for MockGateway {
        fn id(&self) -> &str {
            &self.id
        }
        fn capability(&self) -> Capability {
            self.cap
        }
        async fn project(&self, ev: &OutboundEvent) {
            self.seen.lock().unwrap().push(ev.clone());
        }
    }

    fn mock(id: &str, cap: Capability) -> (Arc<MockGateway>, Arc<Mutex<Vec<OutboundEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gw = Arc::new(MockGateway {
            id: id.into(),
            cap,
            seen: seen.clone(),
        });
        (gw, seen)
    }

    fn notif() -> OutboundEvent {
        OutboundEvent::Notification(NotificationEvent {
            title: Some("Reminder".into()),
            body: "drink water".into(),
            source: "notify_user".into(),
        })
    }

    #[tokio::test]
    async fn fan_out_reaches_every_gateway() {
        let (portal, portal_seen) = mock("portal", Capability::FullParity);
        let (slack, slack_seen) = mock("slack", Capability::TextOnly);
        let mut reg = GatewayRegistry::new();
        reg.register(portal);
        reg.register(slack);
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.ids(), vec!["portal", "slack"]);

        reg.fan_out(&notif()).await;

        // notify is a per-gateway projection: BOTH received it, regardless of
        // capability (design §9.1 — orthogonal to capability()).
        assert_eq!(portal_seen.lock().unwrap().as_slice(), &[notif()]);
        assert_eq!(slack_seen.lock().unwrap().as_slice(), &[notif()]);
    }

    #[tokio::test]
    async fn empty_registry_fan_out_is_noop() {
        let reg = GatewayRegistry::new();
        assert!(reg.is_empty());
        reg.fan_out(&notif()).await; // must not panic
    }

    #[tokio::test]
    async fn fan_out_preserves_variant() {
        let (gw, seen) = mock("portal", Capability::FullParity);
        let mut reg = GatewayRegistry::new();
        reg.register(gw);

        let activity = OutboundEvent::Activity(ActivityEvent {
            topic: "system:loop:progress".into(),
            payload: serde_json::json!({ "loop_id": "L1", "state": "running" }),
        });
        reg.fan_out(&activity).await;

        assert_eq!(seen.lock().unwrap().as_slice(), &[activity]);
    }
}
