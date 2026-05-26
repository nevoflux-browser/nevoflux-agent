//! In-process [`nevoflux_llm_gateway`] boot-up (M1 #010, M2-5).
//!
//! Spawns the gateway as a tokio task at daemon startup, with a free
//! loopback port assigned by the OS and a freshly-generated bearer
//! token. Downstream consumers (gbrain subprocess in M3) read the URL +
//! token off the daemon's [`GatewayHandleSnapshot`] without ever
//! talking to a child process.
//!
//! Upstream provider config is resolved by [`resolve_upstream_config`]
//! in layered order: TOML config file → env vars → built-in defaults.
//!
//! See `docs/plans/2026-05-24-knowledge-base-spike-plan.md` 附录 B.

use std::{net::SocketAddr, time::Duration};

use nevoflux_llm_gateway::{
    serve as serve_gateway, GatewayConfig, GatewayHandle, DEFAULT_ANTHROPIC_VERSION,
    DEFAULT_UPSTREAM_BASE, DEFAULT_UPSTREAM_CONNECT_TIMEOUT, DEFAULT_UPSTREAM_REQUEST_TIMEOUT,
    DEFAULT_UPSTREAM_RETRY_MAX_WAIT, DEFAULT_UPSTREAM_STREAM_IDLE_TIMEOUT,
};
use rand::RngCore;
use tracing::{info, warn};

use crate::config::{GatewayUpstreamConfig, KnowledgeBaseConfig};

/// Clone-safe snapshot of a running gateway, suitable for storing in
/// places that don't own the [`GatewayHandle`] itself (which holds the
/// `JoinHandle` + shutdown channel — not [`Clone`]).
#[derive(Clone, Debug)]
pub struct GatewayHandleSnapshot {
    /// Canonical `http://127.0.0.1:<port>` URL.
    pub url: String,
    /// Bearer token clients must present on `/v1/*`.
    pub bearer_token: String,
}

impl GatewayHandleSnapshot {
    /// Build a snapshot from a live [`GatewayHandle`].
    pub fn from_handle(handle: &GatewayHandle) -> Self {
        Self {
            url: handle.url(),
            bearer_token: handle.bearer_token.clone(),
        }
    }
}

/// Combined return: the live handle (held by the daemon for shutdown)
/// plus the snapshot (stored on the daemon for downstream consumers).
pub struct GatewayBoot {
    pub handle: GatewayHandle,
    pub snapshot: GatewayHandleSnapshot,
}

/// Boxed error returned by gateway init. Daemon doesn't depend on
/// `anyhow`, so we use a plain trait object to stay light.
pub type InitError = Box<dyn std::error::Error + Send + Sync>;

/// Fully-resolved upstream configuration produced by
/// [`resolve_upstream_config`]. Every field has a concrete value with
/// the layered fallback (config → env → default) already applied.
#[derive(Debug, Clone)]
struct ResolvedUpstreamConfig {
    upstream_base_url: String,
    upstream_api_key: String,
    /// `None` = no remap (passthrough). `Some(name)` = rewrite incoming
    /// `model` field before hitting upstream.
    upstream_model_remap: Option<String>,
    anthropic_version: String,
    request_timeout: Duration,
    connect_timeout: Duration,
    stream_idle_timeout: Duration,
    retry_max_wait: Duration,
    /// Models advertised by `GET /v1/models` (M2-1). Always non-empty by
    /// the time this struct is constructed — see [`resolve_upstream_config`]
    /// for the fallback rules.
    advertised_models: Vec<String>,
}

/// Resolve upstream gateway settings using the M2-5 precedence order:
///
///   1. Non-empty value from the TOML config (`[knowledge_base.gateway]`).
///   2. Non-empty value from the corresponding env var.
///   3. Built-in default (the `DEFAULT_*` constants exported by the
///      `nevoflux-llm-gateway` crate).
///
/// `upstream_api_key` additionally chains through `ANTHROPIC_API_KEY`
/// as a final env fallback (preserved from M1 for backward compat).
fn resolve_upstream_config(config: &GatewayUpstreamConfig) -> ResolvedUpstreamConfig {
    let upstream_base_url = if !config.upstream_base_url.is_empty() {
        config.upstream_base_url.clone()
    } else {
        std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_UPSTREAM_BASE.to_string())
    };

    let upstream_api_key = if !config.upstream_api_key.is_empty() {
        config.upstream_api_key.clone()
    } else {
        std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("ANTHROPIC_API_KEY")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default()
    };

    let upstream_model_remap = if !config.upstream_model_remap.is_empty() {
        Some(config.upstream_model_remap.clone())
    } else {
        std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
    };

    let anthropic_version = if !config.anthropic_version.is_empty() {
        config.anthropic_version.clone()
    } else {
        std::env::var("NEVOFLUX_LLM_GATEWAY_ANTHROPIC_VERSION")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ANTHROPIC_VERSION.to_string())
    };

    let request_timeout = if config.request_timeout_secs > 0 {
        Duration::from_secs(config.request_timeout_secs)
    } else {
        env_duration_secs(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_REQUEST_TIMEOUT_SECS",
            DEFAULT_UPSTREAM_REQUEST_TIMEOUT,
        )
    };

    let connect_timeout = if config.connect_timeout_secs > 0 {
        Duration::from_secs(config.connect_timeout_secs)
    } else {
        env_duration_secs(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_CONNECT_TIMEOUT_SECS",
            DEFAULT_UPSTREAM_CONNECT_TIMEOUT,
        )
    };

    let stream_idle_timeout = if config.stream_idle_timeout_secs > 0 {
        Duration::from_secs(config.stream_idle_timeout_secs)
    } else {
        env_duration_secs(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS",
            DEFAULT_UPSTREAM_STREAM_IDLE_TIMEOUT,
        )
    };

    let retry_max_wait = if config.retry_max_wait_secs > 0 {
        Duration::from_secs(config.retry_max_wait_secs)
    } else {
        env_duration_secs(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_RETRY_MAX_WAIT_SECS",
            DEFAULT_UPSTREAM_RETRY_MAX_WAIT,
        )
    };

    // M2-1: synthesize a non-empty `advertised_models` list. Three
    // sources, in priority order:
    //   1. Non-empty TOML list — use verbatim.
    //   2. Otherwise, single entry derived from `upstream_model_remap`
    //      so naive clients calling `GET /v1/models` see at least the
    //      model the gateway will actually forward as.
    //   3. Otherwise, sentinel `"default"` so naive clients calling
    //      list-models on a freshly-booted gateway always get a valid
    //      response shape.
    let advertised_models = if !config.advertised_models.is_empty() {
        config.advertised_models.clone()
    } else if let Some(remap) = upstream_model_remap.as_deref().filter(|s| !s.is_empty()) {
        vec![remap.to_string()]
    } else {
        vec!["default".to_string()]
    };

    ResolvedUpstreamConfig {
        upstream_base_url,
        upstream_api_key,
        upstream_model_remap,
        anthropic_version,
        request_timeout,
        connect_timeout,
        stream_idle_timeout,
        retry_max_wait,
        advertised_models,
    }
}

/// Read a `Duration` from an env var holding whole-second integer text.
/// Falls back to `default` if the var is unset, empty, or unparsable.
fn env_duration_secs(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// Initialize the in-process llm-gateway, if enabled by config.
///
/// Returns `Ok(None)` when `knowledge_base.enabled = false`. Returns
/// `Err(_)` if the listener fails to bind or `/healthz` never returns
/// 2xx within the polling window.
///
/// Behavior:
/// 1. Bind `127.0.0.1:0` so the OS picks a free port.
/// 2. Generate a 32-byte hex bearer token via `rand::thread_rng`.
/// 3. Resolve upstream config via [`resolve_upstream_config`] (M2-5):
///    TOML config file → env vars → built-in defaults. Empty
///    `upstream_api_key` is tolerated for boot — chat-completions will
///    fail upstream until a key is supplied, which is fine: M1 #010
///    only needs the listener up.
/// 4. Call [`nevoflux_llm_gateway::serve`] to spawn the task.
/// 5. Poll `/healthz` up to 20 times at 100 ms intervals before
///    declaring success.
pub async fn init_gateway(
    config: &KnowledgeBaseConfig,
) -> Result<Option<GatewayBoot>, InitError> {
    if !config.enabled {
        info!("knowledge_base.enabled = false — skipping llm-gateway start");
        return Ok(None);
    }

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback addr parse");

    let bearer_token = generate_random_token();

    let resolved = resolve_upstream_config(&config.gateway);

    if resolved.upstream_api_key.is_empty() {
        warn!(
            "no upstream API key found in TOML [knowledge_base.gateway].upstream_api_key, \
             NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY, or ANTHROPIC_API_KEY — gateway \
             /v1/chat/completions will 401 from upstream until one is supplied"
        );
    }

    let gateway_config = GatewayConfig {
        bind_addr,
        bearer_token,
        upstream_base_url: resolved.upstream_base_url,
        upstream_api_key: resolved.upstream_api_key,
        upstream_model_remap: resolved.upstream_model_remap,
        anthropic_version: resolved.anthropic_version,
        upstream_request_timeout: resolved.request_timeout,
        upstream_connect_timeout: resolved.connect_timeout,
        upstream_stream_idle_timeout: resolved.stream_idle_timeout,
        upstream_retry_max_wait: resolved.retry_max_wait,
        advertised_models: resolved.advertised_models,
    };

    let handle = serve_gateway(gateway_config)
        .await
        .map_err(|e| -> InitError { format!("gateway serve failed: {e}").into() })?;
    info!(
        "llm-gateway listening on {} (bearer token redacted)",
        handle.url()
    );

    // Health-check the gateway before declaring boot done. Use a fresh
    // reqwest client (the gateway's own internal one is on the server
    // side, not shareable here).
    let url = format!("{}/healthz", handle.url());
    let client = reqwest::Client::new();
    let max_tries = 20u32;
    let mut tries = 0u32;
    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    "llm-gateway /healthz OK after {} tries",
                    tries.saturating_add(1)
                );
                break;
            }
            _ if tries >= max_tries => {
                return Err(format!(
                    "llm-gateway failed /healthz after {} tries",
                    max_tries
                )
                .into());
            }
            _ => {
                tries = tries.saturating_add(1);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    let snapshot = GatewayHandleSnapshot::from_handle(&handle);
    Ok(Some(GatewayBoot { handle, snapshot }))
}

/// Generate a 32-byte random hex bearer token.
fn generate_random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex serializing every test that mutates process-wide env vars.
    ///
    /// Cargo's default test runner executes tests in parallel by sharing
    /// the same process, so two tests setting/unsetting the same env
    /// var race each other and corrupt the result. Wrapping the
    /// mutating section in this mutex makes those tests serialize
    /// without pulling in `serial_test` as a new dev-dep.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Env vars the resolver consults. Clear them all up-front in any
    /// test that wants a clean baseline.
    const RESOLVER_ENV_VARS: &[&str] = &[
        "NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY",
        "NEVOFLUX_LLM_GATEWAY_UPSTREAM_BASE_URL",
        "NEVOFLUX_LLM_GATEWAY_UPSTREAM_MODEL",
        "NEVOFLUX_LLM_GATEWAY_ANTHROPIC_VERSION",
        "NEVOFLUX_LLM_GATEWAY_UPSTREAM_REQUEST_TIMEOUT_SECS",
        "NEVOFLUX_LLM_GATEWAY_UPSTREAM_CONNECT_TIMEOUT_SECS",
        "NEVOFLUX_LLM_GATEWAY_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS",
        "NEVOFLUX_LLM_GATEWAY_UPSTREAM_RETRY_MAX_WAIT_SECS",
        "ANTHROPIC_API_KEY",
    ];

    fn clear_resolver_env() {
        for var in RESOLVER_ENV_VARS {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn random_token_is_64_hex_chars() {
        let token = generate_random_token();
        assert_eq!(token.len(), 64, "32 bytes -> 64 hex chars");
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "token must be all hex digits"
        );
    }

    #[test]
    fn random_token_is_distinct_between_calls() {
        let a = generate_random_token();
        let b = generate_random_token();
        assert_ne!(a, b, "consecutive tokens should differ");
    }

    #[tokio::test]
    async fn init_gateway_disabled_returns_none() {
        let cfg = KnowledgeBaseConfig {
            enabled: false,
            gateway: GatewayUpstreamConfig::default(),
        };
        let result = init_gateway(&cfg).await.expect("disabled is not an error");
        assert!(
            result.is_none(),
            "disabled config must yield None, got Some"
        );
    }

    #[tokio::test]
    async fn init_gateway_enabled_boots_and_healthchecks() {
        // Use defaults across the board — the resolver will pick a
        // reasonable base URL + empty key, and /healthz is un-authed
        // so the test passes without real Anthropic creds.
        let cfg = KnowledgeBaseConfig {
            enabled: true,
            gateway: GatewayUpstreamConfig::default(),
        };
        let boot = init_gateway(&cfg)
            .await
            .expect("enabled init should succeed")
            .expect("enabled config must yield Some");
        assert!(boot.snapshot.url.starts_with("http://127.0.0.1:"));
        assert_eq!(boot.snapshot.bearer_token.len(), 64);
        boot.handle.shutdown().await;
    }

    #[test]
    fn resolve_upstream_config_uses_config_value_over_env() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        // SAFETY: serialized via ENV_MUTEX above. Set env to a known-
        // wrong value and verify config beats it.
        std::env::set_var(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_BASE_URL",
            "https://env-loses.example",
        );
        let config = GatewayUpstreamConfig {
            upstream_base_url: "https://config-wins.example".into(),
            ..Default::default()
        };
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.upstream_base_url, "https://config-wins.example");
        clear_resolver_env();
    }

    #[test]
    fn resolve_upstream_config_falls_back_to_env() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        std::env::set_var(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_BASE_URL",
            "https://from-env.example",
        );
        let config = GatewayUpstreamConfig::default();
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.upstream_base_url, "https://from-env.example");
        clear_resolver_env();
    }

    #[test]
    fn resolve_upstream_config_falls_back_to_builtin_default() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        let config = GatewayUpstreamConfig::default();
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.upstream_base_url, DEFAULT_UPSTREAM_BASE);
        assert_eq!(resolved.anthropic_version, DEFAULT_ANTHROPIC_VERSION);
        assert!(resolved.upstream_model_remap.is_none());
        assert!(resolved.upstream_api_key.is_empty());
        assert_eq!(resolved.request_timeout, DEFAULT_UPSTREAM_REQUEST_TIMEOUT);
        assert_eq!(resolved.connect_timeout, DEFAULT_UPSTREAM_CONNECT_TIMEOUT);
        assert_eq!(
            resolved.stream_idle_timeout,
            DEFAULT_UPSTREAM_STREAM_IDLE_TIMEOUT
        );
        assert_eq!(resolved.retry_max_wait, DEFAULT_UPSTREAM_RETRY_MAX_WAIT);
        // M2-1: no advertised_models in TOML + no remap = single sentinel
        // "default" entry so `GET /v1/models` never returns an empty list.
        assert_eq!(resolved.advertised_models, vec!["default".to_string()]);
    }

    #[test]
    fn anthropic_api_key_falls_back_through_env_chain() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        // Config empty, NEVOFLUX_*_API_KEY empty, ANTHROPIC_API_KEY set.
        std::env::set_var("ANTHROPIC_API_KEY", "fallback-key");
        let config = GatewayUpstreamConfig::default();
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.upstream_api_key, "fallback-key");
        clear_resolver_env();
    }

    #[test]
    fn anthropic_api_key_prefers_nevoflux_env_over_anthropic_env() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        std::env::set_var(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY",
            "nevoflux-wins",
        );
        std::env::set_var("ANTHROPIC_API_KEY", "anthropic-loses");
        let config = GatewayUpstreamConfig::default();
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.upstream_api_key, "nevoflux-wins");
        clear_resolver_env();
    }

    #[test]
    fn timeout_zero_falls_back_to_env_or_default() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        std::env::set_var(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_REQUEST_TIMEOUT_SECS",
            "120",
        );
        let config = GatewayUpstreamConfig::default();
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.request_timeout, Duration::from_secs(120));
        clear_resolver_env();

        // And with no env set, falls all the way back to the built-in.
        let resolved = resolve_upstream_config(&GatewayUpstreamConfig::default());
        assert_eq!(resolved.request_timeout, DEFAULT_UPSTREAM_REQUEST_TIMEOUT);
    }

    #[test]
    fn advertised_models_uses_toml_list_verbatim() {
        // M2-1: TOML provides an explicit list — it wins, even over a
        // populated `upstream_model_remap`.
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        let config = GatewayUpstreamConfig {
            upstream_model_remap: "remap-loses".into(),
            advertised_models: vec!["m-1".into(), "m-2".into(), "m-3".into()],
            ..Default::default()
        };
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.advertised_models, vec!["m-1", "m-2", "m-3"]);
    }

    #[test]
    fn advertised_models_falls_back_to_remap_when_empty() {
        // M2-1: empty TOML list + populated `upstream_model_remap` -> a
        // single-entry list with the remap target.
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        let config = GatewayUpstreamConfig {
            upstream_model_remap: "claude-haiku-4-5".into(),
            advertised_models: Vec::new(),
            ..Default::default()
        };
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.advertised_models, vec!["claude-haiku-4-5"]);
    }

    #[test]
    fn timeout_nonzero_config_wins_over_env() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_resolver_env();
        std::env::set_var(
            "NEVOFLUX_LLM_GATEWAY_UPSTREAM_REQUEST_TIMEOUT_SECS",
            "999",
        );
        let config = GatewayUpstreamConfig {
            request_timeout_secs: 30,
            ..Default::default()
        };
        let resolved = resolve_upstream_config(&config);
        assert_eq!(resolved.request_timeout, Duration::from_secs(30));
        clear_resolver_env();
    }
}
