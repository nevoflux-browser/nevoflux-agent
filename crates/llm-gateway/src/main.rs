//! `nevoflux-llm-gateway` binary.
//!
//! A loopback-only HTTP server that presents an OpenAI-compatible API on
//! the public side and translates to Anthropic Messages on the upstream
//! side. Designed to be spawned as a child process by the nevoflux daemon
//! (wiring lands in M1 #010).
//!
//! ## Routes
//!
//! * `GET  /healthz`             — un-authed liveness probe.
//! * `POST /v1/chat/completions` — bearer-authed; OpenAI ChatCompletions.
//! * `POST /v1/embeddings`       — bearer-authed; OpenAI-shaped, backed
//!   by `nevoflux-llm`'s [`FastEmbedProvider`] (M1 #008). Native 384-d
//!   e5-small vectors are zero-padded to 512 (附录 B 决策 #7).
//!
//! ## Configuration (environment variables)
//!
//! | Name                                   | Default               | Notes |
//! |----------------------------------------|-----------------------|-------|
//! | `NEVOFLUX_LLM_GATEWAY_PORT`            | `19501`               | bind port |
//! | `NEVOFLUX_LLM_GATEWAY_TOKEN`           | *(required)*          | bearer token for `/v1/*` |
//! | `NEVOFLUX_LLM_GATEWAY_UPSTREAM_BASE_URL` | `https://api.anthropic.com` | upstream Anthropic-compatible host |
//! | `NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY` | *(required for chat)* | passed as `x-api-key` |
//! | `NEVOFLUX_LLM_GATEWAY_UPSTREAM_MODEL`  | *(empty = no remap)*  | if set, rewrites incoming `model` field before upstream call (附录 B 决策 #25) |
//! | `NEVOFLUX_LLM_GATEWAY_ANTHROPIC_VERSION` | `2023-06-01`        | `anthropic-version` request header |
//!
//! These names are stable for M1 #003; M1 #010 will canonicalize them
//! through a proper config struct.
//!
//! See `docs/plans/2026-05-24-knowledge-base-spike-plan.md` 附录 B for
//! the gate-C validation results behind the model-remap (#25) and
//! permissive-enum (#26) decisions implemented here / in `translate.rs`.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use nevoflux_llm::embedding::{EmbedKind, EmbeddingConfig, EmbeddingProvider, FastEmbedProvider};
use nevoflux_llm_gateway::embedding_dim::{zero_pad_to_gateway_dim, GATEWAY_OUTPUT_DIM};
use nevoflux_llm_gateway::translate::{
    anthropic_to_openai_response, openai_to_anthropic_request, AnthropicResponse,
    OpenAIChatRequest, StreamTranslator,
};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{net::TcpListener, sync::OnceCell};

/// Default upstream base URL — canonical Anthropic API.
const DEFAULT_UPSTREAM_BASE: &str = "https://api.anthropic.com";

/// Default Anthropic API version header.
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default loopback port.
const DEFAULT_PORT: u16 = 19501;

struct AppState {
    bearer_token: String,
    chat_request_count: AtomicU64,
    upstream_base_url: String,
    upstream_api_key: String,
    /// If non-empty, overrides the `model` field of every incoming
    /// chat-completions request before hitting upstream. See 附录 B 决策 #25.
    upstream_model_override: String,
    anthropic_version: String,
    http: reqwest::Client,
    /// Lazily-initialized fastembed-backed embedder.
    ///
    /// Loading the ~120 MB ONNX weights is expensive, and gateways used
    /// only for chat-completions should not pay that cost. The cell is
    /// populated on first `/v1/embeddings` call via [`AppState::embedder`].
    embedder: OnceCell<Arc<FastEmbedProvider>>,
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
struct HealthResponse {
    status: &'static str,
    chat_requests_so_far: u64,
}

async fn healthz(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        chat_requests_so_far: state.chat_request_count.load(Ordering::Relaxed),
    })
}

async fn auth_middleware(
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
struct EmbeddingsRequest {
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
struct EmbeddingsResponse {
    object: &'static str,
    data: Vec<EmbeddingData>,
    model: String,
    usage: Usage,
}

#[derive(Serialize)]
struct EmbeddingData {
    object: &'static str,
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Serialize)]
struct Usage {
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
async fn embeddings(
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

async fn chat_completions(
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nevoflux_llm_gateway=info,tower_http=info".into()),
        )
        .init();

    let bearer_token = match std::env::var("NEVOFLUX_LLM_GATEWAY_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            anyhow::bail!(
                "NEVOFLUX_LLM_GATEWAY_TOKEN must be set (refusing to start with no bearer token)"
            );
        }
    };

    let upstream_api_key = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY")
        .unwrap_or_default();
    let upstream_base_url = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_UPSTREAM_BASE.to_string());
    let upstream_model_override = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_MODEL")
        .unwrap_or_default();
    let anthropic_version = std::env::var("NEVOFLUX_LLM_GATEWAY_ANTHROPIC_VERSION")
        .unwrap_or_else(|_| DEFAULT_ANTHROPIC_VERSION.to_string());

    if upstream_api_key.is_empty() {
        tracing::warn!(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY is unset — /v1/chat/completions will fail upstream"
        );
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let state = Arc::new(AppState {
        bearer_token,
        chat_request_count: AtomicU64::new(0),
        upstream_base_url,
        upstream_api_key,
        upstream_model_override,
        anthropic_version,
        http,
        embedder: OnceCell::new(),
    });

    let protected = Router::new()
        .route("/embeddings", post(embeddings))
        .route("/chat/completions", post(chat_completions))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", protected)
        .with_state(state.clone());

    let port: u16 = std::env::var("NEVOFLUX_LLM_GATEWAY_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(
        "nevoflux-llm-gateway listening on {addr} (upstream={})",
        state.upstream_base_url
    );
    axum::serve(listener, app).await?;
    Ok(())
}
