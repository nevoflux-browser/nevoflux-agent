//! Library entrypoint for running the gateway in-process.
//!
//! Moved out of `main.rs` in M1 #010. The daemon can now build a
//! [`GatewayConfig`] in code and call [`serve`] to spawn the gateway as
//! a tokio task without shelling out to a child process. The standalone
//! binary in `src/main.rs` is now a thin Ctrl-C wrapper around this same
//! [`serve`] function.

use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use std::{
    net::SocketAddr,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};
use tokio::{net::TcpListener, sync::OnceCell, task::JoinHandle};

use crate::handlers::{self, AppState};

/// Default upstream base URL — canonical Anthropic API.
pub const DEFAULT_UPSTREAM_BASE: &str = "https://api.anthropic.com";

/// Default Anthropic API version header.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default loopback port used when running standalone via env vars.
pub const DEFAULT_PORT: u16 = 19501;

/// Default total budget for a non-stream upstream request (M2-3).
pub const DEFAULT_UPSTREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Default TCP/TLS connect budget (M2-3).
pub const DEFAULT_UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default per-chunk idle budget for streaming responses (M2-3).
///
/// If no chunk arrives from upstream within this window, the gateway
/// emits a final OpenAI-compatible error chunk + `[DONE]` and closes
/// the response cleanly.
pub const DEFAULT_UPSTREAM_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum `Retry-After` we'll honor before giving up and propagating
/// the 429 directly to the client (M2-3). Anything longer is the
/// client's problem to schedule.
pub const DEFAULT_UPSTREAM_RETRY_MAX_WAIT: Duration = Duration::from_secs(5);

/// Configuration for a single gateway instance.
///
/// Built either from environment variables ([`GatewayConfig::from_env`])
/// when running as a standalone binary, or constructed directly by the
/// daemon in M1 #010+.
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    /// Address to bind the listener on. Loopback by convention; the
    /// daemon passes `127.0.0.1:<port>` or `127.0.0.1:0` to let the OS
    /// pick a free port.
    pub bind_addr: SocketAddr,
    /// Required: bearer token clients must present on `/v1/*`.
    pub bearer_token: String,
    /// Upstream base URL (e.g., `https://api.anthropic.com`).
    pub upstream_base_url: String,
    /// Upstream API key, sent as `x-api-key`. May be empty for boot —
    /// `/v1/chat/completions` will then fail upstream until a key is
    /// supplied.
    pub upstream_api_key: String,
    /// If `Some`, rewrites every incoming `model` field before hitting
    /// upstream (附录 B 决策 #25). `None` = passthrough.
    pub upstream_model_remap: Option<String>,
    /// Value of the `anthropic-version` request header.
    pub anthropic_version: String,
    /// Total budget for a non-stream upstream request (TCP/TLS + headers
    /// + body). Wired to `reqwest::ClientBuilder::timeout` on the
    /// non-stream client. Default: 60s.
    pub upstream_request_timeout: Duration,
    /// TCP/TLS connect budget for both clients. Default: 10s.
    pub upstream_connect_timeout: Duration,
    /// Per-chunk idle budget for streaming responses. Enforced manually
    /// via `tokio::time::timeout` around each chunk read, because the
    /// client-level `timeout()` is total-budget and would cap the whole
    /// stream lifetime. Default: 60s.
    pub upstream_stream_idle_timeout: Duration,
    /// Maximum `Retry-After` we'll honor on a 429 before giving up and
    /// propagating the 429 to our client. Past this, the client should
    /// handle the longer wait itself. Default: 5s.
    pub upstream_retry_max_wait: Duration,
}

impl GatewayConfig {
    /// Build a [`GatewayConfig`] from the historical env vars used by the
    /// standalone binary. Kept around so existing dev workflows
    /// (`cargo run -p nevoflux-llm-gateway`) keep working.
    pub fn from_env() -> anyhow::Result<Self> {
        let bearer_token = match std::env::var("NEVOFLUX_LLM_GATEWAY_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                anyhow::bail!(
                    "NEVOFLUX_LLM_GATEWAY_TOKEN must be set (refusing to start with no bearer token)"
                );
            }
        };

        let upstream_api_key =
            std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY").unwrap_or_default();
        let upstream_base_url = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_UPSTREAM_BASE.to_string());
        let upstream_model_remap = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_MODEL")
            .ok()
            .filter(|s| !s.is_empty());
        let anthropic_version = std::env::var("NEVOFLUX_LLM_GATEWAY_ANTHROPIC_VERSION")
            .unwrap_or_else(|_| DEFAULT_ANTHROPIC_VERSION.to_string());

        let port: u16 = std::env::var("NEVOFLUX_LLM_GATEWAY_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        let bind_addr = SocketAddr::from(([127, 0, 0, 1], port));

        // M2-3: timeout knobs — all overridable via env vars, all
        // expressed in whole seconds for simplicity.
        let upstream_request_timeout = env_duration_secs(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_REQUEST_TIMEOUT_SECS",
            DEFAULT_UPSTREAM_REQUEST_TIMEOUT,
        );
        let upstream_connect_timeout = env_duration_secs(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_CONNECT_TIMEOUT_SECS",
            DEFAULT_UPSTREAM_CONNECT_TIMEOUT,
        );
        let upstream_stream_idle_timeout = env_duration_secs(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS",
            DEFAULT_UPSTREAM_STREAM_IDLE_TIMEOUT,
        );
        let upstream_retry_max_wait = env_duration_secs(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_RETRY_MAX_WAIT_SECS",
            DEFAULT_UPSTREAM_RETRY_MAX_WAIT,
        );

        if upstream_api_key.is_empty() {
            tracing::warn!(
                "NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY is unset — /v1/chat/completions will fail upstream"
            );
        }

        Ok(Self {
            bind_addr,
            bearer_token,
            upstream_base_url,
            upstream_api_key,
            upstream_model_remap,
            anthropic_version,
            upstream_request_timeout,
            upstream_connect_timeout,
            upstream_stream_idle_timeout,
            upstream_retry_max_wait,
        })
    }
}

/// Read a `Duration` from an env var holding whole-second integer text,
/// falling back to `default` if unset / unparsable. Helper for
/// [`GatewayConfig::from_env`].
fn env_duration_secs(name: &str, default: Duration) -> Duration {
    match std::env::var(name) {
        Ok(s) => match s.parse::<u64>() {
            Ok(n) => Duration::from_secs(n),
            Err(_) => {
                tracing::warn!(
                    "{name}={s:?} could not be parsed as u64 seconds, using default {:?}",
                    default
                );
                default
            }
        },
        Err(_) => default,
    }
}

/// Handle for a running gateway. Drop without calling [`Self::shutdown`]
/// gives only best-effort teardown via the underlying task abort path.
pub struct GatewayHandle {
    /// The address the listener was actually bound to. With
    /// `127.0.0.1:0` this is the OS-assigned port — read it back via
    /// [`Self::bind_addr`] / [`Self::url`].
    pub bind_addr: SocketAddr,
    /// Bearer token configured for this instance. Stored on the handle
    /// so the daemon can hand it to downstream consumers (gbrain in M3).
    pub bearer_token: String,
    join: JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl GatewayHandle {
    /// Return the canonical `http://<host>:<port>` URL for this gateway.
    pub fn url(&self) -> String {
        format!("http://{}", self.bind_addr)
    }

    /// Signal the server to stop, then await the task. Returns once the
    /// background task has fully ended.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.join.await;
    }
}

/// Build the axum router, bind the listener, and serve in the background.
///
/// Returns a [`GatewayHandle`] as soon as the listener is bound (so the
/// daemon can safely health-check `/healthz` immediately afterwards).
/// The actual `axum::serve(...)` call runs inside a spawned tokio task
/// with a graceful-shutdown channel held by the returned handle.
pub async fn serve(config: GatewayConfig) -> anyhow::Result<GatewayHandle> {
    // M2-3: two clients with different timeout shapes.
    //
    // - `nonstream_http` has both `connect_timeout` and `timeout()`, so a
    //   stuck non-stream request fails fast with a 504.
    // - `stream_http` has only `connect_timeout`. The total-request
    //   `timeout()` is too coarse for SSE (it would cap the whole stream
    //   lifetime); instead we enforce an idle timeout per chunk via
    //   `tokio::time::timeout` inside the streaming handler.
    let nonstream_http = reqwest::Client::builder()
        .connect_timeout(config.upstream_connect_timeout)
        .timeout(config.upstream_request_timeout)
        .build()?;
    let stream_http = reqwest::Client::builder()
        .connect_timeout(config.upstream_connect_timeout)
        .build()?;

    let bearer_token = config.bearer_token.clone();

    let state = Arc::new(AppState {
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
        embedder: OnceCell::new(),
    });

    let protected = Router::new()
        .route("/embeddings", post(handlers::embeddings))
        .route("/chat/completions", post(handlers::chat_completions))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::auth_middleware,
        ));

    let app = Router::new()
        .route("/healthz", get(handlers::healthz))
        .nest("/v1", protected)
        .with_state(state.clone());

    // Bind first so callers can read back the OS-assigned port (if any)
    // and start health-checking immediately.
    let listener = TcpListener::bind(config.bind_addr).await?;
    let bind_addr = listener.local_addr()?;
    tracing::info!(
        "nevoflux-llm-gateway listening on {bind_addr} (upstream={})",
        state.upstream_base_url
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let join = tokio::spawn(async move {
        let serve_fut = axum::serve(listener, app).with_graceful_shutdown(async move {
            // If the sender is dropped without sending, that's still
            // treated as a shutdown request — keeps the behavior safe
            // when the GatewayHandle is dropped without an explicit
            // shutdown() call.
            let _ = shutdown_rx.await;
        });
        if let Err(e) = serve_fut.await {
            tracing::error!(error = %e, "axum::serve exited with error");
        }
    });

    Ok(GatewayHandle {
        bind_addr,
        bearer_token,
        join,
        shutdown: shutdown_tx,
    })
}
