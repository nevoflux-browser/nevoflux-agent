//! Local OpenAI-compatible HTTP gateway for the nevoflux LLM stack.
//!
//! This crate translates incoming OpenAI ChatCompletions / Embeddings
//! requests into the upstream provider's native protocol. The initial
//! upstream target is the Anthropic Messages API; additional upstreams
//! may be added behind the same OpenAI-shaped public surface.
//!
//! The library half (this module tree) only exposes pure translation
//! logic so it can be unit-tested without network access. The binary
//! half (`src/main.rs`) wires the translator behind an axum server.
//!
//! See `docs/plans/2026-05-24-knowledge-base-spike-plan.md` 附录 B for
//! the gate-C validation results and the model-remap / permissive-enum
//! decisions (#25, #26) implemented in [`translate`].

pub mod translate;
