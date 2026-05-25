//! `nevoflux-llm-gateway` binary.
//!
//! A thin Ctrl-C wrapper around the library
//! [`nevoflux_llm_gateway::serve`] entrypoint. M1 #010 moved all
//! server-bootstrap logic into the library so the daemon can spawn the
//! same gateway in-process; this binary remains for standalone runs
//! (testing, debugging, and ad-hoc usage).
//!
//! ## Routes
//!
//! * `GET  /healthz`             — un-authed liveness probe.
//! * `POST /v1/chat/completions` — bearer-authed; OpenAI ChatCompletions.
//! * `POST /v1/embeddings`       — bearer-authed; OpenAI-shaped, backed
//!   by `nevoflux-llm`'s `FastEmbedProvider`. Native 384-d e5-small
//!   vectors are zero-padded to 512 (附录 B 决策 #7).
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
//! When spawned by the daemon (M1 #010+), the same configuration is
//! supplied via [`nevoflux_llm_gateway::GatewayConfig`] directly,
//! bypassing env-var parsing.

use nevoflux_llm_gateway::{serve, GatewayConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nevoflux_llm_gateway=info,tower_http=info".into()),
        )
        .init();

    let config = GatewayConfig::from_env()?;
    let handle = serve(config).await?;

    tracing::info!("gateway running on {} — Ctrl-C to stop", handle.url());

    tokio::signal::ctrl_c().await?;
    tracing::info!("received Ctrl-C, shutting down gateway");
    handle.shutdown().await;
    Ok(())
}
