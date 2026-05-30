//! `NBRN` binary envelope: plaintext header + ciphertext.

use nevoflux_brain::BrainError;

/// 4-byte magic identifying a `.nbrain` artifact.
pub const MAGIC: &[u8; 4] = b"NBRN";
/// Current envelope version.
pub const VERSION: u8 = 1;

/// Argon2id parameters carried in the header (password mode only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KdfParams {
    pub salt: [u8; 16],
    pub memory_kb: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

/// Parsed `.nbrain` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub nonce: [u8; 24],
    pub kdf: Option<KdfParams>,
}

/// Assemble the full artifact bytes: header + ciphertext.
pub fn encode(header: &Header, ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&header.nonce);
    match &header.kdf {
        None => out.push(0u8),
        Some(p) => {
            out.push(1u8);
            out.extend_from_slice(&p.salt);
            out.extend_from_slice(&p.memory_kb.to_le_bytes());
            out.extend_from_slice(&p.iterations.to_le_bytes());
            out.extend_from_slice(&p.parallelism.to_le_bytes());
        }
    }
    out.extend_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    out.extend_from_slice(ciphertext);
    out
}

/// Parse an artifact: returns the header and a borrowed ciphertext slice.
pub fn decode(bytes: &[u8]) -> Result<(Header, &[u8]), BrainError> {
    let mut cur = 0usize;
    let need = |cur: usize, n: usize| -> Result<(), BrainError> {
        if cur + n > bytes.len() {
            Err(BrainError::UnsupportedFormat("truncated header".into()))
        } else {
            Ok(())
        }
    };
    need(cur, 5)?;
    if &bytes[0..4] != MAGIC {
        return Err(BrainError::UnsupportedFormat("bad magic".into()));
    }
    if bytes[4] != VERSION {
        return Err(BrainError::UnsupportedFormat(format!(
            "version {} unsupported",
            bytes[4]
        )));
    }
    cur = 5;
    need(cur, 24)?;
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&bytes[cur..cur + 24]);
    cur += 24;
    need(cur, 1)?;
    let has_kdf = bytes[cur];
    cur += 1;
    let kdf = if has_kdf == 1 {
        need(cur, 16 + 12)?;
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&bytes[cur..cur + 16]);
        cur += 16;
        let memory_kb = u32::from_le_bytes(bytes[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let iterations = u32::from_le_bytes(bytes[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let parallelism = u32::from_le_bytes(bytes[cur..cur + 4].try_into().unwrap());
        cur += 4;
        Some(KdfParams {
            salt,
            memory_kb,
            iterations,
            parallelism,
        })
    } else {
        None
    };
    need(cur, 8)?;
    let ct_len = u64::from_le_bytes(bytes[cur..cur + 8].try_into().unwrap()) as usize;
    cur += 8;
    need(cur, ct_len)?;
    Ok((Header { nonce, kdf }, &bytes[cur..cur + ct_len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_random_key_mode() {
        let header = Header {
            nonce: [7u8; 24],
            kdf: None,
        };
        let ct = b"ciphertext-bytes".to_vec();
        let bytes = encode(&header, &ct);
        let (h2, ct2) = decode(&bytes).unwrap();
        assert_eq!(h2, header);
        assert_eq!(ct2, &ct[..]);
    }

    #[test]
    fn roundtrip_password_mode() {
        let header = Header {
            nonce: [3u8; 24],
            kdf: Some(KdfParams {
                salt: [9u8; 16],
                memory_kb: 65536,
                iterations: 3,
                parallelism: 4,
            }),
        };
        let bytes = encode(&header, b"abc");
        let (h2, ct2) = decode(&bytes).unwrap();
        assert_eq!(h2, header);
        assert_eq!(ct2, b"abc");
    }

    #[test]
    fn bad_magic_rejected() {
        let err = decode(b"XXXX\x01").unwrap_err();
        assert!(matches!(err, BrainError::UnsupportedFormat(_)));
    }

    #[test]
    fn truncated_rejected() {
        let header = Header {
            nonce: [0u8; 24],
            kdf: None,
        };
        let mut bytes = encode(&header, b"hello");
        bytes.truncate(bytes.len() - 2);
        assert!(matches!(
            decode(&bytes).unwrap_err(),
            BrainError::UnsupportedFormat(_)
        ));
    }
}
