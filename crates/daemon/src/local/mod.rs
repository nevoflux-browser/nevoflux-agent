//! On-device (local) LLM inference.
//!
//! Task 1.1 added the `[llm.local]` config shape. Task 1.2 added the
//! `LocalOnly` latch and its `egress_guard` (`crate::local::latch`). Task
//! 1.4 adds the endpoint registry (`crate::local::endpoint`) and the
//! raw-HTTP provider itself (`crate::wasm::local_llm`). Task 1.6 adds
//! `crate::local::sync`: the latch's config-change hook, which keeps the
//! llm-gateway's upstream (`crate::llm_gateway::apply_latch`) and a
//! `system:local:latch_changed` broadcast (topic:
//! `crate::local::latch::TOPIC_LATCH`) in sync with the latch. Task 2.1
//! adds three pure-logic modules with no engine-process dependency:
//! `gguf` (a hand-rolled GGUF header reader), `catalog` (the known models
//! and their download sources), and `memory` (KV-cache/compute size
//! estimation and GPU/CPU fit selection). Task 2.2 adds `hardware`: a
//! [`hardware::probe`] of this host's NVIDIA/Vulkan/cudart/RAM situation,
//! never run at daemon startup, plus the pure [`hardware::fallback_chain`]
//! that turns a probe + backend preference into the ordered list of
//! [`hardware::InstallKind`]s to try. Task 2.3 adds `release`: the pinned
//! table of what the engine release's downloadable archives actually ARE
//! (name, size, sha256, mirror/original download sources), generated from a
//! verified staging manifest by `scripts/engine-release/gen_release_rs.py`.
//! A later task adds the engine supervisor that publishes to the endpoint
//! registry, calls [`apply_gateway_upstream_for_latch`] when it does, and
//! uses `catalog` + `memory` + `hardware` + `release` to decide what to
//! install/launch and how.

pub mod catalog;
pub mod config;
pub mod endpoint;
pub mod gguf;
pub mod hardware;
pub mod latch;
pub mod memory;
pub mod release;
pub mod sync;
pub use config::*;
pub use sync::{apply_gateway_upstream_for_latch, on_config_changed, publish_current_latch_state};
