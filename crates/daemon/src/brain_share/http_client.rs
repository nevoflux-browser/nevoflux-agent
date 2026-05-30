//! HTTP client for the brain-share endpoints on the CF Worker.
//!
//! Mirrors [`crate::share::http_client`] but targets the `/api/brain/*`
//! routes and the `.nbrain` (NBRN) blob. Zero-knowledge: only opaque
//! ciphertext crosses this client; the content key never does.

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};

/// Default base URL for the CF Worker.
pub const DEFAULT_BASE_URL: &str = "https://share.nevoflux.app";

/// HTTP client for the brain-share API.
#[derive(Clone)]
pub struct BrainShareHttpClient {
    base_url: String,
    client: Client,
}

/// Upload response — matches the Worker `BrainUploadResponse`.
#[derive(Debug, Clone, Deserialize)]
pub struct BrainUploadResponse {
    pub share_id: String,
    /// ISO 8601 timestamp string.
    pub expires_at: String,
    pub size_bytes: u64,
    pub url: String,
}

/// Renew response — matches the Worker PATCH response.
#[derive(Debug, Clone, Deserialize)]
pub struct BrainRenewResponse {
    pub share_id: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct RenewRequestBody<'a> {
    owner_token: &'a str,
    extend_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct RevokeRequestBody<'a> {
    owner_token: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct RevokeResponseBody {
    deleted: bool,
}

impl BrainShareHttpClient {
    /// Construct a client at the given base URL.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| DaemonError::InternalError(format!("reqwest build error: {e}")))?;
        Ok(Self {
            base_url: base_url.into(),
            client,
        })
    }

    /// Construct a client at the default Worker URL.
    pub fn with_default_url() -> Result<Self> {
        Self::new(DEFAULT_BASE_URL)
    }

    /// The configured base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Upload an opaque NBRN blob. `owner_token_hash` is hex SHA-256.
    pub async fn upload(
        &self,
        share_id: &str,
        nbrain_bytes: &[u8],
        owner_token_hash: &str,
        ttl_secs: u64,
    ) -> Result<BrainUploadResponse> {
        let url = format!(
            "{}/api/brain/share?share_id={}&owner_token_hash={}&expiry_secs={}",
            self.base_url, share_id, owner_token_hash, ttl_secs,
        );
        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .body(nbrain_bytes.to_vec())
            .send()
            .await
            .map_err(|e| DaemonError::InternalError(format!("Brain upload request failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(DaemonError::InternalError(format!(
                "Brain upload failed: {status} - {body}"
            )));
        }
        response
            .json::<BrainUploadResponse>()
            .await
            .map_err(|e| DaemonError::InternalError(format!("Brain upload parse: {e}")))
    }

    /// Download an NBRN blob by share ID.
    pub async fn fetch_bundle(&self, share_id: &str) -> Result<Vec<u8>> {
        let url = format!("{}/api/brain/share/{}/bundle", self.base_url, share_id);
        let response =
            self.client.get(&url).send().await.map_err(|e| {
                DaemonError::InternalError(format!("Brain fetch request failed: {e}"))
            })?;
        if response.status() == 404 {
            return Err(DaemonError::InvalidRequest(format!(
                "Share not found: {share_id}"
            )));
        }
        if !response.status().is_success() {
            return Err(DaemonError::InternalError(format!(
                "Brain fetch failed: {}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| DaemonError::InternalError(format!("Read body: {e}")))?;
        Ok(bytes.to_vec())
    }

    /// Renew (extend) a share's TTL. `owner_token_b64url` is base64url-no-pad.
    pub async fn renew(
        &self,
        share_id: &str,
        owner_token_b64url: &str,
        extend_secs: u64,
    ) -> Result<BrainRenewResponse> {
        let url = format!("{}/api/brain/share/{}", self.base_url, share_id);
        let body = RenewRequestBody {
            owner_token: owner_token_b64url,
            extend_secs,
        };
        let response = self
            .client
            .patch(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| DaemonError::InternalError(format!("Brain renew request failed: {e}")))?;
        if response.status() == 403 {
            return Err(DaemonError::InvalidRequest(
                "Unauthorized (wrong owner token)".into(),
            ));
        }
        if response.status() == 404 {
            return Err(DaemonError::InvalidRequest(format!(
                "Share not found: {share_id}"
            )));
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(DaemonError::InternalError(format!(
                "Brain renew failed: {status} - {body}"
            )));
        }
        response
            .json::<BrainRenewResponse>()
            .await
            .map_err(|e| DaemonError::InternalError(format!("Brain renew parse: {e}")))
    }

    /// Revoke (delete) a share. `owner_token_b64url` is base64url-no-pad.
    pub async fn revoke(&self, share_id: &str, owner_token_b64url: &str) -> Result<()> {
        let url = format!("{}/api/brain/share/{}", self.base_url, share_id);
        let body = RevokeRequestBody {
            owner_token: owner_token_b64url,
        };
        let response = self
            .client
            .delete(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| DaemonError::InternalError(format!("Brain revoke request failed: {e}")))?;
        if response.status() == 403 {
            return Err(DaemonError::InvalidRequest("Unauthorized".into()));
        }
        if response.status() == 404 {
            return Err(DaemonError::InvalidRequest(format!(
                "Share not found: {share_id}"
            )));
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(DaemonError::InternalError(format!(
                "Brain revoke failed: {status} - {body}"
            )));
        }
        let resp = response
            .json::<RevokeResponseBody>()
            .await
            .map_err(|e| DaemonError::InternalError(format!("Brain revoke parse: {e}")))?;
        if !resp.deleted {
            return Err(DaemonError::InternalError(
                "Server reported revoke did not succeed".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_construction_keeps_base_url() {
        let c = BrainShareHttpClient::new("https://example.com").unwrap();
        assert_eq!(c.base_url(), "https://example.com");
    }

    #[test]
    fn client_default_url() {
        let c = BrainShareHttpClient::with_default_url().unwrap();
        assert_eq!(c.base_url(), DEFAULT_BASE_URL);
    }

    #[test]
    fn client_is_clone() {
        let c = BrainShareHttpClient::new("https://example.com").unwrap();
        assert_eq!(c.clone().base_url(), "https://example.com");
    }

    #[test]
    fn renew_body_serialization() {
        let b = RenewRequestBody {
            owner_token: "tok",
            extend_secs: 86400,
        };
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"owner_token":"tok","extend_secs":86400}"#
        );
    }

    #[test]
    fn revoke_body_serialization() {
        let b = RevokeRequestBody { owner_token: "tok" };
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"owner_token":"tok"}"#
        );
    }

    #[test]
    fn upload_response_deserialization() {
        let json = r#"{"share_id":"abc123xyz0","expires_at":"2026-06-29T12:00:00.000Z","size_bytes":2048,"url":"https://share.nevoflux.app/b/abc123xyz0"}"#;
        let r: BrainUploadResponse = serde_json::from_str(json).unwrap();
        assert_eq!(r.share_id, "abc123xyz0");
        assert_eq!(r.size_bytes, 2048);
        assert_eq!(r.url, "https://share.nevoflux.app/b/abc123xyz0");
    }

    #[test]
    fn renew_response_deserialization() {
        let json = r#"{"share_id":"abc123xyz0","expires_at":"2026-06-29T12:00:00.000Z"}"#;
        let r: BrainRenewResponse = serde_json::from_str(json).unwrap();
        assert_eq!(r.share_id, "abc123xyz0");
        assert_eq!(r.expires_at, "2026-06-29T12:00:00.000Z");
    }

    #[test]
    fn revoke_response_deserialization() {
        let r: RevokeResponseBody = serde_json::from_str(r#"{"deleted":true}"#).unwrap();
        assert!(r.deleted);
    }
}
