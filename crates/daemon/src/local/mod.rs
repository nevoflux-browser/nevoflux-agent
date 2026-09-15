//! On-device (local) LLM inference.
//!
//! Task 1.1 added the `[llm.local]` config shape. Task 1.2 added the
//! `LocalOnly` latch and its `egress_guard` (`crate::local::latch`). Task
//! 1.4 adds the endpoint registry (`crate::local::endpoint`) and the
//! raw-HTTP provider itself (`crate::wasm::local_llm`). Task 1.6 adds
//! `crate::local::sync`: the latch's config-change hook, which keeps the
//! llm-gateway's upstream (`crate::llm_gateway::apply_latch`) and a
//! `system:local:latch_changed` broadcast (topic:
//! `crate::local::latch::TOPIC_LATCH`) in sync with the latch. A later
//! task adds the engine supervisor that publishes to the endpoint
//! registry and calls [`apply_gateway_upstream_for_latch`] when it does.

pub mod config;
pub mod endpoint;
pub mod latch;
pub mod sync;
pub use config::*;
pub use sync::{apply_gateway_upstream_for_latch, on_config_changed};
