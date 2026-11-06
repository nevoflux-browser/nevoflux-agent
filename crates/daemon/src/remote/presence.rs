//! Who is on each channel this head serves, and by which way: a device
//! attached directly (LAN socket or BLE), and how many peers the relay says it
//! has. Read by the settings page's pendant list (`pendant.list`).
//!
//! Written where those facts are learnt — `lan.rs` and `ble.rs` when a peer
//! attaches or leaves, the data gateway when the relay's presence notice
//! arrives — and never consulted for routing, which `WsSink` decides itself.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// How a device reaches this head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Lan,
    Ble,
    Relay,
}

impl Link {
    pub fn as_str(self) -> &'static str {
        match self {
            Link::Lan => "lan",
            Link::Ble => "ble",
            Link::Relay => "relay",
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Presence {
    /// The direct peer, with the connection number `WsSink::set_lan` gave it.
    direct: Option<(Link, u64)>,
    relay_peers: u32,
}

fn map() -> &'static Mutex<HashMap<String, Presence>> {
    static M: OnceLock<Mutex<HashMap<String, Presence>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A device attached to `channel` directly, as connection `conn`.
pub fn attached(channel: &str, link: Link, conn: u64) {
    map().lock().expect("presence").entry(channel.to_string()).or_default().direct = Some((link, conn));
}

/// Connection `conn` on `channel` left; a newer one that replaced it stays.
pub fn detached(channel: &str, conn: u64) {
    if let Some(p) = map().lock().expect("presence").get_mut(channel) {
        if p.direct.map(|(_, c)| c) == Some(conn) {
            p.direct = None;
        }
    }
}

/// The relay said how many others are on `channel`.
pub fn relay_peers(channel: &str, n: u32) {
    map().lock().expect("presence").entry(channel.to_string()).or_default().relay_peers = n;
}

/// The channel's relay socket went down: whoever was there is unknown now.
pub fn relay_lost(channel: &str) {
    relay_peers(channel, 0);
}

/// How the device on `channel` is reached right now, if it is. A direct peer
/// wins: it is what frames go to while it is attached.
pub fn link(channel: &str) -> Option<Link> {
    let m = map().lock().expect("presence");
    let p = m.get(channel)?;
    p.direct.map(|(l, _)| l).or((p.relay_peers > 0).then_some(Link::Relay))
}

/// The channel is no longer served (unpaired).
pub fn forget(channel: &str) {
    map().lock().expect("presence").remove(channel);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_wins_and_a_replaced_socket_cannot_clear_its_successor() {
        let ch = "presence-test-a";
        assert_eq!(link(ch), None);
        relay_peers(ch, 1);
        assert_eq!(link(ch), Some(Link::Relay));
        attached(ch, Link::Lan, 1);
        attached(ch, Link::Ble, 2); // BLE took over from the LAN socket
        detached(ch, 1); // the old LAN socket closing late
        assert_eq!(link(ch), Some(Link::Ble));
        detached(ch, 2);
        assert_eq!(link(ch), Some(Link::Relay));
        relay_lost(ch);
        assert_eq!(link(ch), None);
        forget(ch);
    }
}
