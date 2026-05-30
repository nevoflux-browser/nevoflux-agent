//! HTTP handlers for the gateway.
//!
//! Moved out of `main.rs` in M1 #010 so the daemon can construct the same
//! axum routes in-process. All items are `pub(crate)` and consumed only by
//! [`crate::server::serve`].

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
#[cfg(feature = "embedding")]
use nevoflux_llm::embedding::{EmbedKind, EmbeddingConfig, EmbeddingProvider, FastEmbedProvider};
#[cfg(feature = "embedding")]
use serde::Deserialize;
use serde::Serialize;
use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
#[cfg(feature = "embedding")]
use tokio::sync::OnceCell;

#[cfg(feature = "embedding")]
use crate::embedding_dim::{zero_pad_to_gateway_dim, GATEWAY_OUTPUT_DIM};
use crate::error::{GatewayError, TimeoutPhase};
use crate::protocol::UpstreamProtocol;
use crate::server::GatewayConfig;
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
    /// Client used for non-stream upstream calls. Has both
    /// `connect_timeout` and `timeout()` set so a stuck request fails
    /// fast with a 504.
    pub(crate) nonstream_http: reqwest::Client,
    /// Client used for streaming upstream calls. Only `connect_timeout`
    /// is set on the client — total-request `timeout()` would cap the
    /// whole stream lifetime, so per-chunk idle timeout is enforced via
    /// `tokio::time::timeout` inside the SSE handler instead.
    pub(crate) stream_http: reqwest::Client,
    /// Per-chunk idle budget for streaming responses (M2-3).
    pub(crate) upstream_stream_idle_timeout: Duration,
    /// Maximum `Retry-After` we'll sleep on a 429 retry before giving up.
    pub(crate) upstream_retry_max_wait: Duration,
    /// Lazily-initialized fastembed-backed embedder.
    ///
    /// Loading the ~120 MB ONNX weights is expensive, and gateways used
    /// only for chat-completions should not pay that cost. The cell is
    /// populated on first `/v1/embeddings` call via [`AppState::embedder`].
    #[cfg(feature = "embedding")]
    pub(crate) embedder: OnceCell<Arc<FastEmbedProvider>>,
    /// Models advertised by `GET /v1/models` (M2-1). The handler falls
    /// back to a single-entry list synthesized from
    /// `upstream_model_override` (or the sentinel `"default"`) when this
    /// is empty, so naive clients calling list-models on a freshly-booted
    /// gateway always get a valid response.
    pub(crate) advertised_models: Vec<String>,
    /// Protocol the upstream LLM endpoint speaks (M4-2.6). Dispatched on
    /// inside `chat_completions` to pick between the Anthropic translator
    /// path and the OpenAI passthrough path.
    pub(crate) upstream_protocol: UpstreamProtocol,
}

impl AppState {
    /// Build an [`AppState`] from a [`GatewayConfig`].
    ///
    /// Factored out of `serve()` so tests (and the `serve_test_router`
    /// helper) can construct exactly the same shared state without
    /// binding a TCP listener. The two reqwest clients are built here
    /// with the M2-3 timeout shapes.
    pub(crate) async fn new(config: GatewayConfig) -> anyhow::Result<Self> {
        // M2-3: two clients with different timeout shapes.
        //
        // - `nonstream_http` has both `connect_timeout` and `timeout()`,
        //   so a stuck non-stream request fails fast with a 504.
        // - `stream_http` has only `connect_timeout`. The total-request
        //   `timeout()` is too coarse for SSE (it would cap the whole
        //   stream lifetime); instead we enforce an idle timeout per
        //   chunk via `tokio::time::timeout` inside the streaming
        //   handler.
        let nonstream_http = reqwest::Client::builder()
            .connect_timeout(config.upstream_connect_timeout)
            .timeout(config.upstream_request_timeout)
            .build()?;
        let stream_http = reqwest::Client::builder()
            .connect_timeout(config.upstream_connect_timeout)
            .build()?;

        Ok(Self {
            bearer_token: config.bearer_token,
            chat_request_count: AtomicU64::new(0),
            upstream_base_url: config.upstream_base_url,
            upstream_api_key: config.upstream_api_key,
            upstream_model_override: config.upstream_model_remap.unwrap_or_default(),
            anthropic_version: config.anthropic_version,
            nonstream_http,
            stream_http,
            upstream_stream_idle_timeout: config.upstream_stream_idle_timeout,
            upstream_retry_max_wait: config.upstream_retry_max_wait,
            #[cfg(feature = "embedding")]
            embedder: OnceCell::new(),
            advertised_models: config.advertised_models,
            upstream_protocol: config.upstream_protocol,
        })
    }

    /// Return the shared [`FastEmbedProvider`], initializing it on the
    /// first call. Subsequent calls reuse the cached instance.
    ///
    /// The `FastEmbedProvider::new` constructor is synchronous and can
    /// load model weights from disk, so we wrap it in `spawn_blocking`
    /// to avoid stalling the async runtime.
    #[cfg(feature = "embedding")]
    async fn embedder(&self) -> anyhow::Result<Arc<FastEmbedProvider>> {
        self.embedder
            .get_or_try_init(|| async {
                tracing::info!("initializing FastEmbedProvider (first /v1/embeddings call)");
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

/// Assemble the axum [`Router`] for this gateway from a constructed
/// [`AppState`]. Factored out of `serve()` so tests can drive the same
/// router via `tower::ServiceExt::oneshot` without binding a listener.
pub(crate) fn build_router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/embeddings", post(embeddings))
        .route("/chat/completions", post(chat_completions))
        .route("/models", get(models))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", protected)
        .with_state(state)
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
#[cfg(feature = "embedding")]
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

#[cfg(feature = "embedding")]
#[derive(Serialize)]
pub(crate) struct EmbeddingsResponse {
    object: &'static str,
    data: Vec<EmbeddingData>,
    model: String,
    usage: Usage,
}

#[cfg(feature = "embedding")]
#[derive(Serialize)]
pub(crate) struct EmbeddingData {
    object: &'static str,
    index: usize,
    embedding: Vec<f32>,
}

#[cfg(feature = "embedding")]
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
#[cfg(feature = "embedding")]
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

/// Fallback `/v1/embeddings` handler for builds compiled without the
/// `embedding` feature (e.g. aarch64 release builds where `ort` can't
/// link). Returns 503 with an OpenAI-shaped error envelope so callers
/// can distinguish "this build can't embed" from a transient failure.
#[cfg(not(feature = "embedding"))]
pub(crate) async fn embeddings() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": {
                "type": "embedding_unavailable",
                "message": "this build was compiled without the embedding feature"
            }
        })),
    )
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

    // M4-2.6: dispatch on the upstream protocol. The Anthropic path is
    // the existing M2 translator; the OpenAI path is a thin passthrough
    // that swaps auth + applies the optional model remap and reuses the
    // same M2-3 retry/timeout/error-classification helpers.
    let result = match state.upstream_protocol {
        UpstreamProtocol::Anthropic => {
            do_chat_completions_anthropic(state.clone(), req, req_idx, stream).await
        }
        UpstreamProtocol::OpenAi => {
            do_chat_completions_openai(state.clone(), req, req_idx, stream).await
        }
    };

    match result {
        Ok(response) => response,
        Err(e) => {
            // M2-3: log at severity appropriate to the failure class.
            // Server-side problems (upstream 5xx, our own bugs) are
            // errors; everything else (timeouts, 4xx, 429) is a warn.
            match &e {
                GatewayError::Internal { .. } | GatewayError::UpstreamServerError { .. } => {
                    tracing::error!(req_idx, error = ?e, "chat_completions failed");
                }
                _ => {
                    tracing::warn!(req_idx, error = ?e, "chat_completions failed");
                }
            }
            error_to_response(&e)
        }
    }
}

/// Inner chat-completions implementation for the Anthropic translator
/// path. Translates OpenAI request → Anthropic Messages API, dispatches
/// upstream via [`post_upstream`], then translates the response back.
async fn do_chat_completions_anthropic(
    state: Arc<AppState>,
    req: OpenAIChatRequest,
    req_idx: u64,
    stream: bool,
) -> Result<Response, GatewayError> {
    let model_for_chunks = req.model.clone();

    // Translate OpenAI -> Anthropic request shape.
    let mut anthr = openai_to_anthropic_request(&req);
    let original_model = anthr.model.clone();

    // Model remap (附录 B 决策 #25): some upstreams accept only a single
    // model name. The gateway is the abstraction layer where that mapping
    // happens. Driven by env var; empty = passthrough.
    if !state.upstream_model_override.is_empty() && anthr.model != state.upstream_model_override {
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
        upstream = "anthropic",
        client_model = %original_model,
        upstream_model = %anthr.model,
        tool_count = anthr.tools.as_ref().map(|t| t.len()).unwrap_or(0),
        msg_count = anthr.messages.len(),
        "chat_completions -> upstream"
    );

    let upstream_body = serde_json::to_vec(&anthr).map_err(|e| GatewayError::Internal {
        detail: format!("upstream body encode failed: {e}"),
    })?;

    // Pick the right reqwest client: streaming needs no total-request
    // timeout (we enforce a per-chunk idle timeout manually instead).
    let client = if stream {
        &state.stream_http
    } else {
        &state.nonstream_http
    };

    let mut anthropic_headers = reqwest::header::HeaderMap::new();
    if let Ok(hv) = reqwest::header::HeaderValue::from_str(&state.upstream_api_key) {
        anthropic_headers.insert("x-api-key", hv);
    }
    if let Ok(hv) = reqwest::header::HeaderValue::from_str(&state.anthropic_version) {
        anthropic_headers.insert("anthropic-version", hv);
    }
    anthropic_headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    let response = post_upstream(
        client,
        &url,
        upstream_body,
        anthropic_headers,
        state.upstream_retry_max_wait,
    )
    .await?;

    if !stream {
        // Non-stream: read full body, parse, translate, return JSON.
        let body = response
            .text()
            .await
            .map_err(|e| reqwest_to_gateway_error(&e))?;
        let anthr_resp: AnthropicResponse =
            serde_json::from_str(&body).map_err(|e| GatewayError::Internal {
                detail: format!("parse upstream response failed: {e}"),
            })?;
        let mut openai = anthropic_to_openai_response(anthr_resp);
        // Echo back the client's original model name so transcripts stay
        // consistent and clients don't reject the response.
        openai.model = original_model.clone();
        tracing::info!(req_idx, "chat_completions ok (non-stream)");
        return Ok(Json(openai).into_response());
    }

    // Streaming: convert upstream byte stream -> Anthropic SSE events ->
    // OpenAI chunks -> outgoing SSE.
    let translator = StreamTranslator::new(model_for_chunks);
    let byte_stream = response.bytes_stream();
    let idle_timeout = state.upstream_stream_idle_timeout;
    let sse_stream = build_sse_stream(byte_stream, translator, req_idx, idle_timeout);
    Ok(Sse::new(sse_stream)
        .keep_alive(KeepAlive::new())
        .into_response())
}

/// Inner chat-completions implementation for the OpenAI passthrough
/// path (M4-2.6). Forwards the client request unchanged except for:
///
/// - applying [`AppState::upstream_model_override`] to the `model` field;
/// - swapping auth to `Authorization: Bearer <upstream_api_key>`;
/// - reusing M2-3's [`post_upstream`] for 429-retry + timeout + error
///   classification;
/// - re-emitting the upstream SSE stream verbatim (with per-chunk idle
///   timeout) instead of running the Anthropic translator.
async fn do_chat_completions_openai(
    state: Arc<AppState>,
    mut req: OpenAIChatRequest,
    req_idx: u64,
    stream: bool,
) -> Result<Response, GatewayError> {
    let original_model = req.model.clone();

    // Model remap — same semantics as the Anthropic path, just applied
    // directly to the OpenAI request body before we forward it.
    if !state.upstream_model_override.is_empty() && req.model != state.upstream_model_override {
        tracing::debug!(
            req_idx,
            "remapping model {} -> {}",
            req.model,
            state.upstream_model_override
        );
        req.model = state.upstream_model_override.clone();
    }

    let url = format!(
        "{}/v1/chat/completions",
        state.upstream_base_url.trim_end_matches('/')
    );
    tracing::info!(
        req_idx,
        stream,
        upstream = "openai",
        client_model = %original_model,
        upstream_model = %req.model,
        msg_count = req.messages.len(),
        "chat_completions -> upstream"
    );

    let upstream_body = serde_json::to_vec(&req).map_err(|e| GatewayError::Internal {
        detail: format!("upstream body encode failed: {e}"),
    })?;

    let client = if stream {
        &state.stream_http
    } else {
        &state.nonstream_http
    };

    let mut openai_headers = reqwest::header::HeaderMap::new();
    if let Ok(hv) =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", state.upstream_api_key))
    {
        openai_headers.insert(reqwest::header::AUTHORIZATION, hv);
    }
    openai_headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    let response = post_upstream(
        client,
        &url,
        upstream_body,
        openai_headers,
        state.upstream_retry_max_wait,
    )
    .await?;

    if !stream {
        // Non-stream: forward the upstream OpenAI JSON body verbatim. The
        // upstream is already OpenAI-shape so we don't need to parse it.
        let bytes = response.bytes().await.map_err(|e| GatewayError::Internal {
            detail: format!("read upstream body: {e}"),
        })?;
        tracing::info!(req_idx, "chat_completions ok (non-stream)");
        let resp = axum::http::Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(bytes))
            .map_err(|e| GatewayError::Internal {
                detail: format!("build response: {e}"),
            })?;
        return Ok(resp);
    }

    // Streaming: re-emit upstream SSE frames verbatim, with the same
    // M2-3 per-chunk idle-timeout and mid-stream error-wrapping
    // contract as the Anthropic path.
    let byte_stream = response.bytes_stream();
    let idle_timeout = state.upstream_stream_idle_timeout;
    let sse_stream = build_openai_passthrough_sse_stream(byte_stream, req_idx, idle_timeout);
    Ok(Sse::new(sse_stream)
        .keep_alive(KeepAlive::new())
        .into_response())
}

/// Render a [`GatewayError`] into an axum [`Response`] with the right
/// status code, the OpenAI-compatible error envelope, and (for 429s) a
/// `Retry-After` header echo.
fn error_to_response(e: &GatewayError) -> Response {
    let status = e.status_code();
    let body = e.to_openai_body();
    let mut resp = (status, Json(body)).into_response();
    if let GatewayError::RateLimited {
        retry_after: Some(d),
        ..
    } = e
    {
        let secs = d.as_secs();
        if let Ok(hv) = HeaderValue::from_str(&secs.to_string()) {
            resp.headers_mut().insert("Retry-After", hv);
        }
    }
    resp
}

/// Send the upstream POST with one polite 429 retry.
///
/// On a 429 with `Retry-After` ≤ `retry_max_wait`, sleeps and retries
/// once. Any other 429 (no `Retry-After`, or beyond max-wait) is
/// propagated immediately so the client can schedule its own backoff.
///
/// The `headers` map is protocol-agnostic: the Anthropic path passes
/// `x-api-key` + `anthropic-version`, the OpenAI passthrough path passes
/// `Authorization: Bearer ...`. Both include `content-type`.
async fn post_upstream(
    client: &reqwest::Client,
    url: &str,
    body: Vec<u8>,
    headers: reqwest::header::HeaderMap,
    retry_max_wait: Duration,
) -> Result<reqwest::Response, GatewayError> {
    let send = || async {
        client
            .post(url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await
    };

    let first = send().await.map_err(|e| reqwest_to_gateway_error(&e))?;
    if first.status().as_u16() != 429 {
        return classify_response(first).await;
    }

    // 429 path — check Retry-After. We MUST drain the headers + body
    // before borrowing the response (else we drop it).
    let retry_after = parse_retry_after(first.headers());
    let body_text = first.text().await.unwrap_or_default();

    match retry_after {
        Some(d) if d <= retry_max_wait => {
            tracing::info!(retry_after_ms = d.as_millis() as u64, "polite 429 retry");
            tokio::time::sleep(d).await;
            let second = send().await.map_err(|e| reqwest_to_gateway_error(&e))?;
            if second.status().as_u16() == 429 {
                let retry_after2 = parse_retry_after(second.headers());
                let body_text2 = second.text().await.unwrap_or_default();
                Err(GatewayError::RateLimited {
                    retry_after: retry_after2,
                    upstream_body: body_text2,
                })
            } else {
                classify_response(second).await
            }
        }
        // No Retry-After, or it exceeds our max — give up immediately
        // and propagate. Client handles longer waits.
        _ => Err(GatewayError::RateLimited {
            retry_after,
            upstream_body: body_text,
        }),
    }
}

/// Parse a `Retry-After` header value. Accepts either delta-seconds
/// (`"5"`) or an HTTP-date (`"Wed, 21 Oct 2015 07:28:00 GMT"`) per
/// RFC 7231 §7.1.3. Returns `None` if the header is missing or
/// unparsable, or if the HTTP-date is in the past.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers.get("retry-after")?.to_str().ok()?;
    let trimmed = v.trim();

    // Try delta-seconds first — by far the most common form.
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }

    // Fall through to HTTP-date format.
    if let Ok(dt) = httpdate::parse_http_date(trimmed) {
        let now = std::time::SystemTime::now();
        return dt.duration_since(now).ok();
    }

    None
}

/// Inspect a `reqwest::Response` and either return it (2xx) or convert
/// the upstream's non-2xx status into the appropriate [`GatewayError`].
async fn classify_response(resp: reqwest::Response) -> Result<reqwest::Response, GatewayError> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    match status {
        429 => Err(GatewayError::RateLimited {
            retry_after: None,
            upstream_body: body,
        }),
        s if (500..600).contains(&s) => Err(GatewayError::UpstreamServerError {
            upstream_status: s,
            upstream_body: body,
        }),
        s => Err(GatewayError::UpstreamClientError {
            upstream_status: s,
            upstream_body: body,
        }),
    }
}

/// Convert a `reqwest::Error` into a [`GatewayError`]. Connection
/// failures map to `UpstreamUnreachable`, timeouts to `UpstreamTimeout`,
/// everything else (decode failures, body errors) to `Internal`.
fn reqwest_to_gateway_error(e: &reqwest::Error) -> GatewayError {
    if e.is_timeout() {
        // `reqwest::Error::is_timeout()` is true for both connect and
        // request timeouts; we can't reliably distinguish without
        // walking the source chain, so report as `Request`.
        GatewayError::UpstreamTimeout {
            phase: TimeoutPhase::Request,
        }
    } else if e.is_connect() {
        GatewayError::UpstreamUnreachable {
            detail: e.to_string(),
        }
    } else {
        GatewayError::Internal {
            detail: e.to_string(),
        }
    }
}

/// Build the outgoing SSE stream from upstream raw bytes + a translator.
///
/// Parses upstream `event: <T>\ndata: <JSON>\n\n` frames, feeds each
/// (event, data) into the translator, and emits OpenAI `data: {...}`
/// payloads (and a final `data: [DONE]`).
///
/// M2-3 hardening:
/// - Each `byte_stream.next().await` is wrapped in
///   `tokio::time::timeout(idle_timeout, ...)`. If no chunk arrives
///   inside that window, emit an OpenAI-compatible error chunk + DONE
///   and close cleanly.
/// - On `Err(reqwest_err)` from the byte stream (drop, TLS reset,
///   etc.), emit the same shape of error chunk + DONE and close.
fn build_sse_stream<S>(
    mut byte_stream: S,
    mut translator: StreamTranslator,
    req_idx: u64,
    idle_timeout: Duration,
) -> impl futures::Stream<Item = Result<Event, Infallible>>
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin + Send + 'static,
{
    async_stream::stream! {
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);

        loop {
            let next = tokio::time::timeout(idle_timeout, byte_stream.next()).await;
            match next {
                Ok(Some(Ok(chunk))) => {
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

                        // Upstream `event: error` frames carry a JSON
                        // error payload; surface them as a final
                        // OpenAI-compatible error chunk + DONE.
                        if event_type == "error" {
                            tracing::error!(
                                req_idx,
                                payload = %data_json,
                                "upstream emitted SSE error event"
                            );
                            let err_msg = data_json
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(|m| m.as_str())
                                .unwrap_or("upstream sent error event")
                                .to_string();
                            let err_type = data_json
                                .get("error")
                                .and_then(|e| e.get("type"))
                                .and_then(|t| t.as_str())
                                .unwrap_or("upstream_error")
                                .to_string();
                            let err_payload = serde_json::json!({
                                "error": {
                                    "type": err_type,
                                    "message": err_msg,
                                }
                            });
                            yield Ok(Event::default().data(err_payload.to_string()));
                            yield Ok(Event::default().data("[DONE]"));
                            return;
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
                Ok(Some(Err(e))) => {
                    // M2-3: mid-stream upstream error. Surface it as a
                    // final OpenAI-compatible error chunk so the client
                    // sees *something* useful instead of a silently
                    // truncated stream.
                    tracing::error!(req_idx, error=%e, "upstream stream errored mid-flight");
                    let err_payload = serde_json::json!({
                        "error": {
                            "type": "upstream_stream_error",
                            "message": e.to_string(),
                        }
                    });
                    yield Ok(Event::default().data(err_payload.to_string()));
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
                Ok(None) => {
                    if !translator.is_done() {
                        tracing::warn!(req_idx, "upstream stream ended without message_stop");
                    }
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
                Err(_elapsed) => {
                    // M2-3: idle timeout fired — no chunk arrived inside
                    // the per-chunk window. Emit error + DONE and bail.
                    tracing::warn!(
                        req_idx,
                        idle_timeout_secs = idle_timeout.as_secs(),
                        "upstream stream idle timeout fired"
                    );
                    let err_payload = serde_json::json!({
                        "error": {
                            "type": "upstream_stream_idle_timeout",
                            "message": format!(
                                "no chunk received in {}s",
                                idle_timeout.as_secs()
                            ),
                        }
                    });
                    yield Ok(Event::default().data(err_payload.to_string()));
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
            }
        }
    }
}

/// Build the outgoing SSE stream for the OpenAI passthrough path
/// (M4-2.6). The upstream is already emitting OpenAI-shape
/// `data: {...}` chunks + a final `data: [DONE]`, so we just re-emit
/// each `data:` line as an axum [`Event`] unchanged. Non-`data:` lines
/// (`event:`, `id:`, `retry:`, `:` comments) are stripped because the
/// SSE event we emit downstream is opaque to OpenAI clients —
/// `Event::default().data(...)` always renders as `data: <payload>\n\n`.
///
/// Same M2-3 hardening as the Anthropic path:
/// - Each chunk read is wrapped in `tokio::time::timeout(idle_timeout, ...)`.
/// - Mid-stream upstream errors emit a final OpenAI-shaped error envelope
///   + `[DONE]` and close cleanly.
fn build_openai_passthrough_sse_stream<S>(
    mut byte_stream: S,
    req_idx: u64,
    idle_timeout: Duration,
) -> impl futures::Stream<Item = Result<Event, Infallible>>
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin + Send + 'static,
{
    async_stream::stream! {
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut saw_done = false;

        loop {
            let next = tokio::time::timeout(idle_timeout, byte_stream.next()).await;
            match next {
                Ok(Some(Ok(chunk))) => {
                    buf.extend_from_slice(&chunk);

                    loop {
                        let frame_end_idx = find_frame_end(&buf);
                        let Some(end) = frame_end_idx else { break };
                        let frame_bytes = buf.drain(..end.end).collect::<Vec<u8>>();
                        let frame_str = match std::str::from_utf8(&frame_bytes[..end.payload_len]) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };

                        // Re-emit every `data:` line as its own SSE event.
                        // OpenAI clients only care about `data:` payloads;
                        // `event:` / `id:` / `retry:` lines are not part
                        // of the Chat Completions wire format.
                        for line in frame_str.lines() {
                            if let Some(rest) = line.strip_prefix("data:") {
                                let payload = rest.trim_start();
                                if payload.is_empty() {
                                    continue;
                                }
                                if payload == "[DONE]" {
                                    saw_done = true;
                                }
                                yield Ok(Event::default().data(payload));
                            }
                        }

                        if saw_done {
                            return;
                        }
                    }
                }
                Ok(Some(Err(e))) => {
                    tracing::error!(req_idx, error=%e, "upstream stream errored mid-flight");
                    let err_payload = serde_json::json!({
                        "error": {
                            "type": "upstream_stream_error",
                            "message": e.to_string(),
                        }
                    });
                    yield Ok(Event::default().data(err_payload.to_string()));
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
                Ok(None) => {
                    if !saw_done {
                        tracing::warn!(req_idx, "upstream stream ended without [DONE]");
                        yield Ok(Event::default().data("[DONE]"));
                    }
                    return;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        req_idx,
                        idle_timeout_secs = idle_timeout.as_secs(),
                        "upstream stream idle timeout fired"
                    );
                    let err_payload = serde_json::json!({
                        "error": {
                            "type": "upstream_stream_idle_timeout",
                            "message": format!(
                                "no chunk received in {}s",
                                idle_timeout.as_secs()
                            ),
                        }
                    });
                    yield Ok(Event::default().data(err_payload.to_string()));
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

// =========================================================================
// /v1/models  (M2-1)
// =========================================================================

/// OpenAI-standard `GET /v1/models` envelope.
#[derive(Serialize)]
pub(crate) struct ModelsList {
    object: &'static str,
    data: Vec<ModelEntry>,
}

/// One entry in the `data` array returned by `GET /v1/models`.
#[derive(Serialize)]
pub(crate) struct ModelEntry {
    id: String,
    object: &'static str,
    /// Unix-seconds timestamp. Synthesized so the value is stable across
    /// boots — clients sometimes cache by `(id, created)` and we don't
    /// want the cache to bust every restart.
    created: u64,
    owned_by: &'static str,
}

/// `GET /v1/models` — OpenAI-standard model discovery.
///
/// Returns the list configured via [`AppState::advertised_models`]. If
/// empty, returns a single entry derived from
/// [`AppState::upstream_model_override`] (if set), otherwise a sentinel
/// `"default"` placeholder so naive clients calling list-models on a
/// freshly-booted gateway always get a valid response.
///
/// The `created` field is a fixed epoch plus a 1-second offset per
/// entry — neither correctness nor freshness matters for OpenAI clients
/// here, only that the value is stable.
pub(crate) async fn models(State(state): State<Arc<AppState>>) -> Json<ModelsList> {
    /// 2024-10-22, an unremarkable past date. Stable across boots so
    /// clients caching by `(id, created)` see the same value.
    const EPOCH: u64 = 1_729_600_000;

    let models: Vec<String> = if !state.advertised_models.is_empty() {
        state.advertised_models.clone()
    } else if !state.upstream_model_override.is_empty() {
        vec![state.upstream_model_override.clone()]
    } else {
        vec!["default".to_string()]
    };

    let data = models
        .into_iter()
        .enumerate()
        .map(|(i, id)| ModelEntry {
            id,
            object: "model",
            created: EPOCH + i as u64,
            owned_by: "nevoflux-gateway",
        })
        .collect();

    Json(ModelsList {
        object: "list",
        data,
    })
}
