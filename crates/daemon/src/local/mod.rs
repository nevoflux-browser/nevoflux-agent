//! On-device (local) LLM inference.
//!
//! Task 1.1 added the `[llm.local]` config shape. Task 1.2 adds the
//! `LocalOnly` latch and its `egress_guard` (`crate::local::latch`). Later
//! tasks add a raw-HTTP local provider and an engine supervisor.

pub mod config;
pub mod latch;
pub use config::*;
