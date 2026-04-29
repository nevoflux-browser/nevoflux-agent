// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `GET /v1/asset/composition/:id/:name` — single asset bytes from the
//! composition's `files` map.
//!
//! Auth: bearer header (default) OR `?t=<short_token>` query param. The
//! token is the multi-use composition token issued by
//! `register_composition_assets` / the composition GET handler; it is
//! `peek`-validated (multi-use within the 5-minute TTL).
//!
//! Range support: a single `Range: bytes=A-B` request returns 206 with a
//! `Content-Range` header. Multi-range requests fall back to 416 — Phase 2
//! simplification per the user spec.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use crate::asset_server::auth::check_composition_request_auth;
use crate::asset_server::state::AssetServerState;
use crate::canvas_video::asset_inline;

#[derive(serde::Deserialize)]
pub struct AuthQuery {
    pub t: Option<String>,
}

pub async fn handle(
    State(state): State<Arc<AssetServerState>>,
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<AuthQuery>,
    headers: HeaderMap,
) -> Response {
    if !check_composition_request_auth(&state, &id, &headers, query.t.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let storage = match state.storage.as_ref() {
        Some(db) => db.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "asset server: storage not wired",
            )
                .into_response();
        }
    };

    use nevoflux_storage::repositories::ArtifactRepository;
    let repo = ArtifactRepository::new(&storage);
    let rec = match repo.get(&id) {
        Ok(Some(rec)) => rec,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("composition not found: {id}"),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(composition_id = %id, error = %e, "asset handler: storage error");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage error").into_response();
        }
    };

    let files = match rec.files.as_ref() {
        Some(f) => f,
        None => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "artifact has no files map (not a composition)",
            )
                .into_response();
        }
    };

    let asset_key = format!("assets/{name}");
    let payload = match files.get(&asset_key) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                format!("asset not found: {asset_key}"),
            )
                .into_response();
        }
    };

    // Decode bytes + sniff MIME. Magic bytes win over filename so an agent
    // that saved a JPEG as `.png` still gets a `image/jpeg` response.
    let (bytes, mime) = decode_payload(payload, &name);

    serve_with_range(&headers, mime, bytes)
}

/// Decode a files-map entry into raw bytes + sniffed MIME.
///
/// Binary assets are stored base64-encoded; text assets (SVG, CSS, JSON)
/// are stored as raw UTF-8. We discriminate via `is_likely_base64` and
/// magic-byte sniffing, falling back to the filename extension only for
/// text content (which has no usable magic).
fn decode_payload(payload: &str, name: &str) -> (Vec<u8>, String) {
    if asset_inline::is_likely_base64(payload) {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let mime = asset_inline::magic_bytes_mime(payload)
            .map(|s| s.to_string())
            .unwrap_or_else(|| asset_inline::mime_for_path(name).to_string());
        let bytes = STANDARD
            .decode(payload.as_bytes())
            .unwrap_or_else(|_| payload.as_bytes().to_vec());
        (bytes, mime)
    } else {
        let mime = asset_inline::mime_for_path(name).to_string();
        (payload.as_bytes().to_vec(), mime)
    }
}

fn serve_with_range(headers: &HeaderMap, mime: String, bytes: Vec<u8>) -> Response {
    let total = bytes.len();
    let range_hdr = headers.get(header::RANGE).and_then(|v| v.to_str().ok());

    let parsed = match range_hdr {
        Some(s) => parse_single_range(s, total),
        None => RangeResult::None,
    };

    match parsed {
        RangeResult::None => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, total.to_string())
            .body(Body::from(bytes))
            .unwrap(),
        RangeResult::Partial { start, end } => {
            let slice = bytes[start..=end].to_vec();
            let len = slice.len();
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_LENGTH, len.to_string())
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .body(Body::from(slice))
                .unwrap()
        }
        RangeResult::Unsatisfiable => Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{total}"))
            .body(Body::empty())
            .unwrap(),
    }
}

enum RangeResult {
    None,
    Partial { start: usize, end: usize },
    Unsatisfiable,
}

fn parse_single_range(s: &str, total: usize) -> RangeResult {
    let s = s.trim();
    let s = match s.strip_prefix("bytes=") {
        Some(v) => v.trim(),
        None => return RangeResult::Unsatisfiable,
    };
    // Multi-range (`A-B,C-D`) collapses to 416 per Phase 2 simplification.
    if s.contains(',') {
        return RangeResult::Unsatisfiable;
    }
    let (raw_start, raw_end) = match s.split_once('-') {
        Some(p) => p,
        None => return RangeResult::Unsatisfiable,
    };
    if total == 0 {
        return RangeResult::Unsatisfiable;
    }

    if raw_start.is_empty() {
        // suffix range: `-N` = last N bytes
        let n: usize = match raw_end.parse() {
            Ok(v) => v,
            Err(_) => return RangeResult::Unsatisfiable,
        };
        if n == 0 {
            return RangeResult::Unsatisfiable;
        }
        let n = n.min(total);
        return RangeResult::Partial {
            start: total - n,
            end: total - 1,
        };
    }

    let start: usize = match raw_start.parse() {
        Ok(v) => v,
        Err(_) => return RangeResult::Unsatisfiable,
    };
    if start >= total {
        return RangeResult::Unsatisfiable;
    }
    let end: usize = if raw_end.is_empty() {
        total - 1
    } else {
        match raw_end.parse::<usize>() {
            Ok(v) => v.min(total - 1),
            Err(_) => return RangeResult::Unsatisfiable,
        }
    };
    if end < start {
        return RangeResult::Unsatisfiable;
    }
    RangeResult::Partial { start, end }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_range() {
        match parse_single_range("bytes=0-9", 100) {
            RangeResult::Partial { start, end } => {
                assert_eq!((start, end), (0, 9));
            }
            _ => panic!("expected partial"),
        }
    }

    #[test]
    fn parses_open_end_range() {
        match parse_single_range("bytes=10-", 100) {
            RangeResult::Partial { start, end } => {
                assert_eq!((start, end), (10, 99));
            }
            _ => panic!("expected partial"),
        }
    }

    #[test]
    fn parses_suffix_range() {
        match parse_single_range("bytes=-5", 100) {
            RangeResult::Partial { start, end } => {
                assert_eq!((start, end), (95, 99));
            }
            _ => panic!("expected partial"),
        }
    }

    #[test]
    fn rejects_multi_range() {
        assert!(matches!(
            parse_single_range("bytes=0-1,3-4", 100),
            RangeResult::Unsatisfiable
        ));
    }

    #[test]
    fn rejects_out_of_bounds() {
        assert!(matches!(
            parse_single_range("bytes=200-300", 100),
            RangeResult::Unsatisfiable
        ));
    }
}
