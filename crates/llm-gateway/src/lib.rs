//! Local OpenAI-compatible HTTP gateway for the nevoflux LLM stack.
//!
//! This crate translates incoming OpenAI ChatCompletions / Embeddings
//! requests into the upstream provider's native protocol. The initial
//! upstream target is the Anthropic Messages API; additional upstreams
//! may be added behind the same OpenAI-shaped public surface.
//!
//! The library half (this module tree) exposes both the pure translation
//! logic (so it can be unit-tested without network access) and a
//! [`serve`] entrypoint that builds the axum router + binds a TCP
//! listener + serves in the background. The binary half
//! (`src/main.rs`) is now a thin Ctrl-C wrapper around [`serve`] — the
//! daemon spawns the same code in-process via M1 #010.
//!
//! See `docs/plans/2026-05-24-knowledge-base-spike-plan.md` 附录 B for
//! the gate-C validation results and the model-remap / permissive-enum
//! decisions (#25, #26) implemented in [`translate`].

pub mod embedding_dim;
pub mod translate;

mod handlers;
mod server;

pub use server::{
    serve, GatewayConfig, GatewayHandle, DEFAULT_ANTHROPIC_VERSION, DEFAULT_PORT,
    DEFAULT_UPSTREAM_BASE,
};
