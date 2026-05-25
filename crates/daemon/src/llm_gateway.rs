//! In-process [`nevoflux_llm_gateway`] boot-up (M1 #010).
//!
//! Spawns the gateway as a tokio task at daemon startup, with a free
//! loopback port assigned by the OS and a freshly-generated bearer
//! token. Downstream consumers (gbrain subprocess in M3) read the URL +
//! token off the daemon's [`GatewayHandleSnapshot`] without ever
//! talking to a child process.
//!
//! See `docs/plans/2026-05-24-knowledge-base-spike-plan.md` 附录 B.

use std::{net::SocketAddr, time::Duration};

use nevoflux_llm_gateway::{
    serve as serve_gateway, GatewayConfig, GatewayHandle, DEFAULT_ANTHROPIC_VERSION,
    DEFAULT_UPSTREAM_BASE,
};
use rand::RngCore;
use tracing::{info, warn};

use crate::config::KnowledgeBaseConfig;

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

/// Initialize the in-process llm-gateway, if enabled by config.
///
/// Returns `Ok(None)` when `knowledge_base.enabled = false`. Returns
/// `Err(_)` if the listener fails to bind or `/healthz` never returns
/// 2xx within the polling window.
///
/// Behavior:
/// 1. Bind `127.0.0.1:0` so the OS picks a free port.
/// 2. Generate a 32-byte hex bearer token via `rand::thread_rng`.
/// 3. Read upstream config from the historical
///    `NEVOFLUX_LLM_GATEWAY_*` env vars (so existing dev workflows
///    still work). Empty `upstream_api_key` is tolerated for boot —
///    chat-completions will fail upstream until a key is supplied,
///    which is fine: M1 #010 only needs the listener up.
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

    let upstream_api_key = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .unwrap_or_default();

    if upstream_api_key.is_empty() {
        warn!(
            "no upstream API key in env (NEVOFLUX_LLM_GATEWAY_UPSTREAM_API_KEY or \
             ANTHROPIC_API_KEY) — gateway /v1/chat/completions will 401 from upstream \
             until one is supplied (M3 gbrain integration will provide it)"
        );
    }

    let upstream_base_url = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_UPSTREAM_BASE.to_string());

    let upstream_model_remap = std::env::var("NEVOFLUX_LLM_GATEWAY_UPSTREAM_MODEL")
        .ok()
        .filter(|s| !s.is_empty());

    let anthropic_version = std::env::var("NEVOFLUX_LLM_GATEWAY_ANTHROPIC_VERSION")
        .unwrap_or_else(|_| DEFAULT_ANTHROPIC_VERSION.to_string());

    let gateway_config = GatewayConfig {
        bind_addr,
        bearer_token,
        upstream_base_url,
        upstream_api_key,
        upstream_model_remap,
        anthropic_version,
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
        let cfg = KnowledgeBaseConfig { enabled: false };
        let result = init_gateway(&cfg).await.expect("disabled is not an error");
        assert!(
            result.is_none(),
            "disabled config must yield None, got Some"
        );
    }

    #[tokio::test]
    async fn init_gateway_enabled_boots_and_healthchecks() {
        // Set an empty upstream API key so we don't need real Anthropic
        // creds for this test. /healthz is un-authed so it'll pass.
        let cfg = KnowledgeBaseConfig { enabled: true };
        let boot = init_gateway(&cfg)
            .await
            .expect("enabled init should succeed")
            .expect("enabled config must yield Some");
        assert!(boot.snapshot.url.starts_with("http://127.0.0.1:"));
        assert_eq!(boot.snapshot.bearer_token.len(), 64);
        boot.handle.shutdown().await;
    }
}
