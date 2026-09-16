//! Memory estimation and ctx/GPU-offload fit selection for on-device
//! inference.
//!
//! [`estimate`] turns a catalog model + quantization + requested context
//! into a [`MemoryEstimate`] (quant file, KV cache, and compute buffer
//! sizes, plus how much of that can live on the GPU for a given VRAM
//! budget). [`choose_ctx`] uses it to pick a context size per
//! [`CtxPref`] and classify the result as [`Fit`] — full GPU, partial GPU
//! (some layers offloaded, the rest + overflow in system RAM), CPU-only,
//! or insufficient for the available hardware.

use crate::local::catalog::{GgufHeaderStatic, LlmModel, Quant};
use crate::local::config::{CtxPref, KvCacheType, CTX_FLOOR, CTX_PREFERRED};

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

/// VRAM headroom kept free of any estimate — a safety margin against the
/// display compositor, other GPU clients, and estimation error, so a
/// "fits" verdict doesn't leave the system with zero free VRAM.
const GPU_FLOOR_BYTES: u64 = 512 * MIB;

/// A memory estimate for running `model`+`quant` at a given context size
/// and KV cache type, optionally split across a GPU VRAM budget.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MemoryEstimate {
    /// Size of the quantized weights file on disk (and, fully or
    /// partially, in memory once loaded).
    pub quant_file_bytes: u64,
    pub kv_bytes: u64,
    pub compute_bytes: u64,
    /// `quant_file_bytes + kv_bytes + compute_bytes`.
    pub total_bytes: u64,
    /// Bytes placed on the GPU under `gpu_budget`, or `None` if no GPU
    /// budget was given (CPU-only).
    pub gpu_bytes: Option<u64>,
    pub gpu_floor_bytes: u64,
    /// Total transformer block count in the model.
    pub layer_count: u32,
    /// Of `layer_count`, how many are offloaded to the GPU.
    pub gpu_layers: u32,
    pub ctx: u32,
    pub kv_cache_type: KvCacheType,
}

/// KV cache size in bytes: `2 * block_count * head_count_kv * key_length *
/// elem_bytes * ctx`, where `elem_bytes` is 2 for `F16` and `34/32`
/// (applied as a final integer multiply-then-divide) for `Q8_0`'s ~6%
/// quantization overhead over 1 byte/element.
pub fn kv_bytes(h: &GgufHeaderStatic, ctx: u32, kv: KvCacheType) -> u64 {
    let base = 2 * h.block_count as u64 * h.head_count_kv as u64 * h.key_length as u64 * ctx as u64;
    match kv {
        KvCacheType::F16 => base * 2,
        KvCacheType::Q8_0 => base * 34 / 32,
    }
}

/// Estimated compute (scratch) buffer size for a forward pass at `ctx`.
///
/// `512 MiB + ctx * embedding_length * 2` — a fixed base plus a per-token
/// working-set term. This is a documented heuristic, not measured:
// TODO(2.9): calibrate against the engine's logged compute buffer size
pub fn compute_bytes(h: &GgufHeaderStatic, ctx: u32) -> u64 {
    512 * MIB + ctx as u64 * h.embedding_length as u64 * 2
}

/// Estimate memory for `model`'s `quant` at `ctx`/`kv`, and how much of it
/// fits on the GPU under `gpu_budget` (`None` = no GPU).
///
/// GPU placement: if `total_bytes` fits within `gpu_budget` minus
/// [`GPU_FLOOR_BYTES`], everything (all layers, KV cache, and compute)
/// goes on the GPU. Otherwise the KV cache and compute buffer — which the
/// engine keeps resident on whichever device runs the forward pass — are
/// reserved first, and whole transformer layers of quantized weight are
/// offloaded into whatever budget remains, largest-first up to
/// `layer_count`.
pub fn estimate(
    m: &LlmModel,
    q: &Quant,
    ctx: u32,
    kv_cache_type: KvCacheType,
    gpu_budget: Option<u64>,
) -> MemoryEstimate {
    let h = &m.header;
    let quant_file_bytes = q.bytes;
    let kv = kv_bytes(h, ctx, kv_cache_type);
    let compute = compute_bytes(h, ctx);
    let total_bytes = quant_file_bytes + kv + compute;
    let layer_count = h.block_count;

    let (gpu_bytes, gpu_layers) = match gpu_budget {
        None => (None, 0),
        Some(budget) => {
            let usable = budget.saturating_sub(GPU_FLOOR_BYTES);
            if total_bytes <= usable {
                (Some(total_bytes), layer_count)
            } else {
                let non_layer = kv + compute;
                let layer_bytes = quant_file_bytes / layer_count.max(1) as u64;
                if usable > non_layer && layer_bytes > 0 {
                    let layers =
                        ((usable - non_layer) / layer_bytes).min(layer_count as u64) as u32;
                    (Some(non_layer + layers as u64 * layer_bytes), layers)
                } else {
                    (Some(non_layer.min(usable)), 0)
                }
            }
        }
    };

    MemoryEstimate {
        quant_file_bytes,
        kv_bytes: kv,
        compute_bytes: compute,
        total_bytes,
        gpu_bytes,
        gpu_floor_bytes: GPU_FLOOR_BYTES,
        layer_count,
        gpu_layers,
        ctx,
        kv_cache_type,
    }
}

/// The outcome of [`choose_ctx`]: which hardware a model+context fits on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "fit", rename_all = "snake_case")]
pub enum Fit {
    FullGpu { ctx: u32 },
    PartialGpu { ctx: u32, gpu_layers: u32 },
    CpuOnly { ctx: u32 },
    Insufficient { reason: String },
}

/// Classify `model`+`quant` at a specific `ctx` against the available
/// hardware, or `None` if it fits nowhere (not even CPU-only in `ram`).
fn fit_at_ctx(
    m: &LlmModel,
    q: &Quant,
    ctx: u32,
    kv: KvCacheType,
    vram: Option<u64>,
    ram: u64,
) -> Option<Fit> {
    let est = estimate(m, q, ctx, kv, vram);

    if let Some(budget) = vram {
        let usable = budget.saturating_sub(GPU_FLOOR_BYTES);
        if est.total_bytes <= usable {
            return Some(Fit::FullGpu { ctx });
        }
        if est.gpu_layers > 0 {
            let ram_needed = est.total_bytes.saturating_sub(est.gpu_bytes.unwrap_or(0));
            if ram >= ram_needed {
                return Some(Fit::PartialGpu {
                    ctx,
                    gpu_layers: est.gpu_layers,
                });
            }
        }
    }

    if ram >= est.total_bytes {
        Some(Fit::CpuOnly { ctx })
    } else {
        None
    }
}

/// Pick a context size per `pref` and classify the result.
///
/// - `Auto`: [`CTX_PREFERRED`] if it fits fully on the GPU; otherwise
///   [`CTX_FLOOR`], accepting full GPU, partial GPU, or CPU-only, whichever
///   the hardware supports; otherwise [`Fit::Insufficient`].
/// - `Fixed(n)`: `n`, rejected as [`Fit::Insufficient`] if below
///   [`CTX_FLOOR`] (mirrors [`crate::local::config::LocalConfig::validated_ctx`]'s
///   floor, defensively re-checked here since callers may not have gone
///   through config validation), otherwise the same fit classification as
///   `Auto`'s fallback step.
pub fn choose_ctx(
    m: &LlmModel,
    q: &Quant,
    pref: CtxPref,
    kv: KvCacheType,
    vram: Option<u64>,
    ram: u64,
) -> Fit {
    match pref {
        CtxPref::Auto => {
            if let Some(fit @ Fit::FullGpu { .. }) = fit_at_ctx(m, q, CTX_PREFERRED, kv, vram, ram)
            {
                return fit;
            }
            fit_at_ctx(m, q, CTX_FLOOR, kv, vram, ram).unwrap_or_else(|| Fit::Insufficient {
                reason: format!(
                    "{} does not fit at {CTX_FLOOR} tokens of context in the available memory",
                    m.id
                ),
            })
        }
        CtxPref::Fixed(n) if n < CTX_FLOOR => Fit::Insufficient {
            reason: format!("ctx {n} is below the minimum of {CTX_FLOOR} tokens"),
        },
        CtxPref::Fixed(n) => {
            fit_at_ctx(m, q, n, kv, vram, ram).unwrap_or_else(|| Fit::Insufficient {
                reason: format!(
                    "{} does not fit at {n} tokens of context in the available memory",
                    m.id
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::catalog;

    #[test]
    fn kv_bytes_matches_design_numbers() {
        let h = catalog::model("qwen3-4b-instruct-2507").unwrap().header;
        let f16_16k = kv_bytes(&h, 16384, KvCacheType::F16);
        assert_eq!(f16_16k, 2 * 36 * 8 * 128 * 2 * 16384); // 2.25 GiB
        let q8_32k = kv_bytes(&h, 32768, KvCacheType::Q8_0);
        // No `as u64` cast here (unlike the brief's literal wording): with
        // one, the parenthesized product is inferred as `i32` (the cast
        // breaks type-inference propagation from this `assert_eq!`'s `u64`
        // comparison) and overflows at compile time. Dropping it keeps the
        // identical left-to-right evaluation order and value, just as `u64`
        // throughout — the same type `kv_bytes`'s own `base` computation uses.
        assert_eq!(q8_32k, 2 * 36 * 8 * 128 * 32768 * 34 / 32); // ~2.39 GiB
    }

    #[test]
    fn auto_prefers_32k_when_it_fits_on_a_16gb_gpu() {
        let m = catalog::model("qwen3-4b-instruct-2507").unwrap();
        let q = &m.quants[0];
        assert_eq!(
            choose_ctx(
                m,
                q,
                CtxPref::Auto,
                KvCacheType::Q8_0,
                Some(16 * GIB),
                48 * GIB
            ),
            Fit::FullGpu { ctx: 32768 }
        );
    }

    #[test]
    fn auto_falls_back_to_16k_then_insufficient() {
        let m = catalog::model("qwen3-8b").unwrap();
        let q = &m.quants[0];
        assert!(matches!(
            choose_ctx(
                m,
                q,
                CtxPref::Auto,
                KvCacheType::Q8_0,
                Some(6 * GIB),
                8 * GIB
            ),
            Fit::PartialGpu { ctx: 16384, .. } | Fit::CpuOnly { ctx: 16384 }
        ));
        assert!(matches!(
            choose_ctx(m, q, CtxPref::Auto, KvCacheType::Q8_0, None, 4 * GIB),
            Fit::Insufficient { .. }
        ));
    }

    #[test]
    fn fixed_below_floor_is_insufficient_regardless_of_hardware() {
        let m = catalog::model("qwen3-4b-instruct-2507").unwrap();
        let q = &m.quants[0];
        assert!(matches!(
            choose_ctx(
                m,
                q,
                CtxPref::Fixed(8192),
                KvCacheType::Q8_0,
                Some(64 * GIB),
                128 * GIB
            ),
            Fit::Insufficient { .. }
        ));
    }

    #[test]
    fn fixed_at_floor_fits_cpu_only_with_no_gpu() {
        let m = catalog::model("qwen3-1.7b").unwrap();
        let q = &m.quants[0];
        assert_eq!(
            choose_ctx(
                m,
                q,
                CtxPref::Fixed(CTX_FLOOR),
                KvCacheType::Q8_0,
                None,
                32 * GIB
            ),
            Fit::CpuOnly { ctx: CTX_FLOOR }
        );
    }

    #[test]
    fn estimate_reports_full_gpu_layers_when_everything_fits() {
        let m = catalog::model("qwen3-1.7b").unwrap();
        let q = &m.quants[0];
        let est = estimate(m, q, 16384, KvCacheType::Q8_0, Some(24 * GIB));
        assert_eq!(est.gpu_layers, est.layer_count);
        assert_eq!(est.gpu_bytes, Some(est.total_bytes));
        assert_eq!(
            est.total_bytes,
            est.quant_file_bytes + est.kv_bytes + est.compute_bytes
        );
    }

    #[test]
    fn estimate_offloads_zero_layers_with_no_gpu_budget() {
        let m = catalog::model("qwen3-1.7b").unwrap();
        let q = &m.quants[0];
        let est = estimate(m, q, 16384, KvCacheType::Q8_0, None);
        assert_eq!(est.gpu_layers, 0);
        assert_eq!(est.gpu_bytes, None);
    }
}
