//! Message chunking for Native Messaging.
//!
//! Firefox caps a single native message at ~1 MB (the daemon enforces the same
//! in [`crate::native_messaging::MAX_MESSAGE_SIZE`]). To move larger payloads —
//! e.g. a >1 MB Canvas artifact in a `content_store.load` response, or a large
//! `content_store.set` write coming the other way — both sides split the
//! message into base64 chunk envelopes and reassemble them.
//!
//! This is the daemon/proxy half of the protocol implemented in the extension's
//! `background.js` (`chunkMessage` / `ChunkReassembler`). The wire format MUST
//! stay byte-compatible with that implementation:
//!
//! ```text
//! {"__chunk": {"id": <str>, "index": <u32>, "total": <u32>, "data": <str>}}
//! ```
//!
//! `data` parts, concatenated in `index` order, form the standard-base64
//! encoding of the UTF-8 JSON bytes of the original message.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use serde_json::{json, Value};
use tracing::warn;

/// Serialized-size threshold (bytes) above which an outgoing message is split.
/// Matches background.js `CHUNK_CONFIG.maxMessageSize` (900 KB, leaving
/// headroom under the 1 MB native-messaging frame limit).
const CHUNK_THRESHOLD: usize = 900_000;

/// Base64 characters per chunk. Matches background.js `CHUNK_CONFIG.chunkSize`.
/// A multiple of 4 (whole base64 quartets); leaves room for the envelope
/// overhead under the 1 MB frame limit.
const CHUNK_SIZE: usize = 800_000;

/// Drop partially-received chunk groups older than this. Mirrors the 30 s
/// reassembly timeout on the extension side, with margin.
const PENDING_TTL: Duration = Duration::from_secs(60);

/// Process-unique counter for outgoing chunk-group ids. The extension keys
/// reassembly by id string; `d{n}` never collides with extension-generated
/// ids (which look like `{timestamp}-{random}`).
static CHUNK_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns the frames to actually write for `value`: a single-element vec (the
/// value itself) when it fits in one native message, or a list of `__chunk`
/// envelopes the extension's `ChunkReassembler` will rejoin.
pub fn split_for_send(value: &Value) -> Vec<Value> {
    let json = match serde_json::to_vec(value) {
        Ok(j) => j,
        // If it does not even serialize, let the caller's write surface the error.
        Err(_) => return vec![value.clone()],
    };
    if json.len() <= CHUNK_THRESHOLD {
        return vec![value.clone()];
    }

    let b64 = BASE64_STANDARD.encode(&json);
    let id = format!("d{}", CHUNK_SEQ.fetch_add(1, Ordering::Relaxed));
    let total = b64.len().div_ceil(CHUNK_SIZE);
    let mut frames = Vec::with_capacity(total);
    for index in 0..total {
        let start = index * CHUNK_SIZE;
        let end = ((index + 1) * CHUNK_SIZE).min(b64.len());
        // base64 output is ASCII, so byte-index slicing is always char-aligned.
        let data = &b64[start..end];
        frames.push(json!({
            "__chunk": { "id": id, "index": index, "total": total, "data": data }
        }));
    }
    frames
}

struct Pending {
    total: usize,
    parts: HashMap<usize, String>,
    created: Instant,
}

/// Reassembles inbound `__chunk` envelopes from the extension. Stateful: feed
/// every inbound message through [`ChunkReassembler::process`].
#[derive(Default)]
pub struct ChunkReassembler {
    pending: HashMap<String, Pending>,
}

impl ChunkReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one inbound message. Returns:
    /// - `Some(value)` for a non-chunk message (pass it through unchanged), or
    ///   for a fully reassembled message.
    /// - `None` while still buffering chunks of a group, or when an envelope is
    ///   malformed / fails to decode (dropped, with a warning).
    pub fn process(&mut self, value: Value) -> Option<Value> {
        let chunk = match value.get("__chunk") {
            Some(c) if c.is_object() => c,
            _ => return Some(value),
        };

        let id = chunk.get("id").and_then(Value::as_str).map(str::to_owned);
        let index = chunk.get("index").and_then(Value::as_u64);
        let total = chunk.get("total").and_then(Value::as_u64);
        let data = chunk.get("data").and_then(Value::as_str).map(str::to_owned);
        let (id, index, total, data) = match (id, index, total, data) {
            (Some(id), Some(index), Some(total), Some(data)) if total > 0 => {
                (id, index as usize, total as usize, data)
            }
            _ => {
                warn!("dropping malformed __chunk envelope");
                return None;
            }
        };
        if index >= total {
            warn!("dropping __chunk with index {} >= total {}", index, total);
            return None;
        }

        self.evict_stale();

        let entry = self.pending.entry(id.clone()).or_insert_with(|| Pending {
            total,
            parts: HashMap::new(),
            created: Instant::now(),
        });
        // Tolerate a `total` that changes mid-group by trusting the latest claim.
        entry.total = total;
        entry.parts.insert(index, data);
        if entry.parts.len() < entry.total {
            return None;
        }

        // Group complete — concatenate parts in order, decode, parse.
        let entry = self.pending.remove(&id).expect("entry just inserted");
        let mut b64 = String::with_capacity(entry.total * CHUNK_SIZE);
        for i in 0..entry.total {
            match entry.parts.get(&i) {
                Some(part) => b64.push_str(part),
                None => {
                    warn!("__chunk group {} missing index {}, dropping", id, i);
                    return None;
                }
            }
        }
        match BASE64_STANDARD.decode(b64.as_bytes()) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!("reassembled __chunk message is not valid JSON: {e}");
                    None
                }
            },
            Err(e) => {
                warn!("failed to base64-decode reassembled __chunk message: {e}");
                None
            }
        }
    }

    fn evict_stale(&mut self) {
        let now = Instant::now();
        self.pending
            .retain(|_, p| now.duration_since(p.created) < PENDING_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big_value(bytes: usize) -> Value {
        json!({
            "type": "system_response",
            "payload": { "data": { "entries": [ { "key": "canvas:x", "value": "z".repeat(bytes) } ] } }
        })
    }

    #[test]
    fn small_message_is_not_chunked() {
        let v = json!({"type": "ping"});
        let frames = split_for_send(&v);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], v);
        assert!(frames[0].get("__chunk").is_none());
    }

    #[test]
    fn large_message_splits_into_chunks() {
        let v = big_value(2_000_000);
        let frames = split_for_send(&v);
        assert!(frames.len() >= 2, "expected multiple chunks");
        for f in &frames {
            let c = f.get("__chunk").expect("chunk envelope");
            assert!(c.get("id").is_some());
            assert!(c.get("data").and_then(Value::as_str).unwrap().len() <= CHUNK_SIZE);
        }
    }

    #[test]
    fn round_trip_reassembles_to_original() {
        let original = big_value(2_000_000);
        let frames = split_for_send(&original);
        let mut r = ChunkReassembler::new();
        let mut out = None;
        for f in frames {
            if let Some(v) = r.process(f) {
                out = Some(v);
            }
        }
        assert_eq!(out.expect("reassembled"), original);
    }

    #[test]
    fn out_of_order_chunks_reassemble() {
        let original = big_value(2_000_000);
        let mut frames = split_for_send(&original);
        frames.reverse();
        let mut r = ChunkReassembler::new();
        let mut out = None;
        for f in frames {
            if let Some(v) = r.process(f) {
                out = Some(v);
            }
        }
        assert_eq!(out.expect("reassembled"), original);
    }

    #[test]
    fn non_chunk_passes_through() {
        let mut r = ChunkReassembler::new();
        let v = json!({"type": "chat", "payload": {"text": "hi"}});
        assert_eq!(r.process(v.clone()), Some(v));
    }

    #[test]
    fn malformed_chunk_is_dropped() {
        let mut r = ChunkReassembler::new();
        let bad = json!({"__chunk": {"id": "x", "index": 0}}); // missing total/data
        assert_eq!(r.process(bad), None);
    }
}
