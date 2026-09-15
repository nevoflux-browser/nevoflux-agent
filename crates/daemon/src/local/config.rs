//! Configuration for the on-device (local) inference engine.
//!
//! This module holds only the `[llm.local]` config shape (Task 1.1). It does
//! not run anything — the raw-HTTP local provider and the engine supervisor
//! that actually launches an inference process are added by later tasks.

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

/// Context window the local engine is asked to run at.
///
/// `Auto` lets the engine supervisor (a later task) pick a size based on
/// available VRAM/RAM. `Fixed(n)` pins it to `n` tokens, subject to
/// [`LocalConfig::validated_ctx`] rejecting anything below [`CTX_FLOOR`].
///
/// Deserializes from either the TOML string `"auto"` or an integer
/// (`ctx_size = 32768`), which is why this implements `Deserialize` by hand
/// instead of deriving it — a derived `#[serde(untagged)]` unit variant
/// would only ever match TOML's absence-of-value, not the string `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CtxPref {
    #[default]
    Auto,
    Fixed(u32),
}

impl Serialize for CtxPref {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            CtxPref::Auto => serializer.serialize_str("auto"),
            CtxPref::Fixed(n) => serializer.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for CtxPref {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            Num(u32),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Str(s) if s == "auto" => Ok(CtxPref::Auto),
            Repr::Str(s) => Err(D::Error::custom(format!(
                "invalid ctx_size {s:?}: expected \"auto\" or an integer"
            ))),
            Repr::Num(n) => Ok(CtxPref::Fixed(n)),
        }
    }
}

/// Compute backend the local engine prefers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendPref {
    /// Probe hardware and pick the best available backend.
    #[default]
    Auto,
    Cpu,
    Vulkan,
    Cuda,
}

/// KV cache quantization the local engine uses.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum KvCacheType {
    #[default]
    Q8_0,
    F16,
}

/// The minimum context size local inference is allowed to run at.
///
/// Below this, prompts routinely truncate mid-conversation for the tool-
/// calling workloads this engine serves — so [`LocalConfig::validated_ctx`]
/// rejects it rather than silently running a degraded context.
pub const CTX_FLOOR: u32 = 16_384;

/// The context size new installs are steered toward when hardware allows it.
pub const CTX_PREFERRED: u32 = 32_768;

/// `[llm.local]` — on-device inference engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LocalConfig {
    /// Whether on-device inference is the active provider's backing engine.
    pub enabled: bool,
    /// Model id (catalog key), e.g. "qwen3-4b-instruct-2507".
    pub model: String,
    /// Quantization to fetch/run, e.g. "Q4_K_M".
    pub quant: String,
    /// Compute backend preference.
    pub backend: BackendPref,
    /// GPU layers to offload; -1 offloads as many as fit.
    pub gpu_layers: i32,
    /// Context window preference.
    pub ctx_size: CtxPref,
    /// KV cache quantization.
    pub kv_cache_type: KvCacheType,
    /// Parallel request slots the engine reserves context for.
    pub parallel: u32,
    /// Seconds of inactivity before the engine process is unloaded.
    pub idle_unload_secs: u64,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "qwen3-4b-instruct-2507".to_string(),
            quant: "Q4_K_M".to_string(),
            backend: BackendPref::Auto,
            gpu_layers: -1,
            ctx_size: CtxPref::Auto,
            kv_cache_type: KvCacheType::Q8_0,
            parallel: 2,
            idle_unload_secs: 300,
        }
    }
}

impl LocalConfig {
    /// Validate [`LocalConfig::ctx_size`] against [`CTX_FLOOR`].
    ///
    /// `Auto` always passes — the engine supervisor resolves it at launch
    /// time and is itself bound to never resolve below the floor. A `Fixed`
    /// value below the floor is rejected so a hand-edited config.toml fails
    /// loudly instead of quietly truncating context.
    pub fn validated_ctx(&self) -> Result<CtxPref, String> {
        match self.ctx_size {
            CtxPref::Auto => Ok(CtxPref::Auto),
            CtxPref::Fixed(n) if n < CTX_FLOOR => Err(format!(
                "ctx_size {n} is below the minimum of {CTX_FLOOR}; local inference requires at least {CTX_FLOOR} tokens of context"
            )),
            fixed => Ok(fixed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let c = LocalConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.ctx_size, CtxPref::Auto);
        assert_eq!(c.kv_cache_type, KvCacheType::Q8_0);
        assert_eq!(c.parallel, 2);
        assert_eq!(c.idle_unload_secs, 300);
    }

    #[test]
    fn ctx_accepts_auto_string_and_integers() {
        let c: LocalConfig = toml::from_str("ctx_size = \"auto\"").unwrap();
        assert_eq!(c.ctx_size, CtxPref::Auto);
        let c: LocalConfig = toml::from_str("ctx_size = 32768").unwrap();
        assert_eq!(c.ctx_size, CtxPref::Fixed(32768));
    }

    #[test]
    fn ctx_below_floor_is_rejected() {
        let c: LocalConfig = toml::from_str("ctx_size = 8192").unwrap();
        assert!(c.validated_ctx().is_err());
    }
}
