//! The gateway behind an agent pairing's channel (design §3, §5).
//!
//! One MCP session per connection of the agent, never longer. The relay's
//! plaintext presence notice is the only sign of a connection: a rise in the
//! count means somebody (re)joined, so the old session is dropped and a fresh
//! challenge goes out before anything else; zero means nobody is left. Frames
//! arrive sealed, are checked against this connection's envelope, and only
//! then reach rmcp as JSON-RPC.

use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use nevoflux_mcp::rmcp::service::{RxJsonRpcMessage, ServiceExt, TxJsonRpcMessage};
use nevoflux_mcp::rmcp::RoleServer;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::envelope::{new_challenge, InboundVerifier, OutboundSealer};
use super::gateway::{OutboundEvent, RemoteGateway};
use super::mcp_server::AgentMcpServer;
use super::mcp_tools::AgentToolBackend;
use super::portal_gateway::WireSink;
use super::relay_protocol::{peer_count, WireMessage};
use super::session::Wire;

/// Messages buffered between the socket and rmcp, each way.
const BUFFER: usize = 32;

struct Live {
    verifier: InboundVerifier,
    inbound: mpsc::Sender<RxJsonRpcMessage<RoleServer>>,
    cancel: CancellationToken,
}

pub struct McpGateway {
    id: String,
    /// Always present: an agent channel has no plaintext mode (design §5.1).
    key: [u8; 32],
    sink: Arc<dyn WireSink>,
    backend: Arc<dyn AgentToolBackend>,
    live: Mutex<Option<Live>>,
    peers: Mutex<u64>,
}

impl McpGateway {
    pub fn new(
        key: [u8; 32],
        sink: Arc<dyn WireSink>,
        backend: Arc<dyn AgentToolBackend>,
        channel_id: &str,
    ) -> Self {
        Self {
            id: format!("mcp:{channel_id}"),
            key,
            sink,
            backend,
            live: Mutex::new(None),
            peers: Mutex::new(0),
        }
    }

    /// Everything the relay hands this channel.
    pub async fn on_wire_in(&self, wire: &Wire) {
        if let Wire::Text(text) = wire {
            if let Some(n) = peer_count(text) {
                self.on_presence(n).await;
            } else {
                // The relay's presence notice is the only plaintext this
                // channel carries. Anything else in text is outside the seal:
                // never parsed, never counted toward the refusal budget, so
                // whoever can write to the relay can neither speak for the
                // agent nor spend its session.
                tracing::debug!(target: "remote", channel = %self.id, "plaintext agent frame dropped");
            }
            return;
        }
        let Some(WireMessage::Frame { frame, .. }) =
            super::channel_codec::decode(Some(&self.key), wire)
        else {
            tracing::warn!(
                target: "remote",
                channel = %self.id,
                "a sealed agent frame would not open; the pairing code may not match"
            );
            return;
        };

        let mut live = self.live.lock().await;
        let Some(session) = live.as_mut() else {
            tracing::debug!(target: "remote", channel = %self.id, "agent frame before any session; dropped");
            return;
        };
        match session.verifier.accept(&frame) {
            Ok(message) => match serde_json::from_value::<RxJsonRpcMessage<RoleServer>>(message) {
                Ok(msg) => {
                    // Never wait here: `live` is held, and a stalled rmcp
                    // must not keep end_session from tearing the session
                    // down. A full buffer means the agent is flooding or
                    // rmcp is stuck; the frame is dropped (its counter is
                    // already spent, so a resend is refused as a replay).
                    if let Err(e) = session.inbound.try_send(msg) {
                        if e.is_full() {
                            tracing::warn!(target: "remote", channel = %self.id, "MCP session overloaded; frame dropped");
                        } else {
                            tracing::debug!(target: "remote", channel = %self.id, "MCP session already gone");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "remote", channel = %self.id, "agent sent something that is not JSON-RPC: {e}");
                }
            },
            Err(reason) => {
                tracing::warn!(
                    target: "remote",
                    channel = %self.id,
                    reason = reason.as_str(),
                    rejected = session.verifier.rejected(),
                    "agent frame refused"
                );
                if session.verifier.exhausted() {
                    if let Some(dead) = live.take() {
                        dead.cancel.cancel();
                    }
                    tracing::warn!(target: "remote", channel = %self.id, "too many refused frames; MCP session dropped");
                }
            }
        }
    }

    /// The head's own socket went away: whatever session there was is over.
    pub async fn on_disconnected(&self) {
        *self.peers.lock().await = 0;
        self.end_session().await;
    }

    async fn on_presence(&self, n: u64) {
        let before = {
            let mut peers = self.peers.lock().await;
            std::mem::replace(&mut *peers, n)
        };
        tracing::info!(target: "remote", channel = %self.id, peers = n, "agent channel presence");
        if n == 0 {
            self.end_session().await;
        } else if n > before {
            self.start_session().await;
        }
    }

    async fn end_session(&self) {
        if let Some(old) = self.live.lock().await.take() {
            old.cancel.cancel();
            tracing::info!(target: "remote", channel = %self.id, "agent MCP session ended");
        }
    }

    async fn start_session(&self) {
        self.end_session().await;

        let challenge = new_challenge();
        let mut sealer = OutboundSealer::new(challenge.clone());
        let cancel = CancellationToken::new();
        let (out_tx, mut out_rx) = mpsc::channel::<TxJsonRpcMessage<RoleServer>>(BUFFER);
        let (in_tx, in_rx) = mpsc::channel::<RxJsonRpcMessage<RoleServer>>(BUFFER);

        // The challenge goes out before anything else can.
        send_sealed(&self.sink, &self.key, sealer.challenge_frame()).await;

        {
            let (sink, key, cancel, id) =
                (self.sink.clone(), self.key, cancel.clone(), self.id.clone());
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        next = out_rx.next() => match next {
                            Some(msg) => match serde_json::to_value(&msg) {
                                Ok(v) => send_sealed(&sink, &key, sealer.wrap(v)).await,
                                Err(e) => tracing::warn!(target: "remote", channel = %id, "could not serialise an MCP message: {e}"),
                            },
                            None => break,
                        },
                    }
                }
            });
        }
        {
            let (backend, cancel, id) = (self.backend.clone(), cancel.clone(), self.id.clone());
            tokio::spawn(async move {
                match AgentMcpServer::new(backend)
                    .serve_with_ct((out_tx, in_rx), cancel)
                    .await
                {
                    Ok(running) => {
                        let _ = running.waiting().await;
                    }
                    Err(e) => {
                        tracing::debug!(target: "remote", channel = %id, "agent MCP session never initialised: {e}");
                    }
                }
            });
        }

        *self.live.lock().await = Some(Live {
            verifier: InboundVerifier::new(challenge),
            inbound: in_tx,
            cancel,
        });
        tracing::info!(target: "remote", channel = %self.id, "agent MCP session started");
    }
}

async fn send_sealed(sink: &Arc<dyn WireSink>, key: &[u8; 32], frame: Value) {
    let wire = super::channel_codec::encode(Some(key), &WireMessage::Frame { seq: None, frame });
    sink.send(wire).await;
}

#[async_trait]
impl RemoteGateway for McpGateway {
    fn id(&self) -> &str {
        &self.id
    }

    /// Nothing is projected in M1; reminders arrive in M3b (design §19).
    async fn project(&self, ev: &OutboundEvent) {
        let _ = ev;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::channel_codec;
    use crate::remote::mcp_tools::StubBrowserBackend;
    use serde_json::json;
    use std::time::Duration;

    const KEY: [u8; 32] = [7u8; 32];

    #[derive(Default)]
    struct Collect {
        wires: std::sync::Mutex<Vec<Wire>>,
    }

    #[async_trait]
    impl WireSink for Collect {
        async fn send(&self, wire: Wire) {
            self.wires.lock().unwrap().push(wire);
        }
    }

    impl Collect {
        fn frames(&self) -> Vec<Value> {
            self.wires
                .lock()
                .unwrap()
                .iter()
                .filter_map(|w| match channel_codec::decode(Some(&KEY), w) {
                    Some(WireMessage::Frame { frame, .. }) => Some(frame),
                    _ => None,
                })
                .collect()
        }
    }

    fn build() -> (Arc<McpGateway>, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        let gw = Arc::new(McpGateway::new(
            KEY,
            sink.clone(),
            Arc::new(StubBrowserBackend),
            "chan-1",
        ));
        (gw, sink)
    }

    fn presence(n: u64) -> Wire {
        Wire::Text(format!(r#"{{"k":"peers","n":{n}}}"#))
    }

    fn sealed(key: &[u8; 32], frame: Value) -> Wire {
        channel_codec::encode(Some(key), &WireMessage::Frame { seq: None, frame })
    }

    fn c2h(n: u64, ch: &str, m: Value) -> Wire {
        sealed(&KEY, json!({"d": "c2h", "n": n, "ch": ch, "m": m}))
    }

    fn initialize(id: u64) -> Value {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        })
    }

    fn initialized() -> Value {
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    }

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    async fn wait_for(sink: &Collect, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        for _ in 0..250 {
            if let Some(f) = sink.frames().into_iter().find(|f| pred(f)) {
                return Some(f);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }

    fn answers(sink: &Collect, id: u64) -> usize {
        sink.frames()
            .iter()
            .filter(|f| f["m"]["id"] == json!(id))
            .count()
    }

    fn challenges(sink: &Collect) -> Vec<String> {
        sink.frames()
            .iter()
            .filter(|f| f["d"] == "h2c" && f["m"].is_null())
            .map(|f| f["ch"].as_str().unwrap().to_string())
            .collect()
    }

    /// Presence → challenge → initialize → initialized. Returns the challenge
    /// and the next free inbound counter.
    async fn handshake(gw: &McpGateway, sink: &Collect, peers: u64) -> (String, u64) {
        let before = challenges(sink).len();
        gw.on_wire_in(&presence(peers)).await;
        let ch = challenges(sink)
            .get(before)
            .cloned()
            .expect("a challenge goes out first");
        gw.on_wire_in(&c2h(0, &ch, initialize(1))).await;
        wait_for(sink, |f| f["ch"] == json!(ch) && f["m"]["id"] == json!(1))
            .await
            .expect("initialize answered");
        gw.on_wire_in(&c2h(1, &ch, initialized())).await;
        (ch, 2)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_handshake_reports_identity_and_protocol() {
        let (gw, sink) = build();
        gw.on_wire_in(&presence(1)).await;
        let ch = challenges(&sink).pop().expect("challenge");
        let first = sink.frames().remove(0);
        assert_eq!(first, json!({"d": "h2c", "n": 0, "ch": ch, "m": null}));
        gw.on_wire_in(&c2h(0, &ch, initialize(1))).await;
        let reply = wait_for(&sink, |f| f["m"]["id"] == json!(1))
            .await
            .expect("answered");
        assert_eq!(reply["d"], "h2c");
        assert_eq!(reply["n"], 1);
        assert_eq!(reply["m"]["result"]["serverInfo"]["name"], "nevoflux-head");
        assert_eq!(
            reply["m"]["result"]["capabilities"]["experimental"]["nevoflux"]["protocol"],
            json!(1)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_list_shows_only_whitelisted_tools() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        gw.on_wire_in(&c2h(n, &ch, request(2, "tools/list", json!({}))))
            .await;
        let reply = wait_for(&sink, |f| f["m"]["id"] == json!(2))
            .await
            .expect("listed");
        let mut names: Vec<String> = reply["m"]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["browser_navigate", "browser_snapshot"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tool_off_the_whitelist_is_refused_with_a_code() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        let call = request(
            3,
            "tools/call",
            json!({"name": "bash", "arguments": {"command": "id"}}),
        );
        gw.on_wire_in(&c2h(n, &ch, call)).await;
        let reply = wait_for(&sink, |f| f["m"]["id"] == json!(3))
            .await
            .expect("answered");
        assert_eq!(reply["m"]["error"]["code"], json!(-32602));
        assert_eq!(reply["m"]["error"]["data"]["code"], "not_allowed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_whitelisted_tool_reaches_the_backend() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        let call = request(
            4,
            "tools/call",
            json!({"name": "browser_snapshot", "arguments": {}}),
        );
        gw.on_wire_in(&c2h(n, &ch, call)).await;
        let reply = wait_for(&sink, |f| f["m"]["id"] == json!(4))
            .await
            .expect("answered");
        assert!(reply["m"]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Example Domain"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replayed_request_is_answered_once() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        let frame = c2h(n, &ch, request(5, "tools/list", json!({})));
        gw.on_wire_in(&frame).await;
        wait_for(&sink, |f| f["m"]["id"] == json!(5))
            .await
            .expect("answered");
        gw.on_wire_in(&frame).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(answers(&sink, 5), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reflected_frame_is_ignored() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        // A head-to-controller envelope reflected back, carrying a real request.
        let reflected = sealed(
            &KEY,
            json!({"d": "h2c", "n": n, "ch": ch, "m": request(20, "tools/list", json!({}))}),
        );
        gw.on_wire_in(&reflected).await;
        // Positive control: the session is alive, and anything queued earlier
        // on the same ordered path would have been answered before this.
        gw.on_wire_in(&c2h(n, &ch, request(21, "tools/list", json!({}))))
            .await;
        wait_for(&sink, |f| f["m"]["id"] == json!(21))
            .await
            .expect("session still alive");
        assert_eq!(answers(&sink, 20), 0, "nothing answers a reflection");
    }

    /// rmcp never reads (its end of the buffer is kept but idle), as when it is
    /// wedged on a stalled socket. Frames beyond the buffer must neither block
    /// the reader nor keep the session from being torn down.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_session_can_still_be_torn_down() {
        let (gw, _sink) = build();
        let challenge = new_challenge();
        let (tx, _idle_rx) = mpsc::channel::<RxJsonRpcMessage<RoleServer>>(BUFFER);
        *gw.live.lock().await = Some(Live {
            verifier: InboundVerifier::new(challenge.clone()),
            inbound: tx,
            cancel: CancellationToken::new(),
        });
        for i in 0..(BUFFER as u64 * 3) {
            let wire = c2h(i, &challenge, request(100 + i, "ping", json!({})));
            tokio::time::timeout(Duration::from_secs(2), gw.on_wire_in(&wire))
                .await
                .expect("a flood never blocks the reader");
        }
        tokio::time::timeout(Duration::from_secs(2), gw.on_wire_in(&presence(0)))
            .await
            .expect("teardown is not stuck behind a stalled session");
        assert!(gw.live.lock().await.is_none());
        tokio::time::timeout(Duration::from_secs(2), gw.on_disconnected())
            .await
            .expect("disconnect completes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rising_head_count_restarts_the_session() {
        let (gw, sink) = build();
        let (old, n) = handshake(&gw, &sink, 1).await;
        // The agent reconnected before the relay dropped its old socket.
        let (new, m) = handshake(&gw, &sink, 2).await;
        assert_ne!(old, new);
        gw.on_wire_in(&c2h(n + 10, &old, request(6, "tools/list", json!({}))))
            .await;
        gw.on_wire_in(&c2h(m, &new, request(7, "tools/list", json!({}))))
            .await;
        wait_for(&sink, |f| f["m"]["id"] == json!(7))
            .await
            .expect("new session answers");
        assert_eq!(answers(&sink, 6), 0, "the old challenge is dead");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nobody_left_ends_the_session() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        gw.on_wire_in(&presence(0)).await;
        gw.on_wire_in(&c2h(n, &ch, request(8, "tools/list", json!({}))))
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(answers(&sink, 8), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_head_disconnect_ends_the_session() {
        let (gw, sink) = build();
        let (old, n) = handshake(&gw, &sink, 1).await;
        gw.on_disconnected().await;
        gw.on_wire_in(&c2h(n, &old, request(9, "tools/list", json!({}))))
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(answers(&sink, 9), 0);
        // Reconnected: the relay announces the agent again, a new session starts.
        let (new, _) = handshake(&gw, &sink, 1).await;
        assert_ne!(old, new);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eight_bad_frames_drop_the_session() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        for i in 0..8 {
            gw.on_wire_in(&c2h(
                i,
                "ZZZZZZZZZZZZZZZZZZZZZZ",
                request(100 + i, "ping", json!({})),
            ))
            .await;
        }
        gw.on_wire_in(&c2h(n, &ch, request(10, "tools/list", json!({}))))
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(answers(&sink, 10), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_frame_before_any_session_is_dropped() {
        let (gw, sink) = build();
        gw.on_wire_in(&c2h(0, "AAECAwQFBgcICQoLDA0ODw", initialize(1)))
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(sink.frames().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_garbage_message_does_not_kill_the_session() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        gw.on_wire_in(&c2h(n, &ch, json!({"foo": 1}))).await;
        gw.on_wire_in(&c2h(n + 1, &ch, request(11, "tools/list", json!({}))))
            .await;
        wait_for(&sink, |f| f["m"]["id"] == json!(11))
            .await
            .expect("still answering");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_key_frames_do_not_exhaust_the_session() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        let other = [9u8; 32];
        for i in 0..8 {
            let f =
                json!({"d": "c2h", "n": n + i, "ch": ch, "m": request(200 + i, "ping", json!({}))});
            gw.on_wire_in(&sealed(&other, f)).await;
        }
        gw.on_wire_in(&c2h(n, &ch, request(12, "tools/list", json!({}))))
            .await;
        wait_for(&sink, |f| f["m"]["id"] == json!(12))
            .await
            .expect("unaffected");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plaintext_envelopes_are_never_read_and_never_spend_the_session() {
        let (gw, sink) = build();
        let (ch, n) = handshake(&gw, &sink, 1).await;
        // A well-formed envelope with the right challenge and counter, but in
        // text: outside the seal. More of them than the refusal budget.
        for i in 0..(crate::remote::envelope::BAD_FRAME_LIMIT as u64 * 2) {
            let frame = json!({"d": "c2h", "n": n + i, "ch": ch, "m": request(300 + i, "tools/list", json!({}))});
            let text = serde_json::to_string(&WireMessage::Frame { seq: None, frame }).unwrap();
            gw.on_wire_in(&Wire::Text(text)).await;
        }
        // Positive control: the same counter, sealed, is still fresh and answered.
        gw.on_wire_in(&c2h(n, &ch, request(13, "tools/list", json!({}))))
            .await;
        wait_for(&sink, |f| f["m"]["id"] == json!(13))
            .await
            .expect("the session survived the plaintext");
        for i in 0..(crate::remote::envelope::BAD_FRAME_LIMIT as u64 * 2) {
            assert_eq!(answers(&sink, 300 + i), 0, "plaintext was answered");
        }
    }

    #[test]
    fn the_gateway_id_names_the_channel() {
        let sink = Arc::new(Collect::default());
        let gw = McpGateway::new(KEY, sink, Arc::new(StubBrowserBackend), "abc");
        assert_eq!(RemoteGateway::id(&gw), "mcp:abc");
    }
}
