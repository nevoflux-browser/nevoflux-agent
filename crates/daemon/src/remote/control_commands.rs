//! Carrying out what a paired device asked for (design §14).
//!
//! The control gateway decides *what* was asked and resolves the opaque id it
//! was asked with; this is where that becomes something the daemon does. Split
//! because the gateway knows about a list and a socket, and these need the
//! injection path and the pairing store — neither of which a socket loop should
//! have ever heard of.
//!
//! **Answers go through `translate::uplink`, the same function the data channel
//! uses.** A gate answered from the list and a gate answered inside the
//! conversation have to become the identical message: `BrowserToolResponse` on
//! the path that resolves the pending oneshot. There was already one wrong
//! answer to this — `PermissionResponse` looks right and has no handler, so
//! answering used to land in `UNKNOWN_MESSAGE_TYPE` and the turn stayed blocked
//! for its full 24-hour timeout. Reusing the translation is how that stays
//! answered exactly once, in one place.

use std::sync::Arc;

use async_trait::async_trait;

use super::control_gateway::ControlCommand;
use super::inject::Injector;
use super::translate::BlockKind;

/// The answers that mean "yes" to a plan.
///
/// A plan is confirm-or-cancel, but the wire carries the label that was on the
/// button, so the mapping has to be stated somewhere. Stated here, and narrow:
/// anything not recognised is a cancel, because running a plan nobody clearly
/// approved is the failure that cannot be undone.
const APPROVALS: &[&str] = &["Allow", "Approve", "Confirm", "Yes", "OK"];

fn is_approval(choice: &str) -> bool {
    APPROVALS.iter().any(|a| a.eq_ignore_ascii_case(choice))
}

/// What the daemon says a session is allowed to do.
///
/// A trait so a rebind can be tested without a database, and so the answer has
/// exactly one source. It is deliberately asked *per attach* rather than
/// inherited from whatever the channel was showing before: `mode` rides on
/// every remote turn and is a grant, and the local invariant is that a channel
/// grants exactly what the target session already had. Carrying the previous
/// binding's mode across would let a phone raise a session's privileges simply
/// by switching to it — silently, because every layer would be doing its own
/// job correctly.
#[async_trait]
pub trait SessionAuthority: Send + Sync {
    /// `(mode, execution_tier)` for `session_id`.
    async fn authorization(&self, session_id: &str) -> (Option<String>, Option<String>);

    /// The conversation so far, as frames, oldest first.
    ///
    /// Answering an approval without the exchange around it is answering the
    /// same words the desktop shows but with none of the context — which is
    /// what makes reading it worth a rebind at all.
    async fn history(&self, session_id: &str) -> Vec<serde_json::Value>;

    /// Persist `mode` as the session's own. `false` if it was not stored, in
    /// which case nothing downstream may act as though it had been.
    async fn set_mode(&self, session_id: &str, mode: &str) -> bool;
}

/// The live authority, over the daemon's database.
pub struct StorageAuthority {
    db: Arc<nevoflux_storage::Database>,
}

impl StorageAuthority {
    /// Read authority out of `db`.
    pub fn new(db: Arc<nevoflux_storage::Database>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SessionAuthority for StorageAuthority {
    async fn authorization(&self, session_id: &str) -> (Option<String>, Option<String>) {
        use nevoflux_storage::SessionRepository;
        // `sessions.mode` is the session's own, persisted. Its domain is
        // narrower than the channel's (`chat | agent` against
        // `chat | browser | agent`), so a session that was being driven in
        // browser mode resolves to whatever it was created as. Narrower is the
        // safe direction, and it is recorded rather than papered over.
        let mode = SessionRepository::new(&self.db)
            .get(session_id)
            .ok()
            .flatten()
            .map(|s| s.mode.as_str().to_string());
        let tier = crate::agent_host::resolve_execution_tier_for(&self.db, session_id)
            .as_setting()
            .to_string();
        (mode, Some(tier))
    }

    async fn history(&self, session_id: &str) -> Vec<serde_json::Value> {
        use nevoflux_storage::{ListMessagesParams, MessageRepository};
        // Newest first, then trimmed to a byte budget from that end — the end
        // of a conversation is the part that explains the question being asked
        // now. The count here is only a ceiling on how much is read; `replay`
        // decides how much is sent.
        let params = ListMessagesParams {
            session_id: session_id.to_string(),
            limit: Some(HISTORY_SCAN_LIMIT),
            offset: None,
            before_id: None,
            after_id: None,
        };
        let mut messages = MessageRepository::new(&self.db)
            .list(params)
            .unwrap_or_default();
        messages.reverse();
        super::history::replay(&messages)
    }

    async fn set_mode(&self, session_id: &str, mode: &str) -> bool {
        use nevoflux_storage::{SessionMode, SessionRepository, UpdateSessionParams};
        let Ok(mode) = mode.parse::<SessionMode>() else {
            return false;
        };
        match SessionRepository::new(&self.db)
            .update(session_id, UpdateSessionParams::new().with_mode(mode))
        {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(target: "remote", %session_id, "could not store a session mode: {e}");
                false
            }
        }
    }
}

/// How many stored messages to read before the byte budget is applied.
///
/// A ceiling on the query, not on the replay. `list` takes a count and nothing
/// else, so the byte budget is spent over whatever this returns; two hundred
/// messages is comfortably more prose than the budget can carry.
const HISTORY_SCAN_LIMIT: u32 = 200;

/// Acts on control-channel commands for one pairing.
pub struct ControlCommands {
    injector: Arc<dyn Injector>,
    pairings: Arc<super::pairing::PairingStore>,
    control_channel_id: String,
    /// The data channel this pairing shows conversations on, once one exists.
    data: Option<Arc<super::portal_gateway::PortalGateway>>,
    authority: Option<Arc<dyn SessionAuthority>>,
}

impl ControlCommands {
    /// Commands for the pairing whose control channel is `control_channel_id`.
    pub fn new(
        injector: Arc<dyn Injector>,
        pairings: Arc<super::pairing::PairingStore>,
        control_channel_id: impl Into<String>,
    ) -> Self {
        Self {
            injector,
            pairings,
            control_channel_id: control_channel_id.into(),
            data: None,
            authority: None,
        }
    }

    /// Give this pairing a data channel to show conversations on.
    pub fn with_data_channel(
        mut self,
        data: Arc<super::portal_gateway::PortalGateway>,
        authority: Arc<dyn SessionAuthority>,
    ) -> Self {
        self.data = Some(data);
        self.authority = Some(authority);
        self
    }

    /// The uplink payload for one answer, or `None` if it does not translate.
    ///
    /// Pure, so the shape can be asserted without a daemon behind it. `session`
    /// comes from the runtime map, never from the device — the same rule the
    /// data channel enforces, kept rather than routed around.
    pub fn answer_payload(
        session: &str,
        kind: BlockKind,
        request_id: &str,
        choice: &str,
    ) -> Option<serde_json::Value> {
        let frame = match kind {
            BlockKind::Gate => {
                serde_json::json!({"kind": "gate_response", "id": request_id, "choice": choice})
            }
            BlockKind::Plan => {
                serde_json::json!({"kind": "plan_response", "approved": is_approval(choice)})
            }
        };
        // `message_id` is unused by both arms; `mode` deliberately so. An answer
        // is not a turn — it starts nothing and grants nothing — which is why
        // answering from the list works whatever mode the session is in.
        super::translate::uplink(&frame, session, "", None, &[])
    }
}

#[async_trait]
impl super::ws::ControlCommandSink for ControlCommands {
    async fn handle(&self, command: ControlCommand) {
        match command {
            ControlCommand::Resolve {
                session,
                kind,
                request_id,
                choice,
            } => {
                if let Some(payload) = Self::answer_payload(&session, kind, &request_id, &choice) {
                    self.injector.inject(payload).await;
                } else {
                    tracing::warn!(
                        target: "remote",
                        %session, ?kind,
                        "a control-channel answer did not translate; the turn is still waiting"
                    );
                }
            }
            ControlCommand::Subscribe(sub) => {
                match self.pairings.set_push(&self.control_channel_id, Some(sub)) {
                    Ok(true) => tracing::info!(target: "remote", "this device can be woken"),
                    // The device subscribed and the pairing was revoked in
                    // between. Nothing to store, and nothing wrong either.
                    Ok(false) => tracing::warn!(
                        target: "remote",
                        "a subscription arrived for a pairing that is gone"
                    ),
                    Err(e) => tracing::error!(target: "remote", "could not store a subscription: {e}"),
                }
            }
            ControlCommand::Attach { session } => {
                let (Some(data), Some(authority)) = (&self.data, &self.authority) else {
                    tracing::warn!(
                        target: "remote",
                        "a device asked to open a conversation on a pairing with no data channel"
                    );
                    return;
                };
                let (mode, tier) = authority.authorization(&session).await;
                // Read before the rebind, so the transcript put on the wire is
                // the one that belongs to the session being opened rather than
                // whatever arrives while it is being opened.
                let history = authority.history(&session).await;
                data.attach(session, mode, tier, history).await;
            }
            ControlCommand::SetMode { session, mode } => {
                let Some(authority) = &self.authority else {
                    tracing::warn!(
                        target: "remote",
                        "a device asked to change a mode on a pairing with no data channel"
                    );
                    return;
                };
                // Stored first, and the binding only follows what was stored:
                // the session row is where a mode is a grant, so the channel
                // must never hold one the row does not.
                if !authority.set_mode(&session, &mode).await {
                    return;
                }
                tracing::info!(target: "remote", %session, %mode, "a device changed a session's mode");
                if let Some(data) = &self.data {
                    data.set_mode(&session, &mode).await;
                }
            }
            ControlCommand::Detach => {
                if let Some(data) = &self.data {
                    data.detach().await;
                }
            }
            ControlCommand::Unsubscribe => {
                if let Err(e) = self.pairings.set_push(&self.control_channel_id, None) {
                    tracing::error!(target: "remote", "could not drop a subscription: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::portal_gateway::{PortalGateway, WireSink};
    use super::super::relay_protocol::WireMessage;
    use super::super::session::Wire;
    use tokio::sync::Mutex;

    struct NoInject;
    #[async_trait]
    impl Injector for NoInject {
        async fn inject(&self, _: serde_json::Value) {}
    }

    #[derive(Default)]
    struct CollectSink {
        sent: Mutex<Vec<Wire>>,
    }
    #[async_trait]
    impl WireSink for CollectSink {
        async fn send(&self, wire: Wire) {
            self.sent.lock().await.push(wire);
        }
    }

    /// Stores whatever it is told, unless told to refuse.
    struct Recording {
        refuse: bool,
        stored: Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl SessionAuthority for Recording {
        async fn authorization(&self, _: &str) -> (Option<String>, Option<String>) {
            (Some("chat".into()), None)
        }
        async fn history(&self, _: &str) -> Vec<serde_json::Value> {
            Vec::new()
        }
        async fn set_mode(&self, session_id: &str, mode: &str) -> bool {
            if self.refuse {
                return false;
            }
            self.stored
                .lock()
                .await
                .push((session_id.into(), mode.into()));
            true
        }
    }

    fn commands(refuse: bool) -> (ControlCommands, Arc<Recording>, Arc<CollectSink>) {
        let sink = Arc::new(CollectSink::default());
        let data = Arc::new(PortalGateway::new(
            None,
            sink.clone(),
            "s1",
            Some("chat".into()),
            None,
            "chan",
        ));
        let authority = Arc::new(Recording {
            refuse,
            stored: Mutex::new(Vec::new()),
        });
        let cmds = ControlCommands::new(
            Arc::new(NoInject),
            Arc::new(super::super::pairing::PairingStore::new("/nonexistent/pairings.json")),
            "ctl",
        )
        .with_data_channel(data, authority.clone());
        (cmds, authority, sink)
    }

    fn announced_modes(sent: &[Wire]) -> Vec<String> {
        sent.iter()
            .filter_map(|w| match w {
                Wire::Text(t) => serde_json::from_str::<WireMessage>(t).ok(),
                _ => None,
            })
            .filter_map(|m| match m {
                WireMessage::Frame { frame, .. } if frame["kind"] == "session_state" => {
                    frame["mode"].as_str().map(str::to_string)
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_mode_is_stored_then_carried_by_the_conversation_on_show() {
        use super::super::ws::ControlCommandSink;
        let (cmds, authority, sink) = commands(false);
        cmds.handle(ControlCommand::SetMode {
            session: "s1".into(),
            mode: "agent".into(),
        })
        .await;
        assert_eq!(
            *authority.stored.lock().await,
            vec![("s1".to_string(), "agent".to_string())]
        );
        assert_eq!(announced_modes(&sink.sent.lock().await), vec!["agent"]);
    }

    #[tokio::test]
    async fn the_stored_mode_is_the_one_the_next_attach_reads() {
        use nevoflux_storage::{CreateSessionParams, Database, SessionRepository};
        let db = Arc::new(Database::open_in_memory().unwrap());
        SessionRepository::new(&db)
            .create(CreateSessionParams::new().with_id("s1"))
            .unwrap();
        let authority = StorageAuthority::new(db);
        assert_eq!(authority.authorization("s1").await.0.as_deref(), Some("chat"));

        assert!(authority.set_mode("s1", "agent").await);
        assert_eq!(authority.authorization("s1").await.0.as_deref(), Some("agent"));

        assert!(!authority.set_mode("s1", "browser").await, "not a stored mode");
        assert!(!authority.set_mode("gone", "agent").await, "no such session");
    }

    #[tokio::test]
    async fn a_mode_that_was_not_stored_is_not_granted_on_the_channel() {
        // The session row is where a mode is a grant. A channel holding one
        // the row does not would be the privilege the attach rule exists to
        // stop.
        use super::super::ws::ControlCommandSink;
        let (cmds, _, sink) = commands(true);
        cmds.handle(ControlCommand::SetMode {
            session: "s1".into(),
            mode: "agent".into(),
        })
        .await;
        assert!(announced_modes(&sink.sent.lock().await).is_empty());
    }

    #[test]
    fn a_gate_is_answered_on_the_path_that_unblocks_the_turn() {
        // `BrowserToolResponse`, keyed by request_id — the message the pending
        // oneshot in `BrowserRequestRegistry` is waiting for. The plausible
        // wrong answer, `PermissionResponse`, has no handler at all.
        let payload =
            ControlCommands::answer_payload("s1", BlockKind::Gate, "r7", "Allow").unwrap();
        assert_eq!(payload["type"], "browser_tool_response");
        assert_eq!(payload["payload"]["request_id"], "r7");
        assert_eq!(payload["payload"]["session_id"], "s1");
        assert_eq!(payload["payload"]["result"]["answer"], "Allow");
    }

    #[test]
    fn the_choice_travels_verbatim_not_reduced_to_a_boolean() {
        // The daemon compares it against the option strings it offered, so a
        // gate with three buttons has to keep which one was pressed.
        let payload =
            ControlCommands::answer_payload("s1", BlockKind::Gate, "r7", "Only this once").unwrap();
        assert_eq!(payload["payload"]["result"]["answer"], "Only this once");
    }

    #[test]
    fn a_plan_is_answered_by_session_because_that_is_how_it_is_keyed() {
        let payload =
            ControlCommands::answer_payload("s2", BlockKind::Plan, "token-9", "Allow").unwrap();
        assert_eq!(payload["type"], "plan_response");
        assert_eq!(payload["payload"]["session_id"], "s2");
        assert_eq!(payload["payload"]["response"], "confirmed");
    }

    #[test]
    fn anything_not_clearly_an_approval_cancels_the_plan() {
        // Running a plan nobody clearly approved is the mistake with no undo.
        for choice in ["Deny", "Cancel", "", "maybe", "Allowed"] {
            let payload =
                ControlCommands::answer_payload("s2", BlockKind::Plan, "t", choice).unwrap();
            assert_eq!(
                payload["payload"]["response"], "cancelled",
                "{choice} must not confirm a plan"
            );
        }
    }

    #[test]
    fn approval_words_are_matched_whatever_their_case() {
        for choice in ["allow", "APPROVE", "Confirm", "yes", "ok"] {
            let payload =
                ControlCommands::answer_payload("s2", BlockKind::Plan, "t", choice).unwrap();
            assert_eq!(payload["payload"]["response"], "confirmed", "{choice}");
        }
    }

    #[test]
    fn the_session_is_the_daemons_own_not_one_the_device_named() {
        // The device sends only an opaque id; this is what it becomes. If this
        // ever took a session off the wire, a paired phone could act on any
        // session it could guess.
        let payload =
            ControlCommands::answer_payload("the-real-session", BlockKind::Gate, "r1", "Allow")
                .unwrap();
        assert_eq!(payload["payload"]["session_id"], "the-real-session");
    }
}
