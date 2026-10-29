//! BLE transport: a paired pendant with no Wi-Fi way to this head (no network,
//! no internet, head off its LAN) turns Wi-Fi off and advertises over Bluetooth
//! LE instead; this head, as the central, connects and serves it exactly like a
//! LAN socket.
//!
//! The pendant is a GATT peripheral with one service: this head writes to RX
//! (write without response) and gets the pendant's bytes as notifications on
//! TX. Each direction is one ordered byte stream, cut into ATT packets
//! anywhere, carrying records `type (1) | length (3, LE) | payload`:
//!
//! - [`REC_CTL`] / [`REC_DATA`]: one sealed frame of the pairing's control or
//!   data channel — the bytes a binary WebSocket message carries on the LAN.
//! - [`REC_HELLO`] (pendant → head, first): `{"c":"<control id>","d":"<data id>"}`,
//!   what `?c=` says on a socket dial.
//!
//! Above that nothing changes: frames go to the channel's [`LanEndpoint`] and
//! the gateway writes back through the same LAN slot of its sink, so holding
//! the channel key is still the authentication and a frame that does not open
//! is dropped. The pendant advertises the data channel's tag (the same 4 bytes
//! mDNS lists in TXT `ch`) as service data, so this head only connects to
//! pendants whose pairing it holds.
//!
//! The central runs on Windows and macOS always, on Linux with the
//! `ble-linux` feature (BlueZ over D-Bus). `NEVOFLUX_BLE=0` turns it off.

use super::lan::LanEndpoint;

/// The pendant's service; RX and TX are its two characteristics.
pub const SERVICE: uuid::Uuid = uuid::Uuid::from_u128(0x6e660001_7c3f_4b8a_9e0d_5f1b2c3d4e5f);
pub const RX: uuid::Uuid = uuid::Uuid::from_u128(0x6e660002_7c3f_4b8a_9e0d_5f1b2c3d4e5f);
pub const TX: uuid::Uuid = uuid::Uuid::from_u128(0x6e660003_7c3f_4b8a_9e0d_5f1b2c3d4e5f);

pub const REC_CTL: u8 = 0x00;
pub const REC_DATA: u8 = 0x01;
pub const REC_HELLO: u8 = 0x7F;
/// Largest record either side accepts; a long spoken sentence is well under it.
pub const REC_MAX: usize = 2 * 1024 * 1024;

/// One record, header and payload.
pub fn record(kind: u8, payload: &[u8]) -> Vec<u8> {
    let n = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&[kind, n as u8, (n >> 8) as u8, (n >> 16) as u8]);
    out.extend_from_slice(payload);
    out
}

/// Records out of a byte stream that arrives in pieces.
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
}

impl Reassembler {
    /// Every record completed by `bytes`. An oversize record is an error: the
    /// stream cannot be followed past it, so the link should be dropped.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 4 {
                return Ok(out);
            }
            let len = self.buf[1] as usize | (self.buf[2] as usize) << 8 | (self.buf[3] as usize) << 16;
            if len > REC_MAX {
                self.buf.clear();
                return Err(format!("record of {len} B"));
            }
            if self.buf.len() < 4 + len {
                return Ok(out);
            }
            let kind = self.buf[0];
            let payload = self.buf[4..4 + len].to_vec();
            self.buf.drain(..4 + len);
            out.push((kind, payload));
        }
    }
}

/// The channel ids in a hello, if both are ids a dial could name.
pub fn hello_channels(payload: &[u8]) -> Option<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Hello {
        c: String,
        d: String,
    }
    let h: Hello = serde_json::from_slice(payload).ok()?;
    let ok = |v: &str| {
        !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    (ok(&h.c) && ok(&h.d)).then_some((h.c, h.d))
}

/// Whether this head serves the data channel whose tag a pendant advertises.
fn serves_tag(tag: &[u8]) -> bool {
    let want = hex::encode(tag);
    super::lan::endpoints()
        .lock()
        .expect("lan endpoints")
        .iter()
        .any(|(id, ep)| ep.is_data() && super::lan::channel_tag(id) == want)
}

fn endpoint(channel_id: &str) -> Option<std::sync::Arc<dyn LanEndpoint>> {
    super::lan::endpoints()
        .lock()
        .expect("lan endpoints")
        .get(channel_id)
        .cloned()
}

pub(crate) fn enabled() -> bool {
    !matches!(
        std::env::var("NEVOFLUX_BLE").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Start looking for pendants, once. A host without Bluetooth logs it and stops.
pub fn ensure_started() {
    #[cfg(any(windows, target_os = "macos", feature = "ble-linux"))]
    central::ensure_started();
}

#[cfg(any(windows, target_os = "macos", feature = "ble-linux"))]
mod central {
    use std::collections::{HashSet, VecDeque};
    use std::sync::{Arc, Mutex as StdMutex, OnceLock};
    use std::time::Duration;

    use btleplug::api::{Central as _, CentralEvent, Manager as _, Peripheral as _, ScanFilter, WriteType};
    use btleplug::platform::{Adapter, Manager, PeripheralId};
    use futures::StreamExt;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;
    use crate::remote::session::Wire;

    /// Frames waiting for the BLE link, per channel, as on the LAN socket.
    const OUTBOUND_DEPTH: usize = 256;
    /// The first write size tried; a link with a smaller MTU halves it down.
    const FIRST_CHUNK: usize = 244;
    const MIN_CHUNK: usize = 20;

    pub fn ensure_started() {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            tokio::spawn(async {
                if let Err(e) = run().await {
                    tracing::info!(target: "remote", "BLE unavailable, pendants out of Wi-Fi cannot reach this head: {e}");
                }
            });
        });
    }

    async fn run() -> Result<(), String> {
        let manager = Manager::new().await.map_err(|e| e.to_string())?;
        let adapter = manager
            .adapters()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .ok_or("no Bluetooth adapter")?;
        let mut events = adapter.events().await.map_err(|e| e.to_string())?;
        // No service filter: the pendant advertises service data, not a service list.
        adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(target: "remote", "BLE: looking for pendants");
        let busy: Arc<StdMutex<HashSet<PeripheralId>>> = Arc::default();
        while let Some(ev) = events.next().await {
            let CentralEvent::ServiceDataAdvertisement { id, service_data } = ev else {
                continue;
            };
            let Some(tag) = service_data.get(&SERVICE) else {
                continue;
            };
            if tag.len() < 4 || !serves_tag(&tag[..4]) {
                continue;
            }
            if !busy.lock().expect("ble busy").insert(id.clone()) {
                continue;
            }
            let adapter = adapter.clone();
            let busy = busy.clone();
            tokio::spawn(async move {
                match serve(&adapter, &id).await {
                    Ok(()) => tracing::info!(target: "remote", "BLE pendant left"),
                    Err(e) => tracing::info!(target: "remote", "BLE pendant link ended: {e}"),
                }
                busy.lock().expect("ble busy").remove(&id);
            });
        }
        Err("adapter event stream ended".into())
    }

    async fn serve(adapter: &Adapter, id: &PeripheralId) -> Result<(), String> {
        let p = adapter.peripheral(id).await.map_err(|e| e.to_string())?;
        if !p.is_connected().await.unwrap_or(false) {
            tokio::time::timeout(Duration::from_secs(15), p.connect())
                .await
                .map_err(|_| "connect timed out".to_string())?
                .map_err(|e| e.to_string())?;
        }
        let result = linked(&p).await;
        let _ = p.disconnect().await;
        result
    }

    async fn linked(p: &btleplug::platform::Peripheral) -> Result<(), String> {
        p.discover_services().await.map_err(|e| e.to_string())?;
        let chars = p.characteristics();
        let rx = chars.iter().find(|c| c.uuid == RX).cloned().ok_or("no RX characteristic")?;
        let tx = chars.iter().find(|c| c.uuid == TX).cloned().ok_or("no TX characteristic")?;
        let mut notes = p.notifications().await.map_err(|e| e.to_string())?;
        p.subscribe(&tx).await.map_err(|e| e.to_string())?;

        // The hello names the pairing; whatever follows it in the same packets waits.
        let mut re = Reassembler::default();
        let mut pending: VecDeque<(u8, Vec<u8>)> = VecDeque::new();
        let (ctl_id, data_id) = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(n) = notes.next().await {
                if n.uuid != TX {
                    continue;
                }
                pending.extend(re.feed(&n.value)?);
                while let Some((kind, payload)) = pending.pop_front() {
                    if kind == REC_HELLO {
                        return hello_channels(&payload).ok_or_else(|| "a hello that names no channels".to_string());
                    }
                }
            }
            Err("gone before its hello".to_string())
        })
        .await
        .map_err(|_| "no hello".to_string())??;
        let ctl = endpoint(&ctl_id).ok_or("control channel not served here")?;
        let data = endpoint(&data_id).ok_or("data channel not served here")?;
        tracing::info!(target: "remote", "BLE pendant attached to channels {ctl_id} / {data_id}");

        // Both channels' gateways write into one record stream to the pendant.
        let (out_tx, mut out_rx) = mpsc::channel::<(u8, Vec<u8>)>(OUTBOUND_DEPTH);
        let mut attached = Vec::new();
        for (kind, ep) in [(REC_CTL, ctl.clone()), (REC_DATA, data.clone())] {
            let (tx_msg, mut rx_msg) = mpsc::channel::<Message>(OUTBOUND_DEPTH);
            let conn = ep.sink().set_lan(tx_msg);
            let out = out_tx.clone();
            let fwd = tokio::spawn(async move {
                while let Some(msg) = rx_msg.recv().await {
                    // Only sealed frames travel; the LAN's text pong has no use here.
                    if let Message::Binary(b) = msg {
                        if out.send((kind, b.to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
            });
            attached.push((ep, conn, fwd));
        }
        drop(out_tx);
        let writer_peer = p.clone();
        let writer = tokio::spawn(async move {
            let mut chunk = FIRST_CHUNK;
            while let Some((kind, payload)) = out_rx.recv().await {
                let rec = record(kind, &payload);
                let mut off = 0;
                while off < rec.len() {
                    let end = (off + chunk).min(rec.len());
                    match writer_peer.write(&rx, &rec[off..end], WriteType::WithoutResponse).await {
                        Ok(()) => off = end,
                        Err(e) if chunk > MIN_CHUNK => {
                            chunk = (chunk / 2).max(MIN_CHUNK);
                            tracing::debug!(target: "remote", "BLE write of {} B refused ({e}); trying {chunk}", end - off);
                        }
                        Err(e) => {
                            tracing::info!(target: "remote", "BLE write failed: {e}");
                            return;
                        }
                    }
                }
            }
        });

        // As on the LAN: tell each gateway somebody arrived, so it announces
        // itself and replays what went into the empty channel.
        for ep in [&ctl, &data] {
            ep.deliver(Wire::Text(r#"{"k":"peers","n":1}"#.to_string())).await;
        }
        let deliver = |kind: u8, payload: Vec<u8>| {
            let ep = match kind {
                REC_CTL => Some(ctl.clone()),
                REC_DATA => Some(data.clone()),
                _ => None,
            };
            async move {
                if let Some(ep) = ep {
                    ep.deliver(Wire::Binary(payload.into())).await;
                }
            }
        };
        for (kind, payload) in pending.drain(..) {
            deliver(kind, payload).await;
        }

        let mut check = tokio::time::interval(Duration::from_secs(3));
        check.tick().await;
        let outcome = loop {
            tokio::select! {
                n = notes.next() => match n {
                    Some(n) if n.uuid == TX => match re.feed(&n.value) {
                        Ok(recs) => {
                            for (kind, payload) in recs {
                                deliver(kind, payload).await;
                            }
                        }
                        Err(e) => break Err(format!("unreadable stream: {e}")),
                    },
                    Some(_) => {}
                    None => break Ok(()),
                },
                _ = check.tick() => {
                    if !p.is_connected().await.unwrap_or(false) || writer.is_finished() {
                        break Ok(());
                    }
                }
            }
        };

        for (ep, conn, fwd) in attached {
            ep.sink().clear_lan(conn);
            fwd.abort();
        }
        writer.abort();
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_survive_any_packet_cut() {
        let mut stream = record(REC_HELLO, br#"{"c":"ctl-1","d":"data-1"}"#);
        stream.extend(record(REC_CTL, b""));
        let big: Vec<u8> = (0..600u32).map(|i| (i + 7) as u8).collect();
        stream.extend(record(REC_DATA, &big));
        for cut in [1, 3, 4, 20, 244, stream.len()] {
            let mut re = Reassembler::default();
            let mut got = Vec::new();
            for piece in stream.chunks(cut) {
                got.extend(re.feed(piece).unwrap());
            }
            assert_eq!(got.len(), 3, "cut {cut}");
            assert_eq!(got[0].0, REC_HELLO);
            assert_eq!(
                hello_channels(&got[0].1),
                Some(("ctl-1".to_string(), "data-1".to_string()))
            );
            assert_eq!(got[1], (REC_CTL, Vec::new()));
            assert_eq!(got[2], (REC_DATA, big.clone()));
        }
    }

    #[test]
    fn the_header_matches_the_pendant() {
        // nf_ble_frame.c: type, then the length little-endian in three bytes
        assert_eq!(record(REC_DATA, &[9u8; 0x0102])[..4], [0x01, 0x02, 0x01, 0x00]);
    }

    #[test]
    fn oversize_and_bad_hellos_are_refused() {
        let n = (REC_MAX + 1) as u32;
        let header = [REC_DATA, n as u8, (n >> 8) as u8, (n >> 16) as u8];
        assert!(Reassembler::default().feed(&header).is_err());
        assert_eq!(hello_channels(br#"{"c":"a b","d":"x"}"#), None);
        assert_eq!(hello_channels(br#"{"c":"a"}"#), None);
        assert_eq!(hello_channels(b"not json"), None);
    }

    #[test]
    fn uuids_match_the_pendant() {
        assert_eq!(SERVICE.to_string(), "6e660001-7c3f-4b8a-9e0d-5f1b2c3d4e5f");
        assert_eq!(TX.to_string(), "6e660003-7c3f-4b8a-9e0d-5f1b2c3d4e5f");
    }
}
