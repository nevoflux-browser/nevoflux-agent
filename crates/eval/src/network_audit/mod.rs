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

/// Parse a pcap byte slice into outbound host names.
///
/// Phase 3d implementation: confirms the bytes are a well-formed pcap file
/// (libpcap or pcapng magic), returns empty until Phase 4 wires in
/// per-packet DNS/IP extraction.
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
    debug!(
        bytes = bytes.len(),
        "pcap recognised (per-packet host extraction deferred to Phase 4)"
    );
    vec![]
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
}
