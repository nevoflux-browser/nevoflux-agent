//! `.nbrain` encrypted knowledge-base sharing — local core (M5-A).
//!
//! Pure logic only (no gbrain coupling): binary envelope, crypto, manifest,
//! strip pipeline, tar+zstd packing, and the `seal`/`open` orchestration.

pub mod crypto;
pub mod http_client;
pub mod manifest;
pub mod nbrain_format;
pub mod pack;
pub mod strip;

pub use http_client::{
    BrainRenewResponse, BrainShareHttpClient, BrainUploadResponse, DEFAULT_BASE_URL,
};

use nevoflux_brain::{BrainError, Unlock};

use manifest::{FileEntry, Manifest};
use pack::Entry;

/// How to lock a bundle when sealing.
pub enum SealMode {
    /// Random 256-bit key (zero-knowledge). Key returned to caller.
    RandomKey,
    /// Password mode — Argon2id-derived key; params embedded in header.
    Password(String),
}

/// Build the `.nbrain` artifact from a manifest + file entries.
///
/// Returns the artifact bytes and, in `RandomKey` mode, the content key.
pub fn seal(
    manifest: &Manifest,
    files: &[Entry],
    mode: SealMode,
) -> Result<(Vec<u8>, Option<[u8; 32]>), BrainError> {
    // Plaintext = tar(manifest.json + files/...) then zstd.
    let mut entries = Vec::with_capacity(files.len() + 1);
    entries.push(Entry {
        path: "manifest.json".into(),
        bytes: manifest.to_json()?,
    });
    for f in files {
        entries.push(Entry {
            path: format!("files/{}", f.path),
            bytes: f.bytes.clone(),
        });
    }
    let plaintext = pack::pack(&entries)?;

    let nonce = crypto::random_nonce();
    let (mut key, kdf, returned) = match mode {
        SealMode::RandomKey => {
            let k = crypto::random_key();
            (k, None, Some(k))
        }
        SealMode::Password(pw) => {
            let params = crypto::default_kdf_params();
            let k = crypto::derive_key(&pw, &params)?;
            (k, Some(params), None)
        }
    };

    let ciphertext = crypto::encrypt(&key, &nonce, &plaintext)?;
    crypto::wipe(&mut key);

    let header = nbrain_format::Header { nonce, kdf };
    Ok((nbrain_format::encode(&header, &ciphertext), returned))
}

/// Open a `.nbrain` artifact: decrypt, decompress, verify each file's
/// sha256 against the manifest, and return `(manifest, files)`.
pub fn open(artifact: &[u8], unlock: &Unlock) -> Result<(Manifest, Vec<Entry>), BrainError> {
    let (header, ciphertext) = nbrain_format::decode(artifact)?;

    let mut key = match unlock {
        Unlock::Key(k) => *k,
        Unlock::Password(pw) => {
            let params = header.kdf.clone().ok_or_else(|| {
                BrainError::UnsupportedFormat("password given but no kdf params".into())
            })?;
            crypto::derive_key(pw, &params)?
        }
    };
    let plaintext = crypto::decrypt(&key, &header.nonce, ciphertext);
    crypto::wipe(&mut key);
    let plaintext = plaintext?;

    let mut all = pack::unpack(&plaintext)?;
    let mpos = all
        .iter()
        .position(|e| e.path == "manifest.json")
        .ok_or_else(|| BrainError::MalformedArchive("missing manifest.json".into()))?;
    let manifest = Manifest::from_json(&all.remove(mpos).bytes)?;

    // Strip the `files/` prefix and verify sha256 against the manifest.
    let mut files = Vec::with_capacity(all.len());
    for e in all {
        let rel = e.path.strip_prefix("files/").unwrap_or(&e.path).to_string();
        verify_sha256(&manifest, &rel, &e.bytes)?;
        files.push(Entry {
            path: rel,
            bytes: e.bytes,
        });
    }
    Ok((manifest, files))
}

fn verify_sha256(manifest: &Manifest, path: &str, bytes: &[u8]) -> Result<(), BrainError> {
    use sha2::{Digest, Sha256};
    let want = manifest
        .files
        .iter()
        .find(|f| f.path == path)
        .ok_or_else(|| BrainError::IntegrityMismatch(format!("{path} not in manifest")))?;
    let got = format!("{:x}", Sha256::digest(bytes));
    if got != want.sha256 {
        return Err(BrainError::IntegrityMismatch(path.to_string()));
    }
    Ok(())
}

/// Compute the manifest `FileEntry` for a file (sha256 + byte length).
pub fn file_entry(path: &str, bytes: &[u8]) -> FileEntry {
    use sha2::{Digest, Sha256};
    FileEntry {
        path: path.to_string(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
        bytes: bytes.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::manifest::{Manifest, Sender, StripRulesApplied, FORMAT};
    use super::pack::Entry;
    use super::*;
    use nevoflux_brain::Unlock;

    fn manifest_for(files: &[Entry]) -> Manifest {
        Manifest {
            format: FORMAT.into(),
            created_at: "2026-05-30T00:00:00Z".into(),
            sender: Sender {
                fingerprint: None,
                display_name: "Alice".into(),
                signature: None,
            },
            files: files
                .iter()
                .map(|e| file_entry(&e.path, &e.bytes))
                .collect(),
            strip_rules_applied: StripRulesApplied {
                compiled_only: true,
                frontmatter_whitelist: vec![],
                frontmatter_redacted: vec![],
                raw_excluded: true,
                directories_excluded: vec![".raw".into()],
            },
            title: "t".into(),
            description: "d".into(),
            tags: vec![],
            expires_at: None,
        }
    }

    #[test]
    fn seal_open_random_key_roundtrip() {
        let files = vec![Entry {
            path: "concepts/yc.md".into(),
            bytes: b"# YC".to_vec(),
        }];
        let m = manifest_for(&files);
        let (artifact, key) = seal(&m, &files, SealMode::RandomKey).unwrap();
        let key = key.expect("random-key mode returns a key");

        let (m2, f2) = open(&artifact, &Unlock::Key(key)).unwrap();
        assert_eq!(m2.title, "t");
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].path, "concepts/yc.md");
        assert_eq!(f2[0].bytes, b"# YC");
    }

    #[test]
    fn seal_open_password_roundtrip() {
        let files = vec![Entry {
            path: "a.md".into(),
            bytes: b"x".to_vec(),
        }];
        let m = manifest_for(&files);
        let (artifact, key) =
            seal(&m, &files, SealMode::Password("hunter2hunter2".into())).unwrap();
        assert!(key.is_none(), "password mode returns no key");

        assert!(open(&artifact, &Unlock::Password("wrong".into())).is_err());
        let (_m, f2) = open(&artifact, &Unlock::Password("hunter2hunter2".into())).unwrap();
        assert_eq!(f2[0].bytes, b"x");
    }

    #[test]
    fn tampered_file_fails_integrity() {
        let files = vec![Entry {
            path: "a.md".into(),
            bytes: b"orig".to_vec(),
        }];
        // Manifest records sha256 of "orig" but we seal "tampered".
        let m = manifest_for(&files);
        let tampered = vec![Entry {
            path: "a.md".into(),
            bytes: b"tampered".to_vec(),
        }];
        let (artifact, key) = seal(&m, &tampered, SealMode::RandomKey).unwrap();
        let err = open(&artifact, &Unlock::Key(key.unwrap())).unwrap_err();
        assert!(matches!(
            err,
            nevoflux_brain::BrainError::IntegrityMismatch(_)
        ));
    }
}
