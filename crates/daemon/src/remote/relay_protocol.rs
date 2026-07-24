//! Portal relay wire protocol + Y2 send-side sequencing (design §Q12).
//!
//! Matches the portal `RelayChatTransport` (`nevoflux-portal`
//! `src/lib/chat/{wire,sequence}.ts`): one WS message carries one
//! [`WireMessage`] (JSON; in the E2E mode the whole message is AES-256-GCM
//! sealed via [`super::crypto`]). The daemon assigns a monotonic `seq` (from 0)
//! to each downlink data frame and retains sent frames so it can honor the
//! portal's `resume{from}` requests; a gap larger than the buffer escalates to
//! `resync` (portal then resets its sequencer to 0).
//!
//! The inner business `frame` is left opaque (`serde_json::Value`) at this
//! layer — the typed `InboundFrame`/`OutboundFrame` schema and their
//! translation to/from `DaemonEnvelope` land with the M2 tap wiring.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The wire envelope, discriminated by `k` (matches portal `wire.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum WireMessage {
    /// A business frame. `seq` is present only on downlink (daemon→portal) data
    /// frames; portal→daemon uplink frames omit it.
    Frame {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        frame: Value,
    },
    /// Uplink-only (portal→daemon): resend from `from` (the portal's expected
    /// next seq).
    Resume { from: u64 },
    /// Downlink-only (daemon→portal): abandon incremental catch-up; the portal
    /// full-reloads the transcript and resets its sequencer to 0.
    Resync,
}

/// Retain at most this many sent frames for resume. A `resume{from}` reaching
/// further back than the buffer holds cannot be honored incrementally and must
/// escalate to a `Resync`.
const SEND_BUFFER_CAP: usize = 512;

/// Assigns monotonic `seq` to downlink frames and retains them for resume
/// (design Y2). Send-side only; the receive-side gap tracker lives in the
/// portal (`sequence.ts`).
#[derive(Debug, Default)]
pub struct SendSequencer {
    next: u64,
    buffer: std::collections::VecDeque<(u64, Value)>,
}

impl SendSequencer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tag a business `frame` with the next seq, retain it, and return the wire
    /// message to send downlink.
    pub fn tag(&mut self, frame: Value) -> WireMessage {
        let seq = self.next;
        self.next += 1;
        self.buffer.push_back((seq, frame.clone()));
        while self.buffer.len() > SEND_BUFFER_CAP {
            self.buffer.pop_front();
        }
        WireMessage::Frame {
            seq: Some(seq),
            frame,
        }
    }

    /// The next seq that will be assigned.
    pub fn next_seq(&self) -> u64 {
        self.next
    }

    /// Resend everything from `from` (inclusive). Returns the wire messages to
    /// send, or `None` when `from` is older than the buffer still holds — the
    /// caller must then send a [`WireMessage::Resync`] and [`reset`](Self::reset).
    pub fn resend_from(&self, from: u64) -> Option<Vec<WireMessage>> {
        if from >= self.next {
            return Some(Vec::new()); // already caught up
        }
        match self.buffer.front().map(|(s, _)| *s) {
            Some(oldest) if from >= oldest => Some(
                self.buffer
                    .iter()
                    .filter(|(s, _)| *s >= from)
                    .map(|(s, f)| WireMessage::Frame {
                        seq: Some(*s),
                        frame: f.clone(),
                    })
                    .collect(),
            ),
            _ => None, // buffer doesn't reach back to `from`
        }
    }

    /// Reset the counter and buffer for a fresh transcript (paired with a
    /// `Resync` and the portal's `resetSequence(0)`).
    pub fn reset(&mut self) {
        self.next = 0;
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(id: &str) -> Value {
        json!({ "kind": "stream_delta", "streamId": id, "delta": "x" })
    }

    #[test]
    fn wire_frame_with_seq_roundtrip() {
        let w = WireMessage::Frame {
            seq: Some(0),
            frame: frame("s1"),
        };
        let s = serde_json::to_string(&w).unwrap();
        assert!(s.contains("\"k\":\"frame\""));
        assert!(s.contains("\"seq\":0"));
        assert_eq!(serde_json::from_str::<WireMessage>(&s).unwrap(), w);
    }

    #[test]
    fn wire_frame_without_seq_omits_field() {
        let w = WireMessage::Frame {
            seq: None,
            frame: json!({ "kind": "user_message", "text": "hi" }),
        };
        let s = serde_json::to_string(&w).unwrap();
        assert!(!s.contains("seq"), "uplink frames omit seq: {s}");
        assert_eq!(serde_json::from_str::<WireMessage>(&s).unwrap(), w);
    }

    #[test]
    fn wire_resume_and_resync_shapes() {
        assert_eq!(
            serde_json::to_string(&WireMessage::Resume { from: 3 }).unwrap(),
            r#"{"k":"resume","from":3}"#
        );
        assert_eq!(
            serde_json::to_string(&WireMessage::Resync).unwrap(),
            r#"{"k":"resync"}"#
        );
        assert_eq!(
            serde_json::from_str::<WireMessage>(r#"{"k":"resync"}"#).unwrap(),
            WireMessage::Resync
        );
    }

    #[test]
    fn sequencer_assigns_monotonic_seq_from_zero() {
        let mut seq = SendSequencer::new();
        for expected in 0..3 {
            match seq.tag(frame("s")) {
                WireMessage::Frame { seq: Some(n), .. } => assert_eq!(n, expected),
                other => panic!("expected Frame with seq, got {other:?}"),
            }
        }
        assert_eq!(seq.next_seq(), 3);
    }

    #[test]
    fn resend_from_returns_buffered_tail() {
        let mut seq = SendSequencer::new();
        for _ in 0..3 {
            seq.tag(frame("s"));
        }
        // resume from 1 → frames 1 and 2.
        let out = seq.resend_from(1).unwrap();
        let seqs: Vec<u64> = out
            .iter()
            .map(|w| match w {
                WireMessage::Frame { seq: Some(n), .. } => *n,
                _ => panic!("expected Frame"),
            })
            .collect();
        assert_eq!(seqs, vec![1, 2]);
        // already caught up → empty.
        assert!(seq.resend_from(3).unwrap().is_empty());
    }

    #[test]
    fn resend_from_too_old_returns_none_for_resync() {
        let mut seq = SendSequencer::new();
        for _ in 0..(SEND_BUFFER_CAP as u64 + 10) {
            seq.tag(frame("s"));
        }
        // seq 0 was evicted → cannot resume from 0.
        assert!(
            seq.resend_from(0).is_none(),
            "gap older than buffer must force resync"
        );
    }

    #[test]
    fn reset_rewinds_to_zero() {
        let mut seq = SendSequencer::new();
        seq.tag(frame("s"));
        seq.tag(frame("s"));
        seq.reset();
        assert_eq!(seq.next_seq(), 0);
        match seq.tag(frame("s")) {
            WireMessage::Frame { seq: Some(0), .. } => {}
            other => panic!("after reset seq restarts at 0, got {other:?}"),
        }
    }
}
