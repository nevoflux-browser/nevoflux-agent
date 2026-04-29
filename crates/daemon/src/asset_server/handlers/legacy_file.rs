// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `GET /file/:token` — legacy browser_upload download route.
//!
//! Ported byte-for-byte from `agent::browser_input::file_server::handle_file_download`
//! so Step A is purely additive: the existing `file_server.rs` keeps
//! running unchanged, and AssetServer serves an identical response when
//! given the same `TokenEntry`.
//!
//! The auth model is the URL-path single-use UUID — bearer is NOT applied.
//! Per design D7 / §5.4, this route lives at the root and bypasses the
//! `/v1/*` bearer middleware naturally.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use crate::asset_server::state::AssetServerState;

/// Atomically take the token, read the file, return it with the original
/// filename in `Content-Disposition`. Returns 404 for unknown / expired
/// tokens (the existing semantics — second GET on a single-use token).
pub async fn handle(
    State(state): State<Arc<AssetServerState>>,
    Path(token): Path<String>,
) -> Response {
    let entry = match state.download_tokens.take(&token) {
        Some(e) => e,
        None => {
            return (StatusCode::NOT_FOUND, "Token not found or expired").into_response();
        }
    };

    let bytes = match tokio::fs::read(&entry.path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(path = %entry.path.display(), error = %e, "asset_server::legacy_file: read failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file: {e}"),
            )
                .into_response();
        }
    };

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
