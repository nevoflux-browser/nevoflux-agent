//! Network audit support for Privacy-Audit benchmark.
//!
//! Spawns `tcpdump` per task on Linux only (other platforms skip via
//! `supports_platform`). On task completion, the capture is closed and the
//! pcap file is examined to extract observed outbound host names.
//!
//! Privacy-Audit assertions of type `NoOutboundTo { hosts }` consult the
//! resulting `TaskResult::outbound_hosts` list — if any of `hosts` matches
//! a substring of an observed entry, the assertion fails.
//!
//! ## Phase 3d scope
//!
//! Phase 3d ships the **smoke implementation**: tcpdump is spawned and the
//! pcap file is recognised by magic bytes, but per-packet host extraction
//! is deferred to Phase 4 (requires a heavier pcap-parser dep that isn't
//! available offline yet). For now, `parse_pcap_hosts` confirms the file
//! is a well-formed pcap and returns an empty list — `NoOutboundTo`
//! assertions therefore PASS by default until Phase 4 fills in extraction.
//! The wiring (tcpdump spawn + assertion plumbing + outbound_hosts field)
//! is complete so Phase 4 only needs to swap in real parsing.

use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::{debug, warn};

pub struct TcpdumpRecorder {
    child: Child,
    pcap_path: PathBuf,
}

impl TcpdumpRecorder {
    /// Start tcpdump capturing all non-loopback traffic into `pcap_path`.
    /// Returns an error if tcpdump cannot be spawned (missing binary, no
    /// `cap_net_raw`, …).
    pub async fn spawn(pcap_path: PathBuf) -> std::io::Result<Self> {
        let child = Command::new("tcpdump")
            .args([
                "-i",
                "any",
                "-w",
                &pcap_path.to_string_lossy(),
                "not (host 127.0.0.1 or host ::1)",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        // Give tcpdump 200ms to bind; if it dies in that window, the
        // caller's first finish_and_extract_hosts() call will see an
        // empty / missing pcap and return [].
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        Ok(Self { child, pcap_path })
    }

    /// Stop capture and parse pcap → unique outbound hosts (best-effort).
    pub async fn finish_and_extract_hosts(mut self) -> std::io::Result<Vec<String>> {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        let bytes = match std::fs::read(&self.pcap_path) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, path = ?self.pcap_path, "pcap read failed");
                return Ok(vec![]);
            }
        };
        Ok(parse_pcap_hosts(&bytes))
    }
}

/// Parse a pcap byte slice into outbound host strings.
///
/// Phase 4 implementation: walks libpcap-format records, decodes the
/// link-layer header (Ethernet / Linux cooked v1+v2 / raw), then the IPv4
/// or IPv6 header to extract a destination IP per packet. Returns each
/// unique destination IP as a string. PCAPng files and DNS-domain
/// extraction remain Phase 5 work — for now we only recognise pcapng's
/// magic and return an empty list (caller can still tell tcpdump
/// produced a valid file).
pub fn parse_pcap_hosts(bytes: &[u8]) -> Vec<String> {
    if !is_pcap_file(bytes) {
        if !bytes.is_empty() {
            warn!(
                bytes = bytes.len(),
                "input is not a recognised pcap file; returning empty host list"
            );
        }
        return vec![];
    }
    // pcapng → Phase 5.
    if bytes.starts_with(&[0x0a, 0x0d, 0x0d, 0x0a]) {
        debug!(
            bytes = bytes.len(),
            "pcapng recognised (record walking deferred to Phase 5)"
        );
        return vec![];
    }
    parse_libpcap_records(bytes).unwrap_or_default()
}

/// Walk libpcap-classic records and return unique destination IPs.
/// Returns Err on malformed input so we don't return partial junk; the
/// public entry point converts Err → empty vec.
fn parse_libpcap_records(bytes: &[u8]) -> Result<Vec<String>, &'static str> {
    use std::collections::BTreeSet;

    // libpcap global header is 24 bytes:
    //   u32 magic | u16 version_major | u16 version_minor |
    //   i32 thiszone | u32 sigfigs | u32 snaplen | u32 network (linktype)
    if bytes.len() < 24 {
        return Err("truncated global header");
    }
    // LE-decode the magic. The canonical libpcap magic is `0xa1b2c3d4`
    // (`0xa1b23c4d` for nanosecond resolution); a writer emits it in its
    // native byte order. So:
    //   - LE writer → raw bytes `d4 c3 b2 a1` → LE-decoded = 0xa1b2c3d4
    //   - BE writer → raw bytes `a1 b2 c3 d4` → LE-decoded = 0xd4c3b2a1
    // i.e. canonical-after-LE-decode ⇒ LE writer, byte-swapped ⇒ BE writer.
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let big_endian = match magic {
        0xa1b2c3d4 | 0xa1b23c4d => false, // LE writer (canonical magic in LE order)
        0xd4c3b2a1 | 0x4d3cb2a1 => true,  // BE writer (byte-swapped from LE reader's view)
        _ => return Err("unknown libpcap magic"),
    };
    let read_u32 = |off: usize| -> u32 {
        let b = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
        if big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        }
    };
    let linktype = read_u32(20);

    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut off = 24usize;
    while off + 16 <= bytes.len() {
        // Record header: ts_sec | ts_usec | incl_len | orig_len
        let incl_len = read_u32(off + 8) as usize;
        let data_off = off + 16;
        if data_off + incl_len > bytes.len() {
            break; // truncated final record — stop, don't error.
        }
        let pkt = &bytes[data_off..data_off + incl_len];
        if let Some(ip) = extract_dest_ip(linktype, pkt) {
            out.insert(ip);
        }
        off = data_off + incl_len;
    }
    Ok(out.into_iter().collect())
}

/// Strip link-layer header for the link types we care about, then call
/// `extract_dest_ip_from_l3`. Returns `None` for unknown link types or
/// malformed packets.
fn extract_dest_ip(linktype: u32, pkt: &[u8]) -> Option<String> {
    // Common linktypes:
    //   1   = LINKTYPE_ETHERNET           — 14-byte header
    //   113 = LINKTYPE_LINUX_SLL          — 16-byte cooked v1
    //   276 = LINKTYPE_LINUX_SLL2         — 20-byte cooked v2
    //   12  = LINKTYPE_RAW (legacy)       — 0 bytes, packet starts at L3
    //   101 = LINKTYPE_RAW                — same
    let l3_offset = match linktype {
        1 => 14,
        113 => 16,
        276 => 20,
        12 | 101 => 0,
        _ => return None,
    };
    if pkt.len() < l3_offset {
        return None;
    }
    extract_dest_ip_from_l3(&pkt[l3_offset..])
}

fn extract_dest_ip_from_l3(l3: &[u8]) -> Option<String> {
    if l3.is_empty() {
        return None;
    }
    let version = (l3[0] >> 4) & 0x0f;
    match version {
        4 => {
            if l3.len() < 20 {
                return None;
            }
            Some(format!("{}.{}.{}.{}", l3[16], l3[17], l3[18], l3[19]))
        }
        6 => {
            if l3.len() < 40 {
                return None;
            }
            let mut groups = [0u16; 8];
            for (i, g) in groups.iter_mut().enumerate() {
                let off = 24 + i * 2;
                *g = u16::from_be_bytes([l3[off], l3[off + 1]]);
            }
            Some(
                groups
                    .iter()
                    .map(|g| format!("{:x}", g))
                    .collect::<Vec<_>>()
                    .join(":"),
            )
        }
        _ => None,
    }
}

/// True iff `bytes` begin with a libpcap or pcapng magic-number sequence.
fn is_pcap_file(bytes: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    // libpcap: D4 C3 B2 A1 (LE) or A1 B2 C3 D4 (BE) or nanosecond variants.
    let libpcap_magics: [[u8; 4]; 4] = [
        [0xd4, 0xc3, 0xb2, 0xa1],
        [0xa1, 0xb2, 0xc3, 0xd4],
        [0x4d, 0x3c, 0xb2, 0xa1],
        [0xa1, 0xb2, 0x3c, 0x4d],
    ];
    if libpcap_magics.iter().any(|m| bytes.starts_with(m)) {
        return true;
    }
    // pcapng: section-header-block starts with type 0x0a0d0d0a.
    bytes.starts_with(&[0x0a, 0x0d, 0x0d, 0x0a])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pcap_hosts_empty_input() {
        assert!(parse_pcap_hosts(&[]).is_empty());
    }

    #[test]
    fn parse_pcap_hosts_invalid_input_does_not_panic() {
        let garbage = b"not a pcap file at all";
        let result = parse_pcap_hosts(garbage);
        assert!(result.is_empty());
    }

    #[test]
    fn is_pcap_file_recognises_libpcap_le_magic() {
        let mut bytes = vec![0xd4, 0xc3, 0xb2, 0xa1];
        bytes.extend_from_slice(&[0; 20]);
        assert!(is_pcap_file(&bytes));
    }

    #[test]
    fn is_pcap_file_recognises_pcapng_magic() {
        let mut bytes = vec![0x0a, 0x0d, 0x0d, 0x0a];
        bytes.extend_from_slice(&[0; 20]);
        assert!(is_pcap_file(&bytes));
    }

    #[test]
    fn is_pcap_file_rejects_garbage() {
        assert!(!is_pcap_file(b"not pcap"));
        assert!(!is_pcap_file(&[]));
    }

    fn build_libpcap_with_ipv4_packet(dest: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        // Global header — LE writer, linktype = 1 (Ethernet).
        // Canonical magic 0xa1b2c3d4 written in LE byte order → raw bytes
        // [d4, c3, b2, a1] on disk (matches real tcpdump LE output).
        out.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes()); // magic
        out.extend_from_slice(&2u16.to_le_bytes()); // version major
        out.extend_from_slice(&4u16.to_le_bytes()); // version minor
        out.extend_from_slice(&0i32.to_le_bytes()); // thiszone
        out.extend_from_slice(&0u32.to_le_bytes()); // sigfigs
        out.extend_from_slice(&65535u32.to_le_bytes()); // snaplen
        out.extend_from_slice(&1u32.to_le_bytes()); // linktype = Ethernet

        // One record:
        let mut pkt = Vec::new();
        // Ethernet header (14 bytes): dst MAC, src MAC, ethertype 0x0800 (IPv4)
        pkt.extend_from_slice(&[0; 6]);
        pkt.extend_from_slice(&[0; 6]);
        pkt.extend_from_slice(&[0x08, 0x00]);
        // IPv4 header (20 bytes minimal): version=4, ihl=5 in byte 0.
        let mut ip = vec![0u8; 20];
        ip[0] = (4 << 4) | 5;
        ip[16] = dest[0];
        ip[17] = dest[1];
        ip[18] = dest[2];
        ip[19] = dest[3];
        pkt.extend_from_slice(&ip);

        // Record header (16 bytes): ts_sec, ts_usec, incl_len, orig_len
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        out.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        out.extend_from_slice(&pkt);
        out
    }

    #[test]
    fn parse_pcap_hosts_extracts_ipv4_dest() {
        let pcap = build_libpcap_with_ipv4_packet([10, 20, 30, 40]);
        let hosts = parse_pcap_hosts(&pcap);
        assert_eq!(hosts, vec!["10.20.30.40".to_string()]);
    }

    #[test]
    fn parse_pcap_hosts_dedupes_repeated_dest() {
        // Two records with the same destination → one entry in the output.
        let one = build_libpcap_with_ipv4_packet([1, 2, 3, 4]);
        // Append the same record section (skip the 24-byte global header).
        let mut twice = one.clone();
        twice.extend_from_slice(&one[24..]);
        let hosts = parse_pcap_hosts(&twice);
        assert_eq!(hosts, vec!["1.2.3.4".to_string()]);
    }

    #[test]
    fn parse_pcap_hosts_ignores_unknown_linktype() {
        // Build a libpcap with linktype = 999 (unknown), one byte of "data".
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes()); // canonical LE magic
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&65535u32.to_le_bytes());
        bytes.extend_from_slice(&999u32.to_le_bytes());
        // One record with 1 byte of data
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(0xff);
        let hosts = parse_pcap_hosts(&bytes);
        assert!(hosts.is_empty());
    }

    fn build_libpcap_with_ipv4_packet_be(dest: [u8; 4]) -> Vec<u8> {
        // BE writer: canonical magic emitted in big-endian byte order, all
        // multi-byte header fields likewise BE.  Packet payload bytes
        // (Ethernet + IP) are byte-order-neutral.
        let mut out = Vec::new();
        out.extend_from_slice(&0xa1b2c3d4u32.to_be_bytes()); // magic
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&0i32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&65535u32.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes()); // linktype = Ethernet

        let mut pkt = Vec::new();
        pkt.extend_from_slice(&[0; 6]);
        pkt.extend_from_slice(&[0; 6]);
        pkt.extend_from_slice(&[0x08, 0x00]);
        let mut ip = vec![0u8; 20];
        ip[0] = (4 << 4) | 5;
        ip[16] = dest[0];
        ip[17] = dest[1];
        ip[18] = dest[2];
        ip[19] = dest[3];
        pkt.extend_from_slice(&ip);

        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(pkt.len() as u32).to_be_bytes());
        out.extend_from_slice(&(pkt.len() as u32).to_be_bytes());
        out.extend_from_slice(&pkt);
        out
    }

    #[test]
    fn parse_pcap_hosts_handles_be_writer() {
        let pcap = build_libpcap_with_ipv4_packet_be([5, 6, 7, 8]);
        let hosts = parse_pcap_hosts(&pcap);
        assert_eq!(hosts, vec!["5.6.7.8".to_string()]);
    }

    #[test]
    fn parse_pcap_hosts_extracts_ipv6_dest_from_real_format() {
        // Mirror the Linux tcpdump on `lo` shape: Ethernet linktype, IPv6
        // ethertype, ::1 → ::1 packet. Regression for the inverted-endian
        // bug that returned [] from real-world captures.
        let mut out = Vec::new();
        out.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&65535u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // Ethernet
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&[0; 6]);
        pkt.extend_from_slice(&[0; 6]);
        pkt.extend_from_slice(&[0x86, 0xdd]); // IPv6 ethertype
        let mut ip6 = vec![0u8; 40];
        ip6[0] = 0x60; // version = 6
        // dest IPv6 = ::1 (bytes 24..40 in L3)
        ip6[24..40].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        pkt.extend_from_slice(&ip6);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        out.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        out.extend_from_slice(&pkt);
        let hosts = parse_pcap_hosts(&out);
        assert_eq!(hosts, vec!["0:0:0:0:0:0:0:1".to_string()]);
    }

    #[test]
    fn extract_dest_ip_from_l3_handles_ipv6() {
        let mut l3 = vec![0u8; 40];
        l3[0] = 6 << 4; // version = 6
                        // Dest IP: 2001:db8::1
        let dest_bytes: [u8; 16] = [
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ];
        l3[24..40].copy_from_slice(&dest_bytes);
        assert_eq!(
            extract_dest_ip_from_l3(&l3),
            Some("2001:db8:0:0:0:0:0:1".into())
        );
    }
}
