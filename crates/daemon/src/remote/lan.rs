//! LAN-direct transport: a paired device on the same network reaches this head
//! without going through the relay.
//!
//! The listener plays the relay's part for one socket per channel — the same
//! `?c=<channel>` dial, the same text `"ping"` → `"pong"` keepalive, the same
//! `{"k":"peers","n":N}` presence notice — and hands every binary frame to the
//! gateway that already serves that channel over the relay. Nothing above the
//! socket changes: frames are sealed with the pairing's channel key exactly as
//! they are on the relay, so a frame that does not open is dropped the same way,
//! and holding the key *is* the authentication. The listener never reads a
//! plaintext frame from a peer as anything but a keepalive: a relay notice
//! spoofed from the LAN would otherwise be able to drive the gateway.
//!
//! While a LAN peer is attached, the channel's [`WsSink`](super::ws::WsSink)
//! writes to it instead of the relay. The relay socket stays dialled (it costs
//! nothing while idle) so a device that walks out of Wi-Fi range finds the head
//! where it left it.
//!
//! Discovery is mDNS: `_nevoflux._tcp.local.` with a TXT `ch` listing the first
//! 8 hex digits of SHA-256 of each data channel id served, so a device can tell
//! which head holds its pairing without the ids being broadcast.
//!
//! `NEVOFLUX_LAN=0` turns all of it off; `NEVOFLUX_LAN_PORT` moves the port.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use super::session::Wire;
use super::ws::WsSink;

/// The default port. Arbitrary, unregistered, and above the ephemeral floor
/// Windows uses so it is never already taken by an outgoing connection.
pub const DEFAULT_PORT: u16 = 47821;

/// mDNS service type a device browses for.
pub const SERVICE_TYPE: &str = "_nevoflux._tcp.local.";

/// A LAN peer silent this long is gone. Three of the device's 30 s pings.
const SILENT_DEADLINE: Duration = Duration::from_secs(90);

/// Outbound frames waiting for a slow LAN socket. A device that cannot keep up
/// with this many is treated as gone rather than left to grow without bound.
const OUTBOUND_DEPTH: usize = 256;

/// Whatever serves one channel, as the LAN listener sees it.
#[async_trait]
pub trait LanEndpoint: Send + Sync {
    /// The sink the gateway writes through; the LAN socket is attached to it.
    fn sink(&self) -> &Arc<WsSink>;
    /// One frame from the LAN peer (or the listener's own presence notice).
    async fn deliver(&self, wire: Wire);
    /// Whether this is a data channel (advertised in the mDNS TXT record).
    fn is_data(&self) -> bool;
}

fn endpoints() -> &'static StdMutex<HashMap<String, Arc<dyn LanEndpoint>>> {
    static E: OnceLock<StdMutex<HashMap<String, Arc<dyn LanEndpoint>>>> = OnceLock::new();
    E.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Offer `channel_id` on the LAN. Starts the listener on first use.
pub fn register(channel_id: &str, endpoint: Arc<dyn LanEndpoint>) {
    if !enabled() {
        return;
    }
    endpoints()
        .lock()
        .expect("lan endpoints")
        .insert(channel_id.to_string(), endpoint);
    ensure_started();
    readvertise();
}

/// Stop offering `channel_id`; a LAN peer on it is dropped with it.
pub fn unregister(channel_id: &str) {
    let gone = endpoints()
        .lock()
        .expect("lan endpoints")
        .remove(channel_id);
    if let Some(ep) = gone {
        ep.sink().clear_lan_any();
        readvertise();
    }
}

fn enabled() -> bool {
    !matches!(
        std::env::var("NEVOFLUX_LAN").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

fn port() -> u16 {
    std::env::var("NEVOFLUX_LAN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// What a device compares against the TXT record: the first 8 hex digits of
/// SHA-256 over the channel id's ASCII.
pub fn channel_tag(channel_id: &str) -> String {
    let digest = Sha256::digest(channel_id.as_bytes());
    hex::encode(&digest[..4])
}

/// The `c` query parameter of a dial, if it names something sensible.
fn channel_of(uri: &str) -> Option<String> {
    let query = uri.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "c"
            && !v.is_empty()
            && v.len() <= 128
            && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .then(|| v.to_string())
    })
}

fn ensure_started() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let port = port();
        tokio::spawn(async move {
            let listener = match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(target: "remote", "LAN listener could not bind :{port}: {e}");
                    return;
                }
            };
            tracing::info!(target: "remote", "LAN listener on :{port}");
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        tokio::spawn(async move {
                            if let Err(e) = serve(stream).await {
                                tracing::debug!(target: "remote", %peer, "LAN socket ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(target: "remote", "LAN accept failed: {e}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
    });
}

async fn serve(stream: TcpStream) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    let mut asked: Option<String> = None;
    let ws = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, resp: Response| {
        asked = channel_of(&req.uri().to_string());
        Ok(resp)
    })
    .await
    .map_err(|e| e.to_string())?;
    let channel = asked.ok_or("no channel in the dial")?;
    let endpoint = endpoints()
        .lock()
        .expect("lan endpoints")
        .get(&channel)
        .cloned()
        .ok_or("channel not served here")?;

    let (mut write, mut read) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Message>(OUTBOUND_DEPTH);
    let conn = endpoint.sink().set_lan(tx.clone());
    tracing::info!(target: "remote", "LAN peer attached to channel {channel}");

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(msg).await.is_err() {
                break;
            }
        }
        let _ = write.close().await;
    });

    // The relay tells a head somebody arrived; so does this. It is what makes
    // the gateway announce itself and replay what went into an empty channel.
    endpoint
        .deliver(Wire::Text(r#"{"k":"peers","n":1}"#.to_string()))
        .await;

    let mut last_inbound = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    ticker.tick().await;
    loop {
        tokio::select! {
            item = read.next() => match item {
                Some(Ok(Message::Binary(b))) => {
                    last_inbound = Instant::now();
                    endpoint.deliver(Wire::Binary(b)).await;
                }
                Some(Ok(Message::Text(t))) => {
                    last_inbound = Instant::now();
                    if t == super::relay_protocol::KEEPALIVE_PING {
                        let _ = tx.try_send(Message::Text(
                            super::relay_protocol::KEEPALIVE_PONG.into(),
                        ));
                    }
                    // Anything else in plaintext is ignored: on the relay only
                    // the relay speaks in the clear.
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => last_inbound = Instant::now(),
            },
            _ = ticker.tick() => {
                if last_inbound.elapsed() >= SILENT_DEADLINE {
                    tracing::info!(target: "remote", "LAN peer on {channel} went silent");
                    break;
                }
            }
        }
    }

    endpoint.sink().clear_lan(conn);
    drop(tx);
    let _ = writer.await;
    tracing::info!(target: "remote", "LAN peer left channel {channel}");
    Ok(())
}

/// (Re)publish the mDNS record with the data channels served right now.
fn readvertise() {
    static DAEMON: OnceLock<Option<mdns_sd::ServiceDaemon>> = OnceLock::new();
    let daemon = DAEMON.get_or_init(|| match mdns_sd::ServiceDaemon::new() {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!(target: "remote", "mDNS unavailable; LAN devices must be pointed at this host: {e}");
            None
        }
    });
    let Some(daemon) = daemon else { return };

    let tags: Vec<String> = {
        let map = endpoints().lock().expect("lan endpoints");
        let mut t: Vec<String> = map
            .iter()
            .filter(|(_, ep)| ep.is_data())
            .map(|(id, _)| channel_tag(id))
            .collect();
        t.sort();
        t
    };
    let host = hostname();
    let instance = format!("nevoflux-{host}");
    // One TXT string holds 255 bytes; 27 tags of 9 fit with room for `ch=`.
    let ch = tags.iter().take(27).cloned().collect::<Vec<_>>().join(",");
    let props = [("v", "1"), ("ch", ch.as_str())];
    match mdns_sd::ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &format!("{host}.local."),
        "",
        port(),
        &props[..],
    ) {
        Ok(info) => {
            if let Err(e) = daemon.register(info.enable_addr_auto()) {
                tracing::warn!(target: "remote", "mDNS register failed: {e}");
            }
        }
        Err(e) => tracing::warn!(target: "remote", "mDNS record rejected: {e}"),
    }
}

fn hostname() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "head".to_string());
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    if clean.is_empty() {
        "head".into()
    } else {
        clean.to_ascii_lowercase()
    }
}

/// A data channel: frames go to the portal gateway, and arriving re-registers
/// it for the M2 tap if a relay outage had detached it.
pub struct DataEndpoint {
    pub sink: Arc<WsSink>,
    pub gateway: Arc<super::portal_gateway::PortalGateway>,
    pub injector: Arc<dyn super::inject::Injector>,
    pub registry: Arc<tokio::sync::Mutex<super::gateway::GatewayRegistry>>,
}

#[async_trait]
impl LanEndpoint for DataEndpoint {
    fn sink(&self) -> &Arc<WsSink> {
        &self.sink
    }
    async fn deliver(&self, wire: Wire) {
        {
            let mut reg = self.registry.lock().await;
            let id = super::gateway::RemoteGateway::id(self.gateway.as_ref()).to_string();
            if !reg.contains(&id) {
                reg.register(self.gateway.clone());
            }
        }
        self.gateway.on_wire_in(wire, self.injector.as_ref()).await;
    }
    fn is_data(&self) -> bool {
        true
    }
}

/// A control channel: frames go to the control gateway, commands to the daemon.
pub struct ControlEndpoint {
    pub sink: Arc<WsSink>,
    pub gateway: Arc<super::control_gateway::ControlGateway>,
    pub on_command: Arc<dyn super::ws::ControlCommandSink>,
}

#[async_trait]
impl LanEndpoint for ControlEndpoint {
    fn sink(&self) -> &Arc<WsSink> {
        &self.sink
    }
    async fn deliver(&self, wire: Wire) {
        if let Some(cmd) = self.gateway.on_wire_in(&wire).await {
            self.on_command.handle(cmd).await;
        }
    }
    fn is_data(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_is_read_off_the_dial() {
        assert_eq!(channel_of("/?c=abc123&t=x").as_deref(), Some("abc123"));
        assert_eq!(channel_of("/?t=x&c=abc-m").as_deref(), Some("abc-m"));
        assert_eq!(channel_of("/?c=").as_deref(), None);
        assert_eq!(channel_of("/?c=../etc").as_deref(), None);
        assert_eq!(channel_of("/").as_deref(), None);
    }

    #[test]
    fn the_tag_is_eight_hex_digits_of_sha256() {
        // sha256("abc") = ba7816bf...
        assert_eq!(channel_tag("abc"), "ba7816bf");
    }

    /// What a channel's gateway saw arrive from the LAN.
    struct Recorder {
        sink: Arc<WsSink>,
        got: tokio::sync::Mutex<Vec<Wire>>,
    }

    #[async_trait]
    impl LanEndpoint for Recorder {
        fn sink(&self) -> &Arc<WsSink> {
            &self.sink
        }
        async fn deliver(&self, wire: Wire) {
            self.got.lock().await.push(wire);
        }
        fn is_data(&self) -> bool {
            true
        }
    }

    /// The whole path a pendant takes: dial `?c=`, be announced, keep alive,
    /// send sealed bytes up, and be the one the gateway's sink writes to.
    #[tokio::test]
    async fn a_lan_peer_is_served_like_a_relay_socket() {
        use super::super::portal_gateway::WireSink;

        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        std::env::set_var("NEVOFLUX_LAN_PORT", port.to_string());
        let ep = Arc::new(Recorder {
            sink: Arc::new(WsSink::new()),
            got: tokio::sync::Mutex::new(Vec::new()),
        });
        register("chan-lan-test", ep.clone());

        let url = format!("ws://127.0.0.1:{port}/?c=chan-lan-test");
        let mut ws = None;
        for _ in 0..50 {
            if let Ok((s, _)) = tokio_tungstenite::connect_async(url.as_str()).await {
                ws = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut ws = ws.expect("the LAN listener should accept");

        // Arrival is announced to the gateway, the way the relay does it.
        for _ in 0..50 {
            if !ep.got.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            ep.got.lock().await.first(),
            Some(&Wire::Text(r#"{"k":"peers","n":1}"#.into()))
        );
        assert!(ep.sink.has_lan());

        // The text keepalive is answered here and never reaches the gateway;
        // other plaintext from a LAN peer is ignored; sealed bytes go up.
        ws.send(Message::Text("ping".into())).await.unwrap();
        ws.send(Message::Text(r#"{"k":"peers","n":0}"#.into())).await.unwrap();
        ws.send(Message::Binary(vec![1, 2, 3])).await.unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(pong, Message::Text("pong".into()));
        for _ in 0..50 {
            if ep.got.lock().await.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            ep.got.lock().await.as_slice(),
            &[
                Wire::Text(r#"{"k":"peers","n":1}"#.into()),
                Wire::Binary(vec![1, 2, 3])
            ]
        );

        // What the gateway sends now goes to the LAN peer, not the relay.
        ep.sink.send(Wire::Binary(vec![9, 9])).await;
        let down = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(down, Message::Binary(vec![9, 9]));

        // Leaving detaches it; an unknown channel is refused outright.
        ws.close(None).await.unwrap();
        for _ in 0..50 {
            if !ep.sink.has_lan() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!ep.sink.has_lan());
        let other = format!("ws://127.0.0.1:{port}/?c=not-served");
        if let Ok((mut s, _)) = tokio_tungstenite::connect_async(other.as_str()).await {
            let next = tokio::time::timeout(Duration::from_secs(2), s.next()).await.unwrap();
            assert!(!matches!(next, Some(Ok(Message::Binary(_)))));
        }
        unregister("chan-lan-test");
    }
}
