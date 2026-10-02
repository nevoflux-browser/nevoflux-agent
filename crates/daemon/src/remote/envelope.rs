//! The envelope every frame on an agent (MCP) channel travels in (design §5.1).
//!
//! The channel key seals both directions, and AES-GCM proves a frame is
//! genuine but not that it is *new*: a relay that kept an old `tools/call`
//! could play it again, or hand the head one of its own frames back. Three
//! fields inside the seal close that: which way the frame travels, a counter
//! that only goes up, and a challenge the head draws fresh for every
//! connection. None of it touches the sealing itself, so the key derivation
//! and the frame layout the Muse side already verified stay exactly as they
//! are.

use base64::Engine;
use serde_json::{json, Value};

/// Agent → head.
pub const C2H: &str = "c2h";
/// Head → agent.
pub const H2C: &str = "h2c";
/// Refusals after which a session is dropped rather than argued with.
pub const BAD_FRAME_LIMIT: u32 = 8;

/// A fresh connection challenge: 16 random bytes, base64url without padding.
pub fn new_challenge() -> String {
    let bytes: [u8; 16] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Why a frame was refused. Logged, never sent back: telling the other end
/// which check failed would make the refusal a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Malformed,
    WrongDirection,
    ChallengeMismatch,
    CounterRegression,
}

impl Reject {
    pub fn as_str(self) -> &'static str {
        match self {
            Reject::Malformed => "malformed",
            Reject::WrongDirection => "wrong_direction",
            Reject::ChallengeMismatch => "challenge_mismatch",
            Reject::CounterRegression => "counter_regression",
        }
    }
}

/// Wraps the head's outgoing messages. The first frame of every connection is
/// the challenge itself, so [`OutboundSealer::challenge_frame`] must be called
/// before [`OutboundSealer::wrap`].
pub struct OutboundSealer {
    challenge: String,
    next: u64,
}

impl OutboundSealer {
    pub fn new(challenge: String) -> Self {
        Self { challenge, next: 0 }
    }

    /// The frame that hands the agent this connection's challenge (`n = 0`).
    pub fn challenge_frame(&mut self) -> Value {
        self.frame(Value::Null)
    }

    /// Wrap one JSON-RPC message for the wire.
    pub fn wrap(&mut self, message: Value) -> Value {
        self.frame(message)
    }

    fn frame(&mut self, m: Value) -> Value {
        let n = self.next;
        self.next += 1;
        json!({ "d": H2C, "n": n, "ch": self.challenge, "m": m })
    }
}

/// Checks the agent's incoming frames for one connection.
pub struct InboundVerifier {
    challenge: String,
    last: Option<u64>,
    rejected: u32,
}

impl InboundVerifier {
    pub fn new(challenge: String) -> Self {
        Self {
            challenge,
            last: None,
            rejected: 0,
        }
    }

    /// The JSON-RPC message inside `frame`, if the frame is one this
    /// connection should act on.
    pub fn accept(&mut self, frame: &Value) -> Result<Value, Reject> {
        let outcome = self.check(frame);
        if outcome.is_err() {
            self.rejected += 1;
        }
        outcome
    }

    pub fn rejected(&self) -> u32 {
        self.rejected
    }

    pub fn exhausted(&self) -> bool {
        self.rejected >= BAD_FRAME_LIMIT
    }

    fn check(&mut self, frame: &Value) -> Result<Value, Reject> {
        let obj = frame.as_object().ok_or(Reject::Malformed)?;
        let d = obj
            .get("d")
            .and_then(Value::as_str)
            .ok_or(Reject::Malformed)?;
        let n = obj
            .get("n")
            .and_then(Value::as_u64)
            .ok_or(Reject::Malformed)?;
        let ch = obj
            .get("ch")
            .and_then(Value::as_str)
            .ok_or(Reject::Malformed)?;
        let m = obj.get("m").ok_or(Reject::Malformed)?;
        if d != C2H {
            return Err(Reject::WrongDirection);
        }
        if ch != self.challenge {
            return Err(Reject::ChallengeMismatch);
        }
        if let Some(last) = self.last {
            if n <= last {
                return Err(Reject::CounterRegression);
            }
        }
        if !m.is_object() {
            return Err(Reject::Malformed);
        }
        self.last = Some(n);
        Ok(m.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CH: &str = "AAECAwQFBgcICQoLDA0ODw";

    fn c2h(n: u64, ch: &str) -> serde_json::Value {
        json!({"d": "c2h", "n": n, "ch": ch, "m": {"jsonrpc": "2.0", "id": n, "method": "ping"}})
    }

    #[test]
    fn a_challenge_is_sixteen_random_bytes_in_base64url() {
        let a = new_challenge();
        let b = new_challenge();
        assert_eq!(a.len(), 22, "16 bytes, base64url, no padding");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, b);
    }

    #[test]
    fn the_challenge_frame_comes_first_and_carries_no_message() {
        let mut out = OutboundSealer::new(CH.into());
        let first = out.challenge_frame();
        assert_eq!(first, json!({"d": "h2c", "n": 0, "ch": CH, "m": null}));
        let second = out.wrap(json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        assert_eq!(second["n"], 1);
        assert_eq!(second["d"], "h2c");
        assert_eq!(second["ch"], CH);
        assert_eq!(second["m"]["id"], 1);
    }

    #[test]
    fn an_in_order_frame_is_accepted_and_unwrapped() {
        let mut v = InboundVerifier::new(CH.into());
        let m = v.accept(&c2h(0, CH)).expect("accepted");
        assert_eq!(m["method"], "ping");
        assert!(
            v.accept(&c2h(5, CH)).is_ok(),
            "gaps are allowed, only order matters"
        );
        assert_eq!(v.rejected(), 0);
    }

    #[test]
    fn a_replayed_frame_is_refused() {
        let mut v = InboundVerifier::new(CH.into());
        v.accept(&c2h(0, CH)).unwrap();
        assert_eq!(v.accept(&c2h(0, CH)), Err(Reject::CounterRegression));
        v.accept(&c2h(3, CH)).unwrap();
        assert_eq!(v.accept(&c2h(2, CH)), Err(Reject::CounterRegression));
    }

    #[test]
    fn a_reflected_frame_is_refused() {
        // The relay sending the head's own frame back to it.
        let mut out = OutboundSealer::new(CH.into());
        let mine = out.wrap(json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        let mut v = InboundVerifier::new(CH.into());
        assert_eq!(v.accept(&mine), Err(Reject::WrongDirection));
    }

    #[test]
    fn a_frame_from_an_older_connection_is_refused() {
        let mut v = InboundVerifier::new(CH.into());
        assert_eq!(
            v.accept(&c2h(0, "ZZZZZZZZZZZZZZZZZZZZZZ")),
            Err(Reject::ChallengeMismatch)
        );
    }

    #[test]
    fn malformed_frames_are_refused() {
        let mut v = InboundVerifier::new(CH.into());
        assert_eq!(v.accept(&json!("hello")), Err(Reject::Malformed));
        assert_eq!(
            v.accept(&json!({"d": "c2h", "n": 0, "ch": CH})),
            Err(Reject::Malformed)
        );
        assert_eq!(
            v.accept(&json!({"d": "c2h", "n": 0, "ch": CH, "m": null})),
            Err(Reject::Malformed)
        );
        assert_eq!(
            v.accept(&json!({"d": "c2h", "n": -1, "ch": CH, "m": {}})),
            Err(Reject::Malformed)
        );
    }

    #[test]
    fn eight_refusals_exhaust_the_verifier() {
        let mut v = InboundVerifier::new(CH.into());
        for _ in 0..7 {
            let _ = v.accept(&json!("junk"));
        }
        assert!(!v.exhausted());
        let _ = v.accept(&json!("junk"));
        assert!(v.exhausted());
        assert_eq!(v.rejected(), BAD_FRAME_LIMIT);
    }

    #[test]
    fn reject_reasons_have_stable_names() {
        assert_eq!(Reject::Malformed.as_str(), "malformed");
        assert_eq!(Reject::WrongDirection.as_str(), "wrong_direction");
        assert_eq!(Reject::ChallengeMismatch.as_str(), "challenge_mismatch");
        assert_eq!(Reject::CounterRegression.as_str(), "counter_regression");
    }
}
