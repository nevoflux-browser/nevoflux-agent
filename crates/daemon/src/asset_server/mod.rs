// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Asset & Stream Plane HTTP server.
//!
//! Companion to native messaging: this localhost-only server handles
//! bulk byte streams (uploads, downloads, asset GET, render frame upload,
//! OAuth callbacks). See
//! `docs/superpowers/specs/2026-04-29-asset-stream-plane-design.md` for
//! the full design and §1-§4 for the boundary rules.
//!
//! Phase 1 (Step A) lights:
//! - `GET /file/:token` (legacy, bearer-less, `download_tokens`)
//! - `GET /v1/health`
//! - `GET /v1/capabilities`
//! - `POST /v1/upload/screenshot/:request_id` (writes to upload_inbox)
//! - all Phase 2-5 routes return `501 Not Implemented` with
//!   `X-NF-Future-Phase: phase-N`

mod auth;
pub mod handlers;
pub mod inbox;
mod port;
pub mod state;
pub mod token_store;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use thiserror::Error;
use tokio::sync::oneshot;

pub use inbox::InboxError;
pub use state::{AssetPlaneInfo, AssetServerState};

/// Wire format version for `bridge:hello.asset_plane.version`.
pub const ASSET_SERVER_VERSION: u32 = 1;

/// Default cap for POST request bodies.  64 MiB lets a 4K PNG screenshot
/// (~20 MiB worst case) and an MP4 frame batch through comfortably.
pub const DEFAULT_MAX_BODY_SIZE: usize = 64 * 1024 * 1024;

/// `download_tokens` TTL — single-use, browser_upload `/file/:token`.
pub const DOWNLOAD_TOKEN_TTL: Duration = Duration::from_secs(60);
/// `composition_tokens` TTL — multi-use, `/v1/asset/composition/...`.
#[allow(dead_code)] // lit in Phase 2
pub const COMPOSITION_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
/// `blob_tokens` TTL — single-or-multi, `/v1/blob/<id>`.
#[allow(dead_code)] // lit in Phase 5
pub const BLOB_TOKEN_TTL: Duration = Duration::from_secs(60 * 60);
/// Tool-side wait deadline for the screenshot inbox.
pub const SCREENSHOT_INBOX_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum BindError {
    #[error("no free port found in range {start}..{end}")]
    NoFreePortInRange { start: u16, end: u16 },
    #[error("axum serve failed: {0}")]
    Serve(String),
}

/// Boot-time configuration for the AssetServer.
#[derive(Debug, Clone)]
pub struct AssetServerConfig {
    /// Half-open port range `[start, end)`. Must match the bridge's range
    /// so co-existing daemons each get distinct AssetServer ports.
    pub port_range: std::ops::Range<u16>,
    /// Daemon-wide bearer; rotates on daemon restart.
    pub bearer_token: String,
    /// Increments on token rotation. Extension URL caches use this to
    /// invalidate stale URLs.
    pub session_id: String,
    /// Max body size for /v1/* POST requests.
    pub max_body_size: usize,
    /// CORS allow-origin (the moz-extension://... URL). May be `None` at
    /// boot — populated when the extension first connects.
    pub allowed_origin: Option<String>,
}

impl Default for AssetServerConfig {
    fn default() -> Self {
        Self {
            // Default range matches `ServerConfig` in `server.rs`. The
            // bridge has already taken its slot by the time AssetServer
            // starts, so AssetServer will pick the next free.
            port_range: 19500..19601,
            bearer_token: token_store::random_token(),
            session_id: token_store::random_token(),
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            allowed_origin: None,
        }
    }
}

/// Running AssetServer. Cheaply cloneable handle that exposes the bound
/// port, bearer token, and the typed API for feature modules to call
/// (`register_download`, `await_screenshot`, etc.).
#[derive(Clone)]
pub struct AssetServer {
    state: Arc<AssetServerState>,
    bound_port: u16,
    /// Held in an `Arc` so that cloning the handle does not steal the
    /// shutdown sender — only the original holder can shut the server
    /// down (typically: nobody, since the server lives until process
    /// exit).
    _shutdown_tx: Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl AssetServer {
    /// Start the server. Returns once the listener is bound and the
    /// background serve task is spawned. Eviction loops for all three
    /// TokenStores and the upload inbox are also spawned.
    pub async fn start(config: AssetServerConfig) -> Result<Self, BindError> {
        let listener = port::bind_in_range(&config.port_range).await?;
        let bound_port = listener
            .local_addr()
            .map_err(|e| BindError::Serve(format!("local_addr: {e}")))?
            .port();

        let state = Arc::new(AssetServerState::new(&config));

        token_store::spawn_eviction_loop(
            state.download_tokens.clone(),
            Duration::from_secs(30),
        );
        token_store::spawn_eviction_loop(
            state.composition_tokens.clone(),
            Duration::from_secs(60),
        );
        token_store::spawn_eviction_loop(
            state.blob_tokens.clone(),
            Duration::from_secs(60),
        );
        inbox::spawn_inbox_eviction_loop(state.upload_inbox.clone(), Duration::from_secs(30));

        let app = build_router(state.clone(), config.max_body_size);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
            if let Err(e) = serve.await {
                tracing::error!("asset_server: serve error: {e}");
            }
        });

        tracing::info!(
            port = bound_port,
            "asset_server: started on 127.0.0.1:{bound_port}"
        );

        Ok(Self {
            state,
            bound_port,
            _shutdown_tx: Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
        })
    }

    pub fn bound_port(&self) -> u16 {
        self.bound_port
    }

    pub fn bearer_token(&self) -> &str {
        &self.state.bearer_token
    }

    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    pub fn state(&self) -> &Arc<AssetServerState> {
        &self.state
    }

    /// Build the `asset_plane` info struct delivered to the extension via
    /// the `system_command status` SystemResponse.
    pub fn asset_plane_info(&self) -> AssetPlaneInfo {
        AssetPlaneInfo {
            port: self.bound_port,
            bearer_token: self.bearer_token().to_string(),
            session_id: self.session_id().to_string(),
            version: ASSET_SERVER_VERSION,
            max_body_size: self.state.max_body_size,
            phases: vec!["screenshot-upload".to_string()],
        }
    }

    /// Update the CORS allow-origin once the extension has identified
    /// itself. Subsequent preflights echo the new origin.
    pub fn set_allowed_origin(&self, origin: Option<String>) {
        self.state.set_allowed_origin(origin);
    }

    /// Park a screenshot upload slot under `request_id`, then await the
    /// extension's POST. Returns the captured bytes or a timeout/cancel
    /// error.
    pub async fn await_screenshot(
        &self,
        request_id: &str,
        timeout: Duration,
    ) -> Result<Bytes, InboxError> {
        self.state
            .upload_inbox
            .await_request(request_id, timeout)
            .await
    }

    /// Step B caller: register a path for one-shot download via
    /// `/file/:token`. Returns the full URL the consumer (browser actor,
    /// ffmpeg subprocess) should fetch.
    pub fn register_download(
        &self,
        path: std::path::PathBuf,
        mime_type: String,
        file_name: String,
        ttl: Duration,
    ) -> String {
        use crate::agent::browser_input::upload::TokenEntry;
        use std::time::Instant;

        let entry = TokenEntry {
            path,
            mime_type,
            file_name,
            size: 0, // size is informational only — the GET reads from disk
            expires_at: Instant::now() + ttl,
        };
        let token = self.state.download_tokens.insert(entry);
        format!("http://127.0.0.1:{}/file/{}", self.bound_port, token)
    }
}

fn build_router(state: Arc<AssetServerState>, max_body: usize) -> Router {
    // /v1/* routes — bearer-protected
    let v1 = Router::new()
        .route("/v1/health", get(handlers::health::handle))
        .route("/v1/capabilities", get(handlers::capabilities::handle))
        .route(
            "/v1/upload/screenshot/:request_id",
            post(handlers::upload::handle_screenshot),
        )
        // Phase 2-5 reserved namespaces — 501 stubs.
        .route("/v1/composition/:id", get(handlers::stubs::stub_phase2))
        .route(
            "/v1/asset/composition/:id/:name",
            get(handlers::stubs::stub_phase2),
        )
        .route("/v1/upload/asset/:id/:name", post(handlers::stubs::stub_phase3))
        .route(
            "/v1/upload/generic/:inbox",
            post(handlers::stubs::stub_phase3),
        )
        .route("/v1/render/:job/frame", post(handlers::stubs::stub_phase4))
        .route("/v1/render/:job/sse", get(handlers::stubs::stub_phase4))
        .route("/v1/blob/:id", get(handlers::stubs::stub_phase5))
        .route(
            "/v1/oauth/:provider/callback",
            get(handlers::stubs::stub_phase5),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::bearer_middleware,
        ))
        .with_state(state.clone());

    // Legacy /file/:token — bearer-LESS (single-use UUID is the auth).
    let legacy = Router::new()
        .route("/file/:token", get(handlers::legacy_file::handle))
        .with_state(state.clone());

    Router::new()
        .merge(legacy)
        .merge(v1)
        .layer(DefaultBodyLimit::max(max_body))
        // CORS layer must be outermost so OPTIONS preflights are answered
        // before the bearer layer rejects them.
        .layer(axum::middleware::from_fn_with_state(
            state,
            auth::cors_middleware,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::browser_input::upload::{TokenEntry, TOKEN_TTL};
    use std::time::Instant;

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    async fn boot() -> AssetServer {
        AssetServer::start(AssetServerConfig {
            bearer_token: "test-bearer".into(),
            session_id: "test-session".into(),
            ..Default::default()
        })
        .await
        .expect("AssetServer should boot in tests")
    }

    // -----------------------------------------------------------------------
    // §13.1 — bearer auth
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn missing_bearer_returns_401() {
        let server = boot().await;
        let url = format!("http://127.0.0.1:{}/v1/health", server.bound_port());
        let resp = test_client().get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn wrong_bearer_returns_401() {
        let server = boot().await;
        let url = format!("http://127.0.0.1:{}/v1/health", server.bound_port());
        let resp = test_client()
            .get(&url)
            .header("authorization", "Bearer nope")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn valid_bearer_returns_200_health() {
        let server = boot().await;
        let url = format!("http://127.0.0.1:{}/v1/health", server.bound_port());
        let resp = test_client()
            .get(&url)
            .header("authorization", "Bearer test-bearer")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok");
    }

    // -----------------------------------------------------------------------
    // §13.4 — /v1/capabilities returns phases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn capabilities_lists_screenshot_upload_phase() {
        let server = boot().await;
        let url = format!("http://127.0.0.1:{}/v1/capabilities", server.bound_port());
        let resp = test_client()
            .get(&url)
            .header("authorization", "Bearer test-bearer")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let phases = body["phases"].as_array().expect("phases must be array");
        assert!(phases.iter().any(|v| v.as_str() == Some("screenshot-upload")));
        assert_eq!(body["version"], 1);
    }

    // -----------------------------------------------------------------------
    // §13.1 — CORS preflight
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cors_preflight_returns_204_with_headers() {
        let server = boot().await;
        server.set_allowed_origin(Some("moz-extension://test-id".into()));
        let url = format!(
            "http://127.0.0.1:{}/v1/upload/screenshot/abc",
            server.bound_port()
        );
        let resp = test_client()
            .request(reqwest::Method::OPTIONS, &url)
            .header("origin", "moz-extension://test-id")
            .header("access-control-request-method", "POST")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("moz-extension://test-id")
        );
    }

    // -----------------------------------------------------------------------
    // §13.1 — screenshot inbox roundtrip
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn screenshot_post_then_await_roundtrip() {
        let server = boot().await;
        let url = format!(
            "http://127.0.0.1:{}/v1/upload/screenshot/req-roundtrip",
            server.bound_port()
        );
        // Producer first: extension POSTs.
        let resp = test_client()
            .post(&url)
            .header("authorization", "Bearer test-bearer")
            .body(b"PNG-bytes".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Consumer then awaits — inbox should already have the bytes.
        let bytes = server
            .await_screenshot("req-roundtrip", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"PNG-bytes");
    }

    #[tokio::test]
    async fn screenshot_await_then_post_roundtrip() {
        // Consumer-first ordering: tool dispatch awaits before extension
        // POSTs. This is the production path.
        let server = boot().await;
        let server_clone = server.clone();
        let waiter = tokio::spawn(async move {
            server_clone
                .await_screenshot("req-await-first", Duration::from_secs(2))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let url = format!(
            "http://127.0.0.1:{}/v1/upload/screenshot/req-await-first",
            server.bound_port()
        );
        let resp = test_client()
            .post(&url)
            .header("authorization", "Bearer test-bearer")
            .body(b"capture".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let bytes = waiter.await.unwrap().unwrap();
        assert_eq!(bytes.as_ref(), b"capture");
    }

    // -----------------------------------------------------------------------
    // §13.1 — 501 stubs carry X-NF-Future-Phase
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn phase2_route_returns_501_with_phase_header() {
        let server = boot().await;
        let url = format!(
            "http://127.0.0.1:{}/v1/composition/abc",
            server.bound_port()
        );
        let resp = test_client()
            .get(&url)
            .header("authorization", "Bearer test-bearer")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 501);
        assert_eq!(
            resp.headers()
                .get("x-nf-future-phase")
                .and_then(|v| v.to_str().ok()),
            Some("phase-2")
        );
    }

    #[tokio::test]
    async fn phase4_render_frame_returns_501_with_phase_header() {
        let server = boot().await;
        let url = format!(
            "http://127.0.0.1:{}/v1/render/job123/frame",
            server.bound_port()
        );
        let resp = test_client()
            .post(&url)
            .header("authorization", "Bearer test-bearer")
            .body(Vec::<u8>::new())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 501);
        assert_eq!(
            resp.headers()
                .get("x-nf-future-phase")
                .and_then(|v| v.to_str().ok()),
            Some("phase-4")
        );
    }

    #[tokio::test]
    async fn phase5_blob_returns_501_with_phase_header() {
        let server = boot().await;
        let url = format!("http://127.0.0.1:{}/v1/blob/abc", server.bound_port());
        let resp = test_client()
            .get(&url)
            .header("authorization", "Bearer test-bearer")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 501);
        assert_eq!(
            resp.headers()
                .get("x-nf-future-phase")
                .and_then(|v| v.to_str().ok()),
            Some("phase-5")
        );
    }

    // -----------------------------------------------------------------------
    // §13.2 — legacy /file/:token byte-for-byte parity
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn legacy_file_route_serves_byte_identical_response() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let server = boot().await;

        // Fixture: a small known payload.
        let mut tf = NamedTempFile::new().unwrap();
        tf.write_all(b"hello legacy file route").unwrap();
        tf.flush().unwrap();
        let entry = TokenEntry {
            path: tf.path().to_path_buf(),
            mime_type: "text/plain".into(),
            file_name: tf
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            size: 23,
            expires_at: Instant::now() + TOKEN_TTL,
        };
        // Register via the AssetServer's download_tokens (Step A: directly;
        // Step B will route through register_download).
        let token = server.state().download_tokens.insert(entry.clone());

        // GET via AssetServer.
        let new_url = format!("http://127.0.0.1:{}/file/{}", server.bound_port(), token);
        let new_resp = test_client().get(&new_url).send().await.unwrap();

        assert_eq!(new_resp.status(), 200);
        let new_ct = new_resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string());
        let new_cd = new_resp
            .headers()
            .get("content-disposition")
            .map(|v| v.to_str().unwrap().to_string());
        let new_body = new_resp.bytes().await.unwrap();

        // Compare against the still-running file_server.rs.
        let store =
            std::sync::Arc::new(crate::agent::browser_input::upload::TokenStore::new());
        let port = crate::agent::browser_input::file_server::start_file_server(store.clone())
            .await
            .unwrap();
        let old_token = store.insert(entry);
        let old_url = format!("http://127.0.0.1:{port}/file/{old_token}");
        let old_resp = test_client().get(&old_url).send().await.unwrap();
        assert_eq!(old_resp.status(), 200);
        let old_ct = old_resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string());
        let old_cd = old_resp
            .headers()
            .get("content-disposition")
            .map(|v| v.to_str().unwrap().to_string());
        let old_body = old_resp.bytes().await.unwrap();

        assert_eq!(new_ct, old_ct, "Content-Type differs");
        assert_eq!(new_cd, old_cd, "Content-Disposition differs");
        assert_eq!(new_body, old_body, "body bytes differ");
    }

    #[tokio::test]
    async fn legacy_file_token_is_single_use() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let server = boot().await;
        let mut tf = NamedTempFile::new().unwrap();
        tf.write_all(b"once").unwrap();
        tf.flush().unwrap();
        let entry = TokenEntry {
            path: tf.path().to_path_buf(),
            mime_type: "text/plain".into(),
            file_name: "once.txt".into(),
            size: 4,
            expires_at: Instant::now() + TOKEN_TTL,
        };
        let token = server.state().download_tokens.insert(entry);
        let url = format!("http://127.0.0.1:{}/file/{}", server.bound_port(), token);

        let r1 = test_client().get(&url).send().await.unwrap();
        assert_eq!(r1.status(), 200);
        let r2 = test_client().get(&url).send().await.unwrap();
        assert_eq!(r2.status(), 404);
    }

    // -----------------------------------------------------------------------
    // §13.3 — bridge:hello asset_plane shape
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn asset_plane_info_advertises_correct_fields() {
        let server = boot().await;
        let info = server.asset_plane_info();
        assert_eq!(info.port, server.bound_port());
        assert_eq!(info.bearer_token, "test-bearer");
        assert_eq!(info.session_id, "test-session");
        assert_eq!(info.version, ASSET_SERVER_VERSION);
        assert_eq!(info.max_body_size, DEFAULT_MAX_BODY_SIZE);
        assert!(info.phases.contains(&"screenshot-upload".to_string()));

        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("port").is_some());
        assert!(json.get("bearer_token").is_some());
        assert!(json.get("session_id").is_some());
    }

    // -----------------------------------------------------------------------
    // §13.5 — two daemons co-exist with distinct asset_plane.port
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn two_servers_pick_distinct_ports() {
        let s1 = boot().await;
        let s2 = boot().await;
        assert_ne!(
            s1.bound_port(),
            s2.bound_port(),
            "two AssetServers must end up on different ports"
        );
        let r = AssetServerConfig::default().port_range;
        assert!(r.contains(&s1.bound_port()));
        assert!(r.contains(&s2.bound_port()));
    }

    // -----------------------------------------------------------------------
    // Step B caller: register_download returns a working URL
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_download_returns_fetchable_url() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let server = boot().await;
        let mut tf = NamedTempFile::new().unwrap();
        tf.write_all(b"register_download payload").unwrap();
        tf.flush().unwrap();

        let url = server.register_download(
            tf.path().to_path_buf(),
            "text/plain".into(),
            "registered.txt".into(),
            DOWNLOAD_TOKEN_TTL,
        );
        assert!(url.starts_with(&format!("http://127.0.0.1:{}/file/", server.bound_port())));

        let resp = test_client().get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"register_download payload");

        // Single-use: second GET 404s.
        let r2 = test_client().get(&url).send().await.unwrap();
        assert_eq!(r2.status(), 404);
    }
}
