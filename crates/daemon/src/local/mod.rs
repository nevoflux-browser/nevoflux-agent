//! On-device (local) LLM inference.
//!
//! Task 1.1 added the `[llm.local]` config shape. Task 1.2 added the
//! `LocalOnly` latch and its `egress_guard` (`crate::local::latch`). Task
//! 1.4 adds the endpoint registry (`crate::local::endpoint`) and the
//! raw-HTTP provider itself (`crate::wasm::local_llm`). A later task adds
//! the engine supervisor that publishes to the endpoint registry.

pub mod config;
pub mod endpoint;
pub mod latch;
pub use config::*;
