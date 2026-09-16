//! The catalog of on-device models this engine knows how to run.
//!
//! Every entry's [`GgufHeaderStatic`] is a compile-time copy of the real
//! GGUF file's header, verified once against the downloaded file (Task
//! 2.1's Step 1) rather than guessed — [`crate::local::memory`] uses those
//! dimensions to estimate KV-cache and compute memory without opening the
//! (multi-gigabyte) file at all. If a model's GGUF conversion ever changes
//! these dimensions, re-verify with [`crate::local::gguf::read_header`] and
//! correct the constants here.
//!
//! Each [`Quant`] lists its download sources in the order they should be
//! tried: ModelScope (mainland-China-reachable, supports HTTP Range),
//! hf-mirror, then huggingface.co directly. ModelScope and hf-mirror were
//! verified byte-identical to huggingface.co (same LFS sha256).

/// Whether a model's chat template exposes a hybrid thinking/non-thinking
/// mode (Qwen3's `enable_thinking` template kwarg) or has none at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingMode {
    None,
    Hybrid,
}

/// Default sampling parameters for a model, applied when a request doesn't
/// override them.
#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: i32,
    pub min_p: f32,
    pub presence_penalty: f32,
}

/// A single quantization of a catalog model: the file to fetch, its exact
/// size and hash (verified against the real download), and where to get it.
#[derive(Debug, Clone, Copy)]
pub struct Quant {
    /// Quantization label, e.g. `"Q4_K_M"`.
    pub bits: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    /// Source URLs in try-order (first to last).
    pub sources: &'static [&'static str],
}

/// The GGUF header dimensions this engine needs, as compile-time constants
/// for a catalog model. See [`crate::local::gguf::GgufHeader`] for the
/// runtime-read counterpart (which also carries `architecture`, needed to
/// resolve the metadata keys but not to run the model once resolved).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GgufHeaderStatic {
    pub block_count: u32,
    pub embedding_length: u32,
    pub head_count: u32,
    pub head_count_kv: u32,
    pub key_length: u32,
    pub context_length: u32,
}

/// A model this engine can download and run.
#[derive(Debug, Clone, Copy)]
pub struct LlmModel {
    /// Catalog id, e.g. `"qwen3-4b-instruct-2507"` — matches
    /// [`crate::local::config::LocalConfig::model`].
    pub id: &'static str,
    pub display_name: &'static str,
    pub thinking: ThinkingMode,
    pub sampling: Sampling,
    pub header: GgufHeaderStatic,
    pub quants: &'static [Quant],
}

const QWEN3_4B_INSTRUCT_2507: LlmModel = LlmModel {
    id: "qwen3-4b-instruct-2507",
    display_name: "Qwen3 4B Instruct (2507)",
    thinking: ThinkingMode::None,
    sampling: Sampling {
        temperature: 0.7,
        top_p: 0.8,
        top_k: 20,
        min_p: 0.0,
        presence_penalty: 0.0,
    },
    header: GgufHeaderStatic {
        block_count: 36,
        embedding_length: 2560,
        head_count: 32,
        head_count_kv: 8,
        key_length: 128,
        context_length: 262144,
    },
    quants: &[Quant {
        bits: "Q4_K_M",
        file: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        bytes: 2_497_281_120,
        sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
        sources: &[
            "https://modelscope.cn/models/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/master/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
            "https://hf-mirror.com/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
            "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        ],
    }],
};

const QWEN3_8B: LlmModel = LlmModel {
    id: "qwen3-8b",
    display_name: "Qwen3 8B",
    thinking: ThinkingMode::Hybrid,
    sampling: Sampling {
        temperature: 0.7,
        top_p: 0.8,
        top_k: 20,
        min_p: 0.0,
        presence_penalty: 1.5,
    },
    header: GgufHeaderStatic {
        block_count: 36,
        embedding_length: 4096,
        head_count: 32,
        head_count_kv: 8,
        key_length: 128,
        context_length: 40960,
    },
    quants: &[Quant {
        bits: "Q4_K_M",
        file: "Qwen3-8B-Q4_K_M.gguf",
        bytes: 5_027_783_488,
        sha256: "d98cdcbd03e17ce47681435b5150e34c1417f50b5c0019dd560e4882c5745785",
        sources: &[
            "https://modelscope.cn/models/Qwen/Qwen3-8B-GGUF/resolve/master/Qwen3-8B-Q4_K_M.gguf",
            "https://hf-mirror.com/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf",
            "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf",
        ],
    }],
};

const QWEN3_1_7B: LlmModel = LlmModel {
    id: "qwen3-1.7b",
    display_name: "Qwen3 1.7B",
    thinking: ThinkingMode::Hybrid,
    sampling: Sampling {
        temperature: 0.7,
        top_p: 0.8,
        top_k: 20,
        min_p: 0.0,
        presence_penalty: 1.5,
    },
    header: GgufHeaderStatic {
        block_count: 28,
        embedding_length: 2048,
        head_count: 16,
        head_count_kv: 8,
        key_length: 128,
        context_length: 40960,
    },
    quants: &[Quant {
        bits: "Q4_K_M",
        file: "Qwen3-1.7B-Q4_K_M.gguf",
        bytes: 1_107_409_472,
        sha256: "b139949c5bd74937ad8ed8c8cf3d9ffb1e99c866c823204dc42c0d91fa181897",
        sources: &[
            "https://modelscope.cn/models/unsloth/Qwen3-1.7B-GGUF/resolve/master/Qwen3-1.7B-Q4_K_M.gguf",
            "https://hf-mirror.com/unsloth/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q4_K_M.gguf",
            "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q4_K_M.gguf",
        ],
    }],
};

/// All catalog models. [`LocalConfig::model`](crate::local::config::LocalConfig::model)
/// defaults to `qwen3-4b-instruct-2507`'s id; `qwen3-8b` and `qwen3-1.7b`
/// are listed so that default can change without touching this catalog.
pub const MODELS: &[LlmModel] = &[QWEN3_4B_INSTRUCT_2507, QWEN3_8B, QWEN3_1_7B];

/// Look up a catalog model by id.
pub fn model(id: &str) -> Option<&'static LlmModel> {
    MODELS.iter().find(|m| m.id == id)
}

/// Look up one of a model's quantizations by its `bits` label.
pub fn quant<'a>(m: &'a LlmModel, bits: &str) -> Option<&'a Quant> {
    m.quants.iter().find(|q| q.bits == bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_looks_up_all_three_catalog_entries() {
        assert_eq!(
            model("qwen3-4b-instruct-2507").unwrap().id,
            "qwen3-4b-instruct-2507"
        );
        assert_eq!(model("qwen3-8b").unwrap().id, "qwen3-8b");
        assert_eq!(model("qwen3-1.7b").unwrap().id, "qwen3-1.7b");
        assert!(model("no-such-model").is_none());
    }

    #[test]
    fn quant_looks_up_q4_k_m_and_rejects_unknown_bits() {
        let m = model("qwen3-4b-instruct-2507").unwrap();
        let q = quant(m, "Q4_K_M").unwrap();
        assert_eq!(q.bytes, 2_497_281_120);
        assert_eq!(
            q.sha256,
            "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597"
        );
        assert_eq!(q.sources.len(), 3);
        assert!(quant(m, "Q8_0").is_none());
    }

    #[test]
    fn every_model_has_a_qwen3_header_and_at_least_one_quant() {
        for m in MODELS {
            assert!(!m.quants.is_empty(), "{} has no quants", m.id);
            assert!(m.header.block_count > 0, "{} has zero block_count", m.id);
        }
    }
}
