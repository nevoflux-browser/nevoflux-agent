//! On-device (local) LLM inference.
//!
//! Task 1.1 adds only the `[llm.local]` config shape. Later tasks add the
//! model-download latch (`crate::local::latch`), a raw-HTTP local provider,
//! and an engine supervisor.

pub mod config;
pub use config::*;
