//! NevoFlux LLM - LLM Provider abstraction layer
//!
//! Built on rig-core with additional providers (Qwen/DashScope).
//!
//! # Built-in Providers (via rig)
//! - Anthropic: `rig::providers::anthropic`
//! - OpenAI: `rig::providers::openai`
//! - OpenRouter: `rig::providers::openrouter`
//! - DeepSeek: `rig::providers::deepseek`
//!
//! # Custom Providers
//! - Qwen (DashScope): `nevoflux_llm::providers::qwen`

pub mod error;
pub mod providers;

// Re-export rig for convenience
pub use rig;

pub use error::{LlmError, Result};
