// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `POST /v1/upload/screenshot/:request_id` — content script delivers a
//! captured PNG (or any image format the LLM accepts) for the matching
//! `browser_screenshot` tool dispatch. The bytes are parked in
//! `AssetServerState::upload_inbox` keyed by `request_id`; the awaiting
//! tool handler picks them up via `AssetServer::await_screenshot`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::asset_server::state::AssetServerState;

/// 30s is plenty: tool handler typically calls `await_screenshot` BEFORE
/// dispatching the browser request, so by the time the POST arrives a
/// waiter is already parked. The TTL only matters when the producer-first
/// branch fires (typical in tests).
const ORPHAN_BYTES_TTL: Duration = Duration::from_secs(30);

pub async fn handle_screenshot(
    State(state): State<Arc<AssetServerState>>,
    Path(request_id): Path<String>,
    body: Bytes,
) -> Response {
    let bytes_len = body.len();
    match state
        .upload_inbox
        .deliver(&request_id, body, ORPHAN_BYTES_TTL)
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "request_id": request_id,
                "bytes": bytes_len,
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(request_id, error = %e, "asset_server: screenshot upload rejected");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "ok": false,
                    "request_id": request_id,
                    "error": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}
