//! Context building for LLM requests.

mod builder;
mod compressor;

pub use builder::{Context, ContextBuilder, ContextMessage, TokenBudget};
pub use compressor::{CompressionResult, ContextCompressor};
