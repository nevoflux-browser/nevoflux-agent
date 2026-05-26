//! Typed error enum for the LLM gateway.
//!
//! Hardened error handling (M2-3): instead of returning `StatusCode::BAD_GATEWAY`
//! for every upstream failure, the handler path classifies the failure into
//! one of a handful of categories that map cleanly to the right HTTP status
//! code for our client. See `handlers.rs` for how these are produced from
//! `reqwest::Response` / `reqwest::Error`, and for the OpenAI-compatible
//! error envelope used in responses.
//!
//! This module is intentionally **pure**: it depends only on `axum` (for
//! `StatusCode`) and `serde_json` (for the response body). It must NOT
//! depend on `reqwest`, so the enum stays a value type that's trivial to
//! construct in tests.

use axum::http::StatusCode;
use std::time::Duration;

/// Classification of an upstream / gateway failure. Each variant maps
/// to a single HTTP status code returned to our client; see
/// [`GatewayError::status_code`].
#[derive(Debug)]
pub enum GatewayError {
    /// Upstream returned 429 and either:
    /// - we used our one retry budget and it failed again, or
    /// - the `Retry-After` exceeded our `upstream_retry_max_wait`.
    ///
    /// `retry_after` is the parsed `Retry-After` header value, if any,
    /// so the handler can echo it back as a response header.
    RateLimited {
        retry_after: Option<Duration>,
        upstream_body: String,
    },

    /// Upstream returned a 5xx. We mask this as `502 Bad Gateway` to
    /// our client because the upstream is the one that's broken.
    UpstreamServerError {
        upstream_status: u16,
        upstream_body: String,
    },

    /// Upstream returned a 4xx other than 429. We propagate the
    /// upstream's status code so the client can react correctly
    /// (`401` = auth issue, `400` = malformed, `413` = too large, ...).
    UpstreamClientError {
        upstream_status: u16,
        upstream_body: String,
    },

    /// `reqwest` connection-level error (DNS / TCP / TLS).
    UpstreamUnreachable { detail: String },

    /// One of our timeouts fired before upstream responded (or before
    /// the next stream chunk arrived).
    UpstreamTimeout { phase: TimeoutPhase },

    /// Internal error in the gateway itself (encoding failure, translator
    /// failure, etc.). Surfaces as `500` so it doesn't get confused with
    /// an upstream-flavored `5xx`.
    Internal { detail: String },
}

/// Which phase of the upstream request was in flight when our timeout
/// fired. Tagged onto [`GatewayError::UpstreamTimeout`] so logs +
/// response bodies can tell `couldn't connect` from `connected but never
/// replied` from `streaming stalled`.
#[derive(Debug, Clone, Copy)]
pub enum TimeoutPhase {
    /// Couldn't establish the TCP/TLS connection in time.
    Connect,
    /// Total request budget exceeded for a non-stream request.
    Request,
    /// Stream stalled — no chunk for the idle-timeout window.
    StreamIdle,
}

impl GatewayError {
    /// HTTP status code we return to our client for this error.
    pub fn status_code(&self) -> StatusCode {
        match self {
            GatewayError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            GatewayError::UpstreamServerError { .. } => StatusCode::BAD_GATEWAY,
            GatewayError::UpstreamClientError {
                upstream_status, ..
            } => {
                // Best effort: keep the upstream's status code so clients
                // can react correctly. If for some reason the value isn't
                // a valid HTTP status (shouldn't happen for `reqwest`),
                // mask as `502`.
                StatusCode::from_u16(*upstream_status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            GatewayError::UpstreamUnreachable { .. } => StatusCode::BAD_GATEWAY,
            GatewayError::UpstreamTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            GatewayError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Build an OpenAI-compatible error envelope:
    ///
    /// ```json
    /// { "error": { "type": "...", "message": "..." } }
    /// ```
    ///
    /// The `message` field always quotes any upstream-supplied text
    /// through [`trunc`] so we don't leak a verbose 50 KB upstream body
    /// back to our client.
    pub fn to_openai_body(&self) -> serde_json::Value {
        let (kind, message) = match self {
            GatewayError::RateLimited { upstream_body, .. } => {
                ("rate_limited", trunc(upstream_body))
            }
            GatewayError::UpstreamServerError {
                upstream_status,
                upstream_body,
            } => (
                "upstream_server_error",
                format!("upstream {upstream_status}: {}", trunc(upstream_body)),
            ),
            GatewayError::UpstreamClientError {
                upstream_status,
                upstream_body,
            } => (
                "upstream_client_error",
                format!("upstream {upstream_status}: {}", trunc(upstream_body)),
            ),
            GatewayError::UpstreamUnreachable { detail } => {
                ("upstream_unreachable", detail.clone())
            }
            GatewayError::UpstreamTimeout { phase } => {
                ("upstream_timeout", format!("phase={phase:?}"))
            }
            GatewayError::Internal { detail } => ("internal_error", detail.clone()),
        };
        serde_json::json!({
            "error": {
                "type": kind,
                "message": message,
            }
        })
    }
}

/// Truncate upstream-quoted text to ~2 KB. Anything longer gets a
/// trailing `"...truncated"` marker so a verbose upstream can't leak a
/// huge body through us. 2 KB is well above what any sane provider
/// error response contains while still being small enough that we don't
/// blow up logs.
fn trunc(s: impl AsRef<str>) -> String {
    const MAX: usize = 2048;
    let s = s.as_ref();
    if s.len() <= MAX {
        s.to_string()
    } else {
        // Walk back to a char boundary so we don't slice mid-codepoint.
        let mut end = MAX;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = s[..end].to_string();
        out.push_str("...truncated");
        out
    }
}
