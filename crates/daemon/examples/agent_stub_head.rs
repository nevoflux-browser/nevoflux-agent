//! A head that answers an agent channel with canned browser results.
//!
//! For the Muse client's integration tests: it dials a relay (a fake one in
//! CI) with a fixed token instead of minting an account JWT, and serves the
//! real gateway, envelope and MCP server over `StubBrowserBackend`.
//!
//!   NF_STUB_CHANNEL=chan-1 NF_STUB_CODE=X-7Q2K-9ABC-DEF3 \
//!     cargo run -p nevoflux-daemon --example agent_stub_head

use std::sync::Arc;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use nevoflux_daemon::remote::crypto::derive_channel_key;
use nevoflux_daemon::remote::mcp_gateway::McpGateway;
use nevoflux_daemon::remote::mcp_tools::StubBrowserBackend;
use nevoflux_daemon::remote::portal_gateway::WireSink;
use nevoflux_daemon::remote::session::Wire;
use nevoflux_daemon::remote::ws::{message_to_wire, wire_to_message};
use tokio::sync::mpsc;

struct ChannelSink(mpsc::UnboundedSender<Wire>);

#[async_trait]
impl WireSink for ChannelSink {
    async fn send(&self, wire: Wire) {
        let _ = self.0.send(wire);
    }
}

fn env(name: &str, default: Option<&str>) -> String {
    std::env::var(name)
        .ok()
        .or_else(|| default.map(str::to_string))
        .unwrap_or_else(|| panic!("{name} must be set"))
}

#[tokio::main]
async fn main() {
    let relay = env("NF_STUB_RELAY", Some("ws://127.0.0.1:8765"));
    let channel = env("NF_STUB_CHANNEL", None);
    let code = env("NF_STUB_CODE", None);
    let token = env("NF_STUB_TOKEN", Some("test"));

    let key = derive_channel_key(&code, &channel).expect("key derivation");
    let (tx, mut rx) = mpsc::unbounded_channel::<Wire>();
    let gateway = Arc::new(McpGateway::new(
        key,
        Arc::new(ChannelSink(tx)),
        Arc::new(StubBrowserBackend),
        &channel,
    ));

    let url = format!("{relay}/?c={channel}&t={token}");
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .expect("relay reachable");
    eprintln!("agent_stub_head: connected to {relay} on channel {channel}");
    let (mut write, mut read) = ws.split();

    let writer = tokio::spawn(async move {
        while let Some(wire) = rx.recv().await {
            if write.send(wire_to_message(wire)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = read.next().await {
        if let Some(wire) = message_to_wire(msg) {
            gateway.on_wire_in(&wire).await;
        }
    }
    gateway.on_disconnected().await;
    writer.abort();
    eprintln!("agent_stub_head: relay closed");
}
