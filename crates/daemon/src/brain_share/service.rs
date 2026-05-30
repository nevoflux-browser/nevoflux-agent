//! BrainShareService — orchestrates the `.nbrain` online share lifecycle.
//!
//! Composes Spec A's `export_snapshot` / `import_snapshot` (on the live
//! [`BrainEngine`]) with the brain-share CF Worker transport and a local
//! encrypted credential store (`brain_shares`). Zero-knowledge: the content
//! key is generated locally, carried only in the share URL `#fragment`, and
//! stored locally encrypted — never sent to the Worker.

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use nevoflux_brain::{
    BrainEngine, ImportOpts, ImportReport, ImportTrust, NbrainBundle, Selection, StripRules, Unlock,
};
use nevoflux_storage::{Storage, StorageError};

use super::http_client::BrainShareHttpClient;
use crate::error::{DaemonError, Result};
use crate::share::local_store::{decrypt_bytes_from_storage, encrypt_bytes_for_storage};
use crate::share::owner_token::{generate_owner_token, hash_owner_token};
use crate::share::share_id::{crockford_decode, crockford_encode, generate_share_id};

/// Default TTL: 30 days (spec decision #17).
pub const DEFAULT_TTL_SECS: u64 = 30 * 24 * 3600;

/// Result of `create`. The key is embedded in `share_url`'s fragment.
#[derive(Debug, Clone)]
pub struct BrainShareResult {
    pub share_id: String,
    /// Full share URL including `#<base32-key>` fragment.
    pub share_url: String,
    pub expires_at: i64,
    pub size_bytes: u64,
}

/// Info row returned by `list`.
#[derive(Debug, Clone)]
pub struct BrainShareInfo {
    pub share_id: String,
    pub share_url: String,
    pub title: String,
    pub expires_at: i64,
    pub size_bytes: u64,
    pub created_at: i64,
}

fn parse_iso8601_to_unix(s: &str) -> i64 {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// Encode a 32-byte content key for the URL fragment (Crockford base32).
fn encode_key(key: &[u8; 32]) -> String {
    crockford_encode(key).to_ascii_lowercase()
}

/// Decode a Crockford base32 key fragment back to 32 bytes.
fn decode_key(fragment: &str) -> Result<[u8; 32]> {
    let bytes = crockford_decode(fragment)
        .ok_or_else(|| DaemonError::InvalidRequest("Invalid key fragment encoding".into()))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| DaemonError::InvalidRequest("Key fragment is not 32 bytes".into()))?;
    Ok(arr)
}

/// Split a share URL into `(share_id, key)`. Accepts `.../b/<id>#<key>`.
pub fn parse_share_url(url: &str) -> Result<(String, [u8; 32])> {
    let (base, fragment) = url
        .split_once('#')
        .ok_or_else(|| DaemonError::InvalidRequest("Share URL missing #key fragment".into()))?;
    let share_id = base
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DaemonError::InvalidRequest("Share URL missing share id".into()))?
        .to_string();
    let key = decode_key(fragment)?;
    Ok((share_id, key))
}

/// Orchestrates the brain-share lifecycle.
pub struct BrainShareService {
    storage: Arc<Storage>,
    http: BrainShareHttpClient,
    master_key: [u8; 32],
}

impl BrainShareService {
    pub fn new(storage: Arc<Storage>, http: BrainShareHttpClient, master_key: [u8; 32]) -> Self {
        Self {
            storage,
            http,
            master_key,
        }
    }

    /// Export a selection, encrypt, upload, and record locally. Returns the
    /// share URL with the key in its `#fragment`.
    pub async fn create(
        &self,
        engine: &Arc<dyn BrainEngine>,
        sel: Selection,
        rules: StripRules,
        title: &str,
        ttl_secs: Option<u64>,
    ) -> Result<BrainShareResult> {
        let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS);

        // 1. Export -> NbrainBundle { artifact, key } (random-key mode).
        let bundle = engine
            .export_snapshot(sel, rules)
            .await
            .map_err(|e| DaemonError::InternalError(format!("export_snapshot: {e}")))?;
        let key = bundle
            .key
            .ok_or_else(|| DaemonError::InternalError("export produced no content key".into()))?;

        // 2. Credentials.
        let share_id = generate_share_id();
        let owner_token = generate_owner_token();
        let owner_token_hash = hash_owner_token(&share_id, &owner_token);

        // 3. Upload opaque ciphertext.
        let resp = self
            .http
            .upload(&share_id, &bundle.artifact, &owner_token_hash, ttl)
            .await?;
        let expires_at = parse_iso8601_to_unix(&resp.expires_at);

        // 4. Build the share URL with the key fragment (zero-knowledge).
        let share_url = format!("{}#{}", resp.url, encode_key(&key));

        // 5. Persist locally, encrypted at rest.
        let enc_token = encrypt_bytes_for_storage(&owner_token, &self.master_key)?;
        let enc_key = encrypt_bytes_for_storage(&key, &self.master_key)?;
        let now = Utc::now().timestamp();
        let (share_id_db, url_db, title_db) =
            (resp.share_id.clone(), share_url.clone(), title.to_string());
        let size = resp.size_bytes;
        self.storage.database().with_connection(|conn| {
            conn.execute(
                "INSERT INTO brain_shares (share_id, share_url, encrypted_owner_token, encrypted_key, title, size_bytes, expires_at, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![share_id_db, url_db, enc_token, enc_key, title_db, size as i64, expires_at, now],
            )?;
            Ok(())
        })?;

        Ok(BrainShareResult {
            share_id: resp.share_id,
            share_url,
            expires_at,
            size_bytes: resp.size_bytes,
        })
    }

    /// Import from a share URL (`.../b/<id>#<key>`): fetch ciphertext + import.
    pub async fn import_url(
        &self,
        engine: &Arc<dyn BrainEngine>,
        url: &str,
        source_name: &str,
        trust: ImportTrust,
    ) -> Result<ImportReport> {
        let (share_id, key) = parse_share_url(url)?;
        let artifact = self.http.fetch_bundle(&share_id).await?;
        let bundle = NbrainBundle {
            artifact,
            key: None,
        };
        let opts = ImportOpts {
            source_name: source_name.to_string(),
            trust,
            unlock: Unlock::Key(key),
        };
        engine
            .import_snapshot(bundle, opts)
            .await
            .map_err(|e| DaemonError::InternalError(format!("import_snapshot: {e}")))
    }

    /// List the user's locally-recorded brain shares, newest first.
    pub fn list(&self) -> Result<Vec<BrainShareInfo>> {
        let rows = self.storage.database().with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT share_id, share_url, title, expires_at, size_bytes, created_at \
                 FROM brain_shares ORDER BY created_at DESC",
            )?;
            let iter = stmt.query_map([], |row| {
                Ok(BrainShareInfo {
                    share_id: row.get(0)?,
                    share_url: row.get(1)?,
                    title: row.get(2)?,
                    expires_at: row.get(3)?,
                    size_bytes: row.get::<_, i64>(4)? as u64,
                    created_at: row.get(5)?,
                })
            })?;
            let mut out = Vec::new();
            for r in iter {
                out.push(r?);
            }
            Ok(out)
        })?;
        Ok(rows)
    }

    /// Renew a share's TTL. Returns the new expiry (Unix).
    pub async fn renew(&self, share_id: &str, extend_secs: u64) -> Result<i64> {
        let enc_token = self.load_encrypted_owner_token(share_id)?;
        let owner_token = decrypt_bytes_from_storage(&enc_token, &self.master_key)?;
        let owner_token_b64 = URL_SAFE_NO_PAD.encode(&owner_token);
        let resp = self
            .http
            .renew(share_id, &owner_token_b64, extend_secs)
            .await?;
        let new_expires = parse_iso8601_to_unix(&resp.expires_at);
        let id = share_id.to_string();
        self.storage.database().with_connection(|conn| {
            conn.execute(
                "UPDATE brain_shares SET expires_at = ?1 WHERE share_id = ?2",
                rusqlite::params![new_expires, id],
            )?;
            Ok(())
        })?;
        Ok(new_expires)
    }

    /// Revoke a share server-side and move the local row to history.
    pub async fn revoke(&self, share_id: &str) -> Result<()> {
        let enc_token = self.load_encrypted_owner_token(share_id)?;
        let owner_token = decrypt_bytes_from_storage(&enc_token, &self.master_key)?;
        let owner_token_b64 = URL_SAFE_NO_PAD.encode(&owner_token);
        self.http.revoke(share_id, &owner_token_b64).await?;
        let id = share_id.to_string();
        self.storage.database().with_connection(|conn| {
            conn.execute(
                "INSERT INTO brain_share_history (share_id, reason) VALUES (?1, 'revoked')",
                rusqlite::params![id],
            )?;
            conn.execute(
                "DELETE FROM brain_shares WHERE share_id = ?1",
                rusqlite::params![id],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    fn load_encrypted_owner_token(&self, share_id: &str) -> Result<String> {
        let id = share_id.to_string();
        self.storage
            .database()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT encrypted_owner_token FROM brain_shares WHERE share_id = ?1",
                    rusqlite::params![id],
                    |row| row.get::<_, String>(0),
                )
                .map_err(StorageError::from)
            })
            .map_err(|e| match e {
                StorageError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                    DaemonError::InvalidRequest(format!("Share not found: {share_id}"))
                }
                other => DaemonError::from(other),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    fn make_service() -> BrainShareService {
        let storage = Arc::new(Storage::open_in_memory().expect("open storage"));
        let http = BrainShareHttpClient::new("https://example.test").expect("http client");
        BrainShareService::new(storage, http, test_key())
    }

    #[test]
    fn key_fragment_roundtrip() {
        let key = test_key();
        let frag = encode_key(&key);
        assert_eq!(decode_key(&frag).unwrap(), key);
    }

    #[test]
    fn parse_share_url_splits_id_and_key() {
        let key = test_key();
        let url = format!(
            "https://share.nevoflux.app/b/abc123xyz0#{}",
            encode_key(&key)
        );
        let (id, k) = parse_share_url(&url).unwrap();
        assert_eq!(id, "abc123xyz0");
        assert_eq!(k, key);
    }

    #[test]
    fn parse_share_url_rejects_missing_fragment() {
        let err = parse_share_url("https://share.nevoflux.app/b/abc123xyz0").unwrap_err();
        assert!(matches!(err, DaemonError::InvalidRequest(_)));
    }

    #[test]
    fn parse_share_url_rejects_bad_key_length() {
        // "00" decodes to 1 byte, not 32.
        let err = parse_share_url("https://x/b/abc123xyz0#00").unwrap_err();
        assert!(matches!(err, DaemonError::InvalidRequest(_)));
    }

    #[test]
    fn list_empty_returns_empty_vec() {
        let svc = make_service();
        assert!(svc.list().expect("list").is_empty());
    }

    #[test]
    fn load_owner_token_missing_returns_error() {
        let svc = make_service();
        let err = svc.load_encrypted_owner_token("nope1234560").unwrap_err();
        match err {
            DaemonError::InvalidRequest(m) => assert!(m.contains("Share not found")),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }
}
