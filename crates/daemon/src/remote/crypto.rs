//! End-to-end crypto for a remote-control channel (design S2 / K2).
//!
//! A human-transferable pairing code (from `share::password::generate_password`)
//! plus the `channel_id` derive a 256-bit key via Argon2id — reusing
//! `share::crypto::derive_key` so the daemon and the portal JS share one KDF
//! contract (design C5). Frames are then sealed with AES-256-GCM (an AEAD, so
//! integrity + authenticity come for free). The Durable Object relay only ever
//! sees ciphertext (design K2: it can neither read, forge, nor tamper).
//!
//! Unlike `share::crypto` (specialized to a `ShareBundle` JSON payload), this
//! module seals **arbitrary bytes**, so any protocol frame — chat stream,
//! notify, uplink — can ride the channel.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use rand::Rng;

use crate::error::{DaemonError, Result};
use crate::share::crypto::derive_key;
use crate::share::types::KdfParams;

/// AES-256-GCM nonce length in bytes (96 bits).
const NONCE_LEN: usize = 12;

/// A sealed channel frame: a fresh random nonce plus the AES-256-GCM
/// ciphertext with its 16-byte auth tag appended (aes-gcm convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedFrame {
    /// Per-frame random 96-bit nonce.
    pub nonce: [u8; NONCE_LEN],
    /// Ciphertext with the 16-byte GCM auth tag appended.
    pub ciphertext: Vec<u8>,
}

/// Derive the 256-bit channel key from the pairing code and `channel_id`.
///
/// Contract (design C5): algorithm (Argon2id), `KdfParams`, and salt format
/// (`"{code}|{channel_id}"`, defined in `share::crypto::derive_key`) MUST match
/// the portal JS implementation byte-for-byte. Both sides derive through this
/// single source of truth, so the cross-implementation vector test (Rust ↔
/// portal JS) reduces to "portal reproduces `derive_channel_key`'s output".
pub fn derive_channel_key(pairing_code: &str, channel_id: &str) -> Result<[u8; 32]> {
    derive_key(pairing_code, channel_id, &KdfParams::default())
}

/// Seal `plaintext` for the channel with a fresh random nonce.
pub fn seal_frame(key: &[u8; 32], plaintext: &[u8]) -> Result<SealedFrame> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| DaemonError::InternalError(format!("AES-256-GCM key error: {e}")))?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| DaemonError::InternalError(format!("AES-256-GCM encrypt error: {e}")))?;

    Ok(SealedFrame {
        nonce: nonce_bytes,
        ciphertext,
    })
}

/// Open a sealed channel frame. Fails — without leaking why — on a wrong key
/// or tampered ciphertext; AES-GCM verifies integrity/authenticity here.
pub fn open_frame(key: &[u8; 32], frame: &SealedFrame) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| DaemonError::InternalError(format!("AES-256-GCM key error: {e}")))?;

    let nonce = Nonce::from_slice(&frame.nonce);
    cipher
        .decrypt(nonce, frame.ciphertext.as_ref())
        .map_err(|_| {
            DaemonError::InvalidRequest(
                "channel frame decryption failed (wrong key or tampered)".into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed key for frame tests — avoids the (slow, 64 MiB) Argon2id KDF where
    /// only the AEAD behavior is under test.
    fn test_key() -> [u8; 32] {
        [7u8; 32]
    }

    #[test]
    fn seal_open_roundtrip() {
        let key = test_key();
        let pt = b"hello remote channel \x00\x01\x02 payload";
        let frame = seal_frame(&key, pt).unwrap();
        assert_eq!(open_frame(&key, &frame).unwrap(), pt);
    }

    #[test]
    fn open_with_wrong_key_fails() {
        let frame = seal_frame(&test_key(), b"secret").unwrap();
        let mut wrong = test_key();
        wrong[0] ^= 0xFF;
        assert!(open_frame(&wrong, &frame).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = test_key();
        let mut frame = seal_frame(&key, b"secret payload").unwrap();
        frame.ciphertext[0] ^= 0xFF;
        assert!(open_frame(&key, &frame).is_err());
    }

    #[test]
    fn nonce_is_random_per_seal() {
        let key = test_key();
        let a = seal_frame(&key, b"x").unwrap();
        let b = seal_frame(&key, b"x").unwrap();
        assert_ne!(a.nonce, b.nonce, "nonces must differ per seal");
        assert_ne!(a.ciphertext, b.ciphertext, "ciphertexts must differ");
    }

    #[test]
    fn derive_channel_key_deterministic_and_binds_channel() {
        // Uses default (64 MiB) KdfParams — kept to 3 derivations.
        let code = "X-7Q2K-9ABC-DEF3";
        let k1 = derive_channel_key(code, "chan-abc").unwrap();
        let k2 = derive_channel_key(code, "chan-abc").unwrap();
        assert_eq!(k1, k2, "same (code, channel) → same key");
        let k3 = derive_channel_key(code, "chan-xyz").unwrap();
        assert_ne!(k1, k3, "different channel_id must change the key");
    }

    #[test]
    fn derived_key_opens_its_own_frames() {
        let key = derive_channel_key("A-BCDE-FGHJ-KMNP", "chan-1").unwrap();
        let frame = seal_frame(&key, b"end-to-end").unwrap();
        assert_eq!(open_frame(&key, &frame).unwrap(), b"end-to-end");
    }
}
