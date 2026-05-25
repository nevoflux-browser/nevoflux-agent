//! HTTP handlers for the gateway.
//!
//! Moved out of `main.rs` in M1 #010 so the daemon can construct the same
//! axum routes in-process. All items are `pub(crate)` and consumed only by
//! [`crate::server::serve`].

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures::StreamExt;
use nevoflux_llm::embedding::{EmbedKind, EmbeddingConfig, EmbeddingProvider, FastEmbedProvider};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::OnceCell;

use crate::embedding_dim::{zero_pad_to_gateway_dim, GATEWAY_OUTPUT_DIM};
use crate::translate::{
    anthropic_to_openai_response, openai_to_anthropic_request, AnthropicResponse,
    OpenAIChatRequest, StreamTranslator,
};

/// Shared application state.
pub(crate) struct AppState {
    pub(crate) bearer_token: String,
    pub(crate) chat_request_count: AtomicU64,
    pub(crate) upstream_base_url: String,
    pub(crate) upstream_api_key: String,
    /// If non-empty, overrides the `model` field of every incoming
    /// chat-completions request before hitting upstream. See 附录 B 决策 #25.
    pub(crate) upstream_model_override: String,
    pub(crate) anthropic_version: String,
    pub(crate) http: reqwest::Client,
    /// Lazily-initialized fastembed-backed embedder.
    ///
    /// Loading the ~120 MB ONNX weights is expensive, and gateways used
    /// only for chat-completions should not pay that cost. The cell is
    /// populated on first `/v1/embeddings` call via [`AppState::embedder`].
    pub(crate) embedder: OnceCell<Arc<FastEmbedProvider>>,
}

impl AppState {
    /// Return the shared [`FastEmbedProvider`], initializing it on the
    /// first call. Subsequent calls reuse the cached instance.
    ///
    /// The `FastEmbedProvider::new` constructor is synchronous and can
    /// load model weights from disk, so we wrap it in `spawn_blocking`
    /// to avoid stalling the async runtime.
    async fn embedder(&self) -> anyhow::Result<Arc<FastEmbedProvider>> {
        self.embedder
            .get_or_try_init(|| async {
                tracing::info!(
                    "initializing FastEmbedProvider (first /v1/embeddings call)"
                );
                let start = std::time::Instant::now();
                let provider = tokio::task::spawn_blocking(|| {
                    FastEmbedProvider::new(EmbeddingConfig::default())
                })
                .await
                .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))??;
                tracing::info!("FastEmbedProvider ready in {:?}", start.elapsed());
                Ok(Arc::new(provider))
            })
            .await
            .cloned()
    }
}

#[derive(Serialize)]
pub(crate) struct HealthResponse {
    status: &'static str,
    chat_requests_so_far: u64,
}

pub(crate) async fn healthz(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        chat_requests_so_far: state.chat_request_count.load(Ordering::Relaxed),
    })
}

pub(crate) async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    match token {
        Some(t) if t == state.bearer_token => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

// =========================================================================
// /v1/embeddings
// =========================================================================

/// OpenAI-compatible embeddings request body.
///
/// `input` is intentionally a `serde_json::Value` so we can accept either a
/// single string (OpenAI's canonical shape) or an array of strings (also
/// canonical) without two separate handlers. `input_type` is a Cohere
/// extension we map to [`EmbedKind`] so e5-family prefixes flow through.
#[derive(Deserialize)]
pub(crate) struct EmbeddingsRequest {
    #[allow(dead_code)]
    model: String,
    input: serde_json::Value,
    #[serde(default)]
    input_type: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    encoding_format: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct EmbeddingsResponse {
    object: &'static str,
    data: Vec<EmbeddingData>,
    model: String,
    usage: Usage,
}

#[derive(Serialize)]
pub(crate) struct EmbeddingData {
    object: &'static str,
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Serialize)]
pub(crate) struct Usage {
    prompt_tokens: u32,
    total_tokens: u32,
}

/// Real `/v1/embeddings` handler.
///
/// Accepts the OpenAI request shape, dispatches to the lazily-initialized
/// [`FastEmbedProvider`] via the kind-aware [`EmbeddingProvider`] API, and
/// zero-pads the native 384-d e5-small vectors up to
/// [`GATEWAY_OUTPUT_DIM`] (= 512) so downstream consumers (e.g. gbrain
/// 0.40.8.1's openai recipe) accept the response. See 附录 B 决策 #7.
pub(crate) async fn embeddings(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EmbeddingsRequest>,
) -> Result<Json<EmbeddingsResponse>, (StatusCode, String)> {
    let inputs: Vec<String> = match req.input {
        serde_json::Value::String(s) => vec![s],
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "field `input` must be a string or array of strings".into(),
            ));
        }
    };

    if inputs.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "field `input` resolved to zero strings".into(),
        ));
    }

    // OpenAI itself doesn't distinguish query vs passage, but Cohere does
    // via `input_type`. We honor "search_query" -> Query and treat any
    // other value (including the OpenAI default of None and Cohere's
    // "search_document") as Passage.
    let kind = match req.input_type.as_deref() {
        Some("search_query") => EmbedKind::Query,
        _ => EmbedKind::Passage,
    };

    let embedder = state.embedder().await.map_err(|e| {
        tracing::error!("embedder init failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("embedder init failed: {e}"),
        )
    })?;

    let raw_vectors = embedder
        .embed_batch_kind(kind, &inputs)
        .await
        .map_err(|e| {
            tracing::error!("embed_batch_kind failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("embedding error: {e}"),
            )
        })?;

    // Sanity check: native dim should match what the provider advertises
    // (384 for e5-small). A mismatch points at a model/config drift.
    let native_dim = raw_vectors.first().map(|v| v.len()).unwrap_or(0);
    if native_dim != embedder.dimensions() {
        tracing::warn!(
            "embedder returned dim={native_dim}, expected {}",
            embedder.dimensions()
        );
    }

    let data: Vec<EmbeddingData> = raw_vectors
        .into_iter()
        .enumerate()
        .map(|(i, v)| EmbeddingData {
            object: "embedding",
            index: i,
            embedding: zero_pad_to_gateway_dim(v),
        })
        .collect();

    debug_assert!(
        data.iter().all(|d| d.embedding.len() == GATEWAY_OUTPUT_DIM),
        "all output vectors must be padded to GATEWAY_OUTPUT_DIM"
    );

    Ok(Json(EmbeddingsResponse {
        object: "list",
        data,
        model: req.model,
        usage: Usage {
            prompt_tokens: 0,
            total_tokens: 0,
        },
    }))
}

// =========================================================================
// /v1/chat/completions
// =========================================================================

pub(crate) async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<OpenAIChatRequest>,
) -> Response {
    let req_idx = state.chat_request_count.fetch_add(1, Ordering::Relaxed) + 1;
    let stream = req.stream.unwrap_or(false);
    let model_for_chunks = req.model.clone();

    // Translate OpenAI -> Anthropic request shape.
    let mut anthr = openai_to_anthropic_request(&req);
    let original_model = anthr.model.clone();

    // Model remap (附录 B 决策 #25): some upstreams accept only a single
    // model name. The gateway is the abstraction layer where that mapping
    // happens. Driven by env var; empty = passthrough.
    if !state.upstream_model_override.is_empty()
        && anthr.model != state.upstream_model_override
    {
        tracing::debug!(
            req_idx,
            "remapping model {} -> {}",
            anthr.model,
            state.upstream_model_override
        );
        anthr.model = state.upstream_model_override.clone();
    }

    let url = format!("{}/v1/messages", state.upstream_base_url);
    tracing::info!(
        req_idx,
        stream,
        client_model = %original_model,
        upstream_model = %anthr.model,
        tool_count = anthr.tools.as_ref().map(|t| t.len()).unwrap_or(0),
        msg_count = anthr.messages.len(),
        "chat_completions -> upstream"
    );

    let upstream_body = match serde_json::to_vec(&anthr) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(req_idx, error = %e, "failed to encode upstream body");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "upstream encode failed"})),
            )
                .into_response();
        }
    };

    let response = match state
        .http
        .post(&url)
        .header("x-api-key", &state.upstream_api_key)
        .header("anthropic-version", &state.anthropic_version)
        .header("content-type", "application/json")
        .body(upstream_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(req_idx, error = %e, "upstream POST failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("upstream POST failed: {e}")})),
            )
                .into_response();
        }
    };

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        tracing::error!(req_idx, %status, body = %body, "upstream non-2xx");
        let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return (
            code,
            Json(serde_json::json!({
                "error": "upstream error",
                "status": status.as_u16(),
                "body": body,
            })),
        )
            .into_response();
    }

    if !stream {
        // Non-stream: read full body, parse, translate, return JSON.
        let body = match response.text().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(req_idx, error = %e, "failed to read upstream body");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": "read upstream body failed"})),
                )
                    .into_response();
            }
        };
        let anthr_resp: AnthropicResponse = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(req_idx, error = %e, raw = %body, "failed to parse anthropic response");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({
                        "error": format!("parse upstream failed: {e}"),
                        "raw": body,
                    })),
                )
                    .into_response();
            }
        };
        let mut openai = anthropic_to_openai_response(anthr_resp);
        // Echo back the client's original model name so transcripts stay
        // consistent and clients don't reject the response.
        openai.model = original_model.clone();
        tracing::info!(req_idx, "chat_completions ok (non-stream)");
        return Json(openai).into_response();
    }

    // Streaming: convert upstream byte stream -> Anthropic SSE events ->
    // OpenAI chunks -> outgoing SSE.
    let translator = StreamTranslator::new(model_for_chunks);
    let byte_stream = response.bytes_stream();
    let sse_stream = build_sse_stream(byte_stream, translator, req_idx);
    Sse::new(sse_stream)
        .keep_alive(KeepAlive::new())
        .into_response()
}

/// Build the outgoing SSE stream from upstream raw bytes + a translator.
///
/// Parses upstream `event: <T>\ndata: <JSON>\n\n` frames, feeds each
/// (event, data) into the translator, and emits OpenAI `data: {...}`
/// payloads (and a final `data: [DONE]`).
fn build_sse_stream<S>(
    mut byte_stream: S,
    mut translator: StreamTranslator,
    req_idx: u64,
) -> impl futures::Stream<Item = Result<Event, Infallible>>
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin + Send + 'static,
{
    async_stream::stream! {
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);

        loop {
            match byte_stream.next().await {
                Some(Ok(chunk)) => {
                    buf.extend_from_slice(&chunk);

                    // Process complete SSE frames (separated by blank line).
                    // A frame ends with "\n\n" or "\r\n\r\n".
                    loop {
                        let frame_end_idx = find_frame_end(&buf);
                        let Some(end) = frame_end_idx else { break };
                        let frame_bytes = buf.drain(..end.end).collect::<Vec<u8>>();
                        let frame_str = match std::str::from_utf8(&frame_bytes[..end.payload_len]) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };

                        let (event_type, data_json) = parse_sse_frame(frame_str);
                        if event_type.is_empty() && data_json.is_null() {
                            continue;
                        }

                        let chunks = translator.translate_event(&event_type, &data_json);
                        for ck in chunks {
                            match serde_json::to_string(&ck) {
                                Ok(s) => yield Ok(Event::default().data(s)),
                                Err(e) => {
                                    tracing::error!(req_idx, error=%e, "failed to encode chunk");
                                }
                            }
                        }

                        if translator.is_done() {
                            yield Ok(Event::default().data("[DONE]"));
                            return;
                        }
                    }
                }
                Some(Err(e)) => {
                    tracing::error!(req_idx, error=%e, "upstream stream error");
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
                None => {
                    if !translator.is_done() {
                        tracing::warn!(req_idx, "upstream stream ended without message_stop");
                    }
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
            }
        }
    }
}

struct FrameBounds {
    /// Length of the payload (text before the blank-line separator).
    payload_len: usize,
    /// Total number of bytes to drain from the buffer (payload + separator).
    end: usize,
}

/// Find the first `\n\n` or `\r\n\r\n` separator in `buf`. Returns the
/// payload length and total bytes-to-consume.
fn find_frame_end(buf: &[u8]) -> Option<FrameBounds> {
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(FrameBounds {
                payload_len: i,
                end: i + 4,
            });
        }
    }
    for i in 0..buf.len().saturating_sub(1) {
        if &buf[i..i + 2] == b"\n\n" {
            return Some(FrameBounds {
                payload_len: i,
                end: i + 2,
            });
        }
    }
    None
}

/// Parse a single SSE frame's payload (everything before the blank line)
/// into (event_type, data_json). Supports multi-line `data: ...` (concat
/// with newline per SSE spec), though Anthropic emits one-liners.
fn parse_sse_frame(frame: &str) -> (String, serde_json::Value) {
    let mut event_type = String::new();
    let mut data_buf = String::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest.trim_start());
        }
        // Ignore id:/retry:/comment lines.
    }
    let data = if data_buf.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&data_buf).unwrap_or(serde_json::Value::Null)
    };
    (event_type, data)
}
