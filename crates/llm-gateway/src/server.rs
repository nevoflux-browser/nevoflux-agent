//! Library entrypoint for running the gateway in-process.
//!
//! Moved out of `main.rs` in M1 #010. The daemon can now build a
//! [`GatewayConfig`] in code and call [`serve`] to spawn the gateway as
//! a tokio task without shelling out to a child process. The standalone
//! binary in `src/main.rs` is now a thin Ctrl-C wrapper around this same
//! [`serve`] function.

use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle};

use crate::handlers::{self, AppState};
use crate::protocol::UpstreamProtocol;
use nevoflux_llm::providers::acp::AcpProviderConfig;

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
    /// Models advertised by `GET /v1/models` (M2-1).
    ///
    /// The daemon side is expected to compute a non-empty list before
    /// constructing this struct (using `upstream_model_remap` or the
    /// sentinel `"default"` as fallback). The standalone binary path
    /// (see [`GatewayConfig::from_env`]) reads
    /// `NEVOFLUX_LLM_GATEWAY_ADVERTISED_MODELS` as a comma-separated
    /// list. If still empty, the handler synthesizes a fallback at
    /// request time.
    pub advertised_models: Vec<String>,
    /// Protocol the upstream LLM endpoint speaks (M4-2.6). Determines
    /// whether `chat_completions` runs the OpenAI ↔ Anthropic translator
    /// path (existing M2 behavior) or forwards the request unchanged to
    /// an OpenAI-compatible upstream. Defaults to
    /// [`UpstreamProtocol::Anthropic`] for back-compat.
    pub upstream_protocol: UpstreamProtocol,
    /// ACP agent config used when `upstream_protocol == Acp`. `None` for
    /// every other protocol. Supplied by the daemon (built from
    /// `nevoflux_llm::providers::acp::claude::build_config`). The gateway
    /// lazily spawns this subprocess on the first `Acp` chat request.
    pub acp_config: Option<AcpProviderConfig>,
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

        // M2-1: comma-separated list of models for `GET /v1/models`.
        // Empty entries are dropped; an entirely missing/empty env var
        // yields an empty Vec and the handler synthesizes a fallback.
        let advertised_models: Vec<String> =
            std::env::var("NEVOFLUX_LLM_GATEWAY_ADVERTISED_MODELS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

        // M4-2.6: protocol selector. "anthropic" (default) keeps the
        // existing M2 translator path; "openai" runs the passthrough
        // path against an OpenAI Chat Completions upstream.
        let upstream_protocol = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_PROTOCOL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| UpstreamProtocol::parse_label(&s))
            .unwrap_or_default();

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
            advertised_models,
            upstream_protocol,
            acp_config: None,
        })
    }

    /// Minimal config for tests. Bearer token is a known value so test
    /// clients can include it. `bind_addr` is `127.0.0.1:0` so the OS
    /// can pick a free port if the test ever binds (the in-router tests
    /// via [`serve_test_router`] don't, but the field still needs a
    /// value).
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_default() -> Self {
        Self {
            bind_addr: "127.0.0.1:0".parse().expect("loopback addr parse"),
            bearer_token: "test-token".into(),
            upstream_base_url: "https://test.example".into(),
            upstream_api_key: String::new(),
            upstream_model_remap: None,
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.to_string(),
            upstream_request_timeout: DEFAULT_UPSTREAM_REQUEST_TIMEOUT,
            upstream_connect_timeout: DEFAULT_UPSTREAM_CONNECT_TIMEOUT,
            upstream_stream_idle_timeout: DEFAULT_UPSTREAM_STREAM_IDLE_TIMEOUT,
            upstream_retry_max_wait: DEFAULT_UPSTREAM_RETRY_MAX_WAIT,
            advertised_models: Vec::new(),
            upstream_protocol: UpstreamProtocol::Anthropic,
            acp_config: None,
        }
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
    let bind_addr_requested = config.bind_addr;
    let bearer_token = config.bearer_token.clone();

    // M2-1: state + router construction is factored out so tests can drive
    // the same router via `tower::ServiceExt::oneshot` without binding a
    // TCP listener — see [`serve_test_router`].
    let state = Arc::new(AppState::new(config).await?);
    let upstream_base_url_log = state.upstream_base_url.clone();
    let app = handlers::build_router(state);

    // Bind first so callers can read back the OS-assigned port (if any)
    // and start health-checking immediately.
    let listener = TcpListener::bind(bind_addr_requested).await?;
    let bind_addr = listener.local_addr()?;
    tracing::info!(
        "nevoflux-llm-gateway listening on {bind_addr} (upstream={})",
        upstream_base_url_log
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

/// Build the gateway's [`axum::Router`] for tests, without binding a TCP
/// listener. Used by `tower::ServiceExt::oneshot`-based unit tests to
/// fire requests directly into the router.
///
/// Gated behind `#[cfg(any(test, feature = "test-util"))]` so it doesn't
/// pollute the public API in release builds while still being callable
/// from `tests/*.rs` integration test binaries (which compile against
/// the lib's `test` cfg via the standard `dev-dependencies` path).
#[cfg(any(test, feature = "test-util"))]
pub async fn serve_test_router(config: GatewayConfig) -> axum::Router {
    let state = Arc::new(AppState::new(config).await.expect("AppState::new for test"));
    handlers::build_router(state)
}
