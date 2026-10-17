//! The task engine's view of the running daemon.

use std::sync::Arc;

use async_trait::async_trait;
use nevoflux_protocol::{Channel, DaemonEnvelope, ProxyEnvelope};
use nevoflux_storage::Database;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::engine::TaskHost;
use super::limits::SCREENSHOT_WAIT;
use crate::pending::Pending;
use crate::registry::ConnectedClients;
use crate::remote::pairing::{Pairing, PairingStore};
use crate::wasm::services::{BrowserRequest, BrowserResponse, BrowserSender};

/// The daemon as the task engine sees it.
pub struct DaemonTaskHost {
    database: Arc<Database>,
    msg_tx: mpsc::Sender<(Vec<u8>, ProxyEnvelope)>,
    response_tx: mpsc::Sender<(Vec<u8>, DaemonEnvelope)>,
    browser_tx: Option<BrowserSender>,
    /// The daemon's live config (`server.rs` `agent_config`, the same
    /// `SharedAgentConfig` a local turn reads), for R38's provider check.
    shared_config: crate::server::SharedAgentConfig,
}

impl DaemonTaskHost {
    /// `response_tx` must be the chat writer's queue every turn sends on:
    /// the turn-end marker [`TaskHost::send`] queues means "the turn's last
    /// frame passed the tap" only behind those frames.
    pub fn new(
        database: Arc<Database>,
        msg_tx: mpsc::Sender<(Vec<u8>, ProxyEnvelope)>,
        response_tx: mpsc::Sender<(Vec<u8>, DaemonEnvelope)>,
        browser_tx: Option<BrowserSender>,
        shared_config: crate::server::SharedAgentConfig,
    ) -> Self {
        Self {
            database,
            msg_tx,
            response_tx,
            browser_tx,
            shared_config,
        }
    }
}

/// The connection a delegated turn runs under: the sidebar that last sent a
/// turn of its own, else the only connection — the rule
/// `registry::browser_target_for` uses for an injected turn's browser calls.
pub fn pick_sidebar(clients: &ConnectedClients) -> Option<String> {
    clients
        .last_local()
        .or_else(|| clients.single())
        .map(|(proxy, _)| proxy)
}

/// The agent pairing on `channel`, read from `store` now: never cached (the
/// toggle and the tier are honoured from the next call on, R30/R39), and an
/// unreadable file reads as no pairing at all (fail closed).
fn agent_pairing(store: &PairingStore, channel: &str) -> Option<Pairing> {
    store
        .load()
        .ok()?
        .into_iter()
        .find(|p| p.control_channel_id == channel && p.is_agent())
}

#[async_trait]
impl TaskHost for DaemonTaskHost {
    fn sidebar(&self) -> Option<String> {
        pick_sidebar(crate::registry::CURRENT_CONNECTED_CLIENTS.get()?)
    }

    fn is_connected(&self, proxy_id: &str) -> bool {
        crate::registry::CURRENT_CONNECTED_CLIENTS
            .get()
            .is_some_and(|clients| clients.is_connected(proxy_id))
    }

    fn database(&self) -> Arc<Database> {
        self.database.clone()
    }

    fn pairing(&self, channel: &str) -> Option<Pairing> {
        agent_pairing(
            &crate::remote::start::control_deps()?.agent_pairings,
            channel,
        )
    }

    fn pending(&self) -> Arc<Pending> {
        crate::pending::shared()
    }

    fn acp_busy(&self) -> Option<String> {
        // The provider the injected turn will use: the active one, read now,
        // parsed exactly as the turn parses it (`ProviderType: FromStr`).
        // Read now, never cached: config.llm.set swaps the inner Arc.
        let config = self.shared_config.read().ok()?.clone();
        let id = config.llm.active_provider()?;
        if !crate::config::is_acp_provider(id) {
            return None;
        }
        let key = format!("{:?}", id.parse::<nevoflux_llm::ProviderType>().ok()?);
        (crate::wasm::acp_lease::holder_count(&key) > 0).then_some(key)
    }

    async fn send(&self, proxy_id: &str, payload: Value) {
        // A connection's identity is its proxy id's bytes (`handle_proxy_connection`).
        let envelope = DaemonEnvelope::new(proxy_id, Channel::Chat, payload);
        let _ = self
            .response_tx
            .send((proxy_id.as_bytes().to_vec(), envelope))
            .await;
    }

    async fn inject(&self, proxy_id: &str, payload: Value) -> bool {
        let envelope = ProxyEnvelope::new(
            proxy_id.to_string(),
            uuid::Uuid::new_v4().to_string(),
            Channel::Chat,
            payload,
        );
        self.msg_tx
            .send((proxy_id.as_bytes().to_vec(), envelope))
            .await
            .is_ok()
    }

    async fn browser(&self, request: BrowserRequest) -> Option<BrowserResponse> {
        let sender = self.browser_tx.as_ref()?;
        let (tx, rx) = oneshot::channel();
        sender.send((request, tx)).await.ok()?;
        tokio::time::timeout(SCREENSHOT_WAIT, rx).await.ok()?.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::pairing::PairingKind;
    use serde_json::json;

    #[test]
    fn the_sidebar_is_the_connection_that_last_spoke_else_the_only_one() {
        let clients = ConnectedClients::new();
        assert_eq!(pick_sidebar(&clients), None);
        clients.register("p1", b"p1".to_vec());
        assert_eq!(
            pick_sidebar(&clients).as_deref(),
            Some("p1"),
            "the only one"
        );
        clients.register("p2", b"p2".to_vec());
        assert_eq!(
            pick_sidebar(&clients),
            None,
            "two, and neither has sent a turn"
        );
        clients.note_local_turn("p2");
        assert_eq!(pick_sidebar(&clients).as_deref(), Some("p2"));
    }

    type Pipes = (
        DaemonTaskHost,
        mpsc::Receiver<(Vec<u8>, ProxyEnvelope)>,
        mpsc::Sender<(Vec<u8>, DaemonEnvelope)>,
        mpsc::Receiver<(Vec<u8>, DaemonEnvelope)>,
    );

    fn host() -> Pipes {
        let (msg_tx, msg_rx) = mpsc::channel(8);
        let (response_tx, response_rx) = mpsc::channel(8);
        let config: crate::server::SharedAgentConfig = Arc::new(std::sync::RwLock::new(Arc::new(
            crate::config::AgentConfig::default(),
        )));
        let host = DaemonTaskHost::new(
            Arc::new(Database::open_in_memory().unwrap()),
            msg_tx,
            response_tx.clone(),
            None,
            config,
        );
        (host, msg_rx, response_tx, response_rx)
    }

    /// The turn-end marker means "the turn's last frame passed the tap" only
    /// if it queues behind those frames on the one chat writer the turn uses.
    #[tokio::test]
    async fn a_frame_the_engine_sends_queues_behind_the_turns_frames_on_the_chat_writer() {
        let (host, _msg_rx, turn_tx, mut writer) = host();
        let last = DaemonEnvelope::new("p1", Channel::Chat, json!({"type": "stream_end"}));
        turn_tx.send((b"p1".to_vec(), last)).await.unwrap();
        host.send("p1", json!({"type": "session_origin"})).await;

        let (identity, first) = writer.recv().await.unwrap();
        assert_eq!(
            (identity.as_slice(), first.payload["type"].as_str()),
            (&b"p1"[..], Some("stream_end"))
        );
        let (identity, marker) = writer.recv().await.unwrap();
        assert_eq!(identity, b"p1".to_vec());
        assert_eq!(marker.channel, Channel::Chat, "past the chat tap");
        assert_eq!(marker.payload, json!({"type": "session_origin"}));
    }

    #[tokio::test]
    async fn an_injected_turn_enters_the_message_loop_as_the_sidebar() {
        let (host, mut msg_rx, _turn_tx, _writer) = host();
        assert!(host.inject("p1", json!({"type": "chat_message"})).await);
        let (identity, envelope) = msg_rx.recv().await.unwrap();
        assert_eq!(identity, b"p1".to_vec());
        assert_eq!(envelope.channel, Channel::Chat);
        assert_eq!(envelope.payload, json!({"type": "chat_message"}));
    }

    #[tokio::test]
    async fn an_injection_with_no_message_loop_is_reported_not_lost() {
        let (host, msg_rx, _turn_tx, _writer) = host();
        drop(msg_rx);
        assert!(!host.inject("p1", json!({"type": "chat_message"})).await);
    }

    fn agent(id: &str, guarded: bool) -> Pairing {
        Pairing {
            control_channel_id: id.into(),
            data_channel_id: String::new(),
            control_key: "cc".repeat(32),
            data_key: String::new(),
            created_at: 2,
            label: None,
            push: None,
            kind: PairingKind::Agent,
            tier: None,
            paused: false,
            muse_may_approve_guarded: guarded,
        }
    }

    #[test]
    fn the_pairing_is_read_from_the_file_at_every_call_and_unreadable_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-pairings.json");
        let store = PairingStore::new(path.clone());
        assert_eq!(agent_pairing(&store, "a1"), None, "no file yet");
        store.add(agent("a1", false)).unwrap();
        assert_eq!(agent_pairing(&store, "a1"), Some(agent("a1", false)));
        // Another writer changes the file: the next read sees it.
        PairingStore::new(path.clone())
            .set_muse_may_approve_guarded("a1", true)
            .unwrap();
        assert!(
            agent_pairing(&store, "a1")
                .unwrap()
                .muse_may_approve_guarded
        );
        assert_eq!(agent_pairing(&store, "nobody"), None);
        std::fs::write(&path, "{ nope").unwrap();
        assert_eq!(agent_pairing(&store, "a1"), None, "fail closed");
    }
}
