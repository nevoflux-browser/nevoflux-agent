//! XChaCha20-Poly1305 seal/open for `.nbrain` plaintext, plus Argon2id
//! key derivation for password mode. Random-key mode is the default.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use zeroize::Zeroize;

use nevoflux_brain::BrainError;

use super::nbrain_format::KdfParams;

/// Default Argon2id parameters for password mode (m=64MB, t=3, p=4).
pub fn default_kdf_params() -> KdfParams {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    KdfParams {
        salt,
        memory_kb: 65536,
        iterations: 3,
        parallelism: 4,
    }
}

/// Derive a 256-bit key from a password + KDF params via Argon2id.
pub fn derive_key(password: &str, params: &KdfParams) -> Result<[u8; 32], BrainError> {
    let argon2 = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(
            params.memory_kb,
            params.iterations,
            params.parallelism,
            Some(32),
        )
        .map_err(|e| BrainError::Backend(format!("argon2 params: {e}")))?,
    );
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), &params.salt, &mut key)
        .map_err(|e| BrainError::Backend(format!("argon2 kdf: {e}")))?;
    Ok(key)
}

/// Generate a fresh random 256-bit content key.
pub fn random_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    key
}

/// Generate a fresh random 24-byte XChaCha nonce.
pub fn random_nonce() -> [u8; 24] {
    let mut n = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

/// Encrypt `plaintext` with `key` + `nonce`. Returns ciphertext (incl. tag).
pub fn encrypt(key: &[u8; 32], nonce: &[u8; 24], plaintext: &[u8]) -> Result<Vec<u8>, BrainError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| BrainError::Backend(format!("xchacha key: {e}")))?;
    cipher
        .encrypt(XNonce::from_slice(nonce), plaintext)
        .map_err(|_| BrainError::Backend("xchacha encrypt".into()))
}

/// Decrypt `ciphertext` with `key` + `nonce`. Auth failure → `DecryptFailed`.
pub fn decrypt(key: &[u8; 32], nonce: &[u8; 24], ciphertext: &[u8]) -> Result<Vec<u8>, BrainError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| BrainError::Backend(format!("xchacha key: {e}")))?;
    cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| BrainError::DecryptFailed)
}

/// Zeroize a key array in place (call after use).
pub fn wipe(key: &mut [u8; 32]) {
    key.zeroize();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = random_key();
        let nonce = random_nonce();
        let ct = encrypt(&key, &nonce, b"hello brain").unwrap();
        let pt = decrypt(&key, &nonce, &ct).unwrap();
        assert_eq!(pt, b"hello brain");
    }

    #[test]
    fn wrong_key_fails() {
        let nonce = random_nonce();
        let ct = encrypt(&random_key(), &nonce, b"secret").unwrap();
        let err = decrypt(&random_key(), &nonce, &ct).unwrap_err();
        assert!(matches!(err, BrainError::DecryptFailed));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = random_key();
        let nonce = random_nonce();
        let mut ct = encrypt(&key, &nonce, b"secret").unwrap();
        ct[0] ^= 0xFF;
        assert!(matches!(
            decrypt(&key, &nonce, &ct).unwrap_err(),
            BrainError::DecryptFailed
        ));
    }

    #[test]
    fn password_derive_is_deterministic() {
        let params = KdfParams {
            salt: [1u8; 16],
            memory_kb: 1024,
            iterations: 1,
            parallelism: 1,
        };
        let k1 = derive_key("pw", &params).unwrap();
        let k2 = derive_key("pw", &params).unwrap();
        assert_eq!(k1, k2);
        assert_ne!(derive_key("other", &params).unwrap(), k1);
    }

    #[test]
    fn password_roundtrip() {
        let params = KdfParams {
            salt: [2u8; 16],
            memory_kb: 1024,
            iterations: 1,
            parallelism: 1,
        };
        let key = derive_key("correct horse", &params).unwrap();
        let nonce = random_nonce();
        let ct = encrypt(&key, &nonce, b"diceware").unwrap();
        let wrong = derive_key("wrong horse", &params).unwrap();
        assert!(decrypt(&wrong, &nonce, &ct).is_err());
        assert_eq!(decrypt(&key, &nonce, &ct).unwrap(), b"diceware");
    }
}
