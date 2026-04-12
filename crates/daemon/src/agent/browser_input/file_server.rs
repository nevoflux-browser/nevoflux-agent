// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Localhost HTTP file server for upload token redemption.
//!
//! Exposes a single `GET /file/:token` endpoint.  The Actor calls this
//! endpoint to download a file whose upload token was registered via
//! [`TokenStore`].  Each token is single-use and expires after
//! [`TOKEN_TTL`](super::upload::TOKEN_TTL).

use std::sync::{Arc, OnceLock};

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tokio::net::TcpListener;
use tokio::sync::OnceCell;

use super::upload::TokenStore;

// ---------------------------------------------------------------------------
// Shared server state
// ---------------------------------------------------------------------------

/// State threaded through the axum router.
#[derive(Clone)]
struct FileServerState {
    token_store: Arc<TokenStore>,
}

// ---------------------------------------------------------------------------
// Route handler
// ---------------------------------------------------------------------------

/// `GET /file/:token` — consume a token and stream the file back.
async fn handle_file_download(
    State(state): State<FileServerState>,
    Path(token): Path<String>,
) -> Response {
    // Atomically remove the entry; returns None if unknown or expired.
    let entry = match state.token_store.take(&token) {
        Some(e) => e,
        None => {
            return (StatusCode::NOT_FOUND, "Token not found or expired").into_response();
        }
    };

    // Read the file from disk.
    let bytes = match tokio::fs::read(&entry.path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(path = %entry.path.display(), error = %e, "file_server: read failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file: {e}"),
            )
                .into_response();
        }
    };

    // Build Content-Disposition header with the original filename.
    let content_disposition = format!(
        "attachment; filename=\"{}\"",
        entry.file_name.replace('"', "\\\"")
    );

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, entry.mime_type),
            (header::CONTENT_DISPOSITION, content_disposition),
        ],
        bytes,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Server startup
// ---------------------------------------------------------------------------

/// Start a localhost-only HTTP file server on a random port.
///
/// Returns the bound port number. The server runs in a detached Tokio task.
pub async fn start_file_server(token_store: Arc<TokenStore>) -> Result<u16, String> {
    let state = FileServerState { token_store };

    let app = Router::new()
        .route("/file/:token", get(handle_file_download))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("file_server: bind failed: {e}"))?;

    let port = listener
        .local_addr()
        .map_err(|e| format!("file_server: local_addr failed: {e}"))?
        .port();

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "file_server: server error");
        }
    });

    tracing::info!(port, "file_server: started on 127.0.0.1:{port}");
    Ok(port)
}

// ---------------------------------------------------------------------------
// Lazy singleton
// ---------------------------------------------------------------------------

/// Shared [`TokenStore`] singleton.
static TOKEN_STORE: OnceLock<Arc<TokenStore>> = OnceLock::new();

/// Port singleton — initialized on the first call to
/// [`get_or_start_file_server`].
static FILE_SERVER_PORT: OnceCell<u16> = OnceCell::const_new();

/// Return `(port, Arc<TokenStore>)`, starting the server on the first call.
///
/// Subsequent calls return the cached values without starting a new server.
pub async fn get_or_start_file_server() -> Result<(u16, Arc<TokenStore>), String> {
    // Ensure the token store is initialized.
    let store = TOKEN_STORE
        .get_or_init(|| Arc::new(TokenStore::new()))
        .clone();

    // Ensure the server is started and its port is cached.
    let port = FILE_SERVER_PORT
        .get_or_try_init(|| async { start_file_server(store.clone()).await })
        .await
        .copied()
        .map_err(|e| e.clone())?;

    Ok((port, store))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use tempfile::NamedTempFile;

    use super::super::upload::{TokenEntry, TOKEN_TTL};
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build a reqwest client that never hits a proxy (safe for localhost tests).
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    /// Write `contents` to a temp file and return it (caller must keep it alive).
    fn make_temp_file(contents: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents).unwrap();
        f.flush().unwrap();
        f
    }

    /// Build a valid [`TokenEntry`] pointing at the given path.
    fn make_entry(f: &NamedTempFile, mime: &str) -> TokenEntry {
        TokenEntry {
            path: f.path().to_path_buf(),
            mime_type: mime.to_string(),
            file_name: f
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            size: f.path().metadata().map(|m| m.len()).unwrap_or(0),
            expires_at: Instant::now() + TOKEN_TTL,
        }
    }

    // -----------------------------------------------------------------------
    // file_server_serves_valid_token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn file_server_serves_valid_token() {
        let store = Arc::new(TokenStore::new());
        let port = start_file_server(store.clone()).await.unwrap();

        let contents = b"hello file server";
        let f = make_temp_file(contents);
        let entry = make_entry(&f, "text/plain");
        let token = store.insert(entry);

        let url = format!("http://127.0.0.1:{port}/file/{token}");
        let resp = test_client().get(&url).send().await.unwrap();

        assert_eq!(resp.status(), 200);
        // Verify Content-Type header.
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(ct, "text/plain");
        // Verify body.
        let body = resp.bytes().await.unwrap();
        assert_eq!(body.as_ref(), contents);
        // Token must be consumed — store should now be empty.
        assert!(store.is_empty());
    }

    // -----------------------------------------------------------------------
    // file_server_returns_404_for_unknown_token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn file_server_returns_404_for_unknown_token() {
        let store = Arc::new(TokenStore::new());
        let port = start_file_server(store.clone()).await.unwrap();

        let url = format!(
            "http://127.0.0.1:{port}/file/00000000-0000-0000-0000-000000000000"
        );
        let resp = test_client().get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    // -----------------------------------------------------------------------
    // file_server_returns_404_for_expired_token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn file_server_returns_404_for_expired_token() {
        let store = Arc::new(TokenStore::new());
        let port = start_file_server(store.clone()).await.unwrap();

        let f = make_temp_file(b"data");
        // Create an already-expired entry.
        let entry = TokenEntry {
            path: f.path().to_path_buf(),
            mime_type: "application/octet-stream".to_string(),
            file_name: "data.bin".to_string(),
            size: 4,
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        let token = store.insert(entry);

        let url = format!("http://127.0.0.1:{port}/file/{token}");
        let resp = test_client().get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    // -----------------------------------------------------------------------
    // file_server_single_use_token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn file_server_single_use_token() {
        let store = Arc::new(TokenStore::new());
        let port = start_file_server(store.clone()).await.unwrap();

        let f = make_temp_file(b"once");
        let entry = make_entry(&f, "application/octet-stream");
        let token = store.insert(entry);

        let url = format!("http://127.0.0.1:{port}/file/{token}");

        // First request succeeds.
        let resp1 = test_client().get(&url).send().await.unwrap();
        assert_eq!(resp1.status(), 200);

        // Second request must fail — token was consumed on the first download.
        let resp2 = test_client().get(&url).send().await.unwrap();
        assert_eq!(resp2.status(), 404);
    }
}
