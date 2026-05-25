//! Output-dimension policy for the gateway's `/v1/embeddings` endpoint.
//!
//! `gbrain` 0.40.8.1's `openai` recipe rejects 384-dim vectors and requires
//! one of `[256, 512, 768, 1024, 1536, 3072]`. We keep `nevoflux-llm` at its
//! native 384 (e5-small) and zero-pad to 512 at the gateway exit. Cosine /
//! L2 / dot are mathematically invariant under shared zero-padding on both
//! query and passage sides, so the retrieval geometry is preserved.
//!
//! See `docs/plans/2026-05-24-knowledge-base-spike-plan.md` 附录 B 决策 #7.

/// Target output dimensionality for vectors leaving `/v1/embeddings`.
pub const GATEWAY_OUTPUT_DIM: usize = 512;

/// Zero-pad `v` up to [`GATEWAY_OUTPUT_DIM`]. Vectors already at or beyond
/// the target length are returned untouched (we resize-extend, never
/// truncate).
pub fn zero_pad_to_gateway_dim(mut v: Vec<f32>) -> Vec<f32> {
    if v.len() < GATEWAY_OUTPUT_DIM {
        v.resize(GATEWAY_OUTPUT_DIM, 0.0);
    }
    v
}
