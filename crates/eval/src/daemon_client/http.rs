//! HTTP wrapper around the daemon's `/_eval/*` endpoints. Wire-format-
//! compatible with `daemon::eval_bridge::routes`. All requests carry the
//! bearer token from `daemon.lock`.

use crate::daemon_client::lock::DaemonLock;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("daemon returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct DaemonHttpClient {
    base: String,
    bearer: String,
    http: Client,
}

#[derive(Debug, Serialize)]
pub struct CreateSessionRequest {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mock_browser: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SetupStep {
    InjectMessage { role: String, content: String },
    SeedMemory { key: String, value: String },
    GrantPermission { tool: String },
}

#[derive(Debug, Serialize)]
pub struct SetupRequest {
    pub steps: Vec<SetupStep>,
}

#[derive(Debug, Deserialize)]
pub struct SetupResponse {
    pub applied: usize,
    pub skipped: usize,
}

#[derive(Debug, Serialize)]
pub struct SubmitMessageRequest {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Server-side default is ToolsConfig::None when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_config: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct SubmitMessageResponse {
    pub accepted: bool,
}

impl DaemonHttpClient {
    pub fn from_lock(lock: &DaemonLock) -> Self {
        Self {
            base: format!("http://{}/_eval", lock.http_addr),
            bearer: lock.bearer_token.clone(),
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub async fn create_session(
        &self,
        req: &CreateSessionRequest,
    ) -> Result<CreateSessionResponse, HttpError> {
        let resp = self
            .http
            .post(format!("{}/sessions", self.base))
            .bearer_auth(&self.bearer)
            .json(req)
            .send()
            .await?;
        check_ok_and_decode(resp).await
    }

    pub async fn setup_session(
        &self,
        session_id: &str,
        req: &SetupRequest,
    ) -> Result<SetupResponse, HttpError> {
        let resp = self
            .http
            .post(format!("{}/sessions/{}/setup", self.base, session_id))
            .bearer_auth(&self.bearer)
            .json(req)
            .send()
            .await?;
        check_ok_and_decode(resp).await
    }

    pub async fn submit_message(
        &self,
        session_id: &str,
        req: &SubmitMessageRequest,
    ) -> Result<SubmitMessageResponse, HttpError> {
        let resp = self
            .http
            .post(format!("{}/sessions/{}/messages", self.base, session_id))
            .bearer_auth(&self.bearer)
            .json(req)
            .send()
            .await?;
        check_ok_and_decode(resp).await
    }

    /// Returns the raw streaming response. Caller wraps it in an SSE parser
    /// (see `daemon_client::sse`). Uses a no-timeout client because SSE is
    /// long-lived.
    pub async fn open_events(&self, session_id: &str) -> Result<reqwest::Response, HttpError> {
        let resp = Client::builder()
            .build()
            .expect("reqwest client (no timeout)")
            .get(format!("{}/sessions/{}/events", self.base, session_id))
            .bearer_auth(&self.bearer)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpError::Status { status, body });
        }
        Ok(resp)
    }

    /// Returns traces as JSONL (one record per line).
    pub async fn get_traces(&self, session_id: &str) -> Result<String, HttpError> {
        let resp = self
            .http
            .get(format!("{}/sessions/{}/traces", self.base, session_id))
            .bearer_auth(&self.bearer)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpError::Status { status, body });
        }
        Ok(resp.text().await?)
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<(), HttpError> {
        let resp = self
            .http
            .delete(format!("{}/sessions/{}", self.base, session_id))
            .bearer_auth(&self.bearer)
            .send()
            .await?;
        if resp.status().as_u16() != 204 {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpError::Status { status, body });
        }
        Ok(())
    }
}

async fn check_ok_and_decode<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, HttpError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(HttpError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let bytes = resp.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}
