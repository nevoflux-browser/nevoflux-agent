//! Gemini CLI provider implementation.
//!
//! Provides access to Gemini models via the Gemini CLI (`gemini` command)
//! as a subprocess.
//!
//! # Example
//! ```ignore
//! use nevoflux_llm::providers::gemini_cli::GeminiCliClient;
//!
//! let client = GeminiCliClient::new("gemini");
//! let model = client.completion_model("gemini-2.5-pro");
//! ```

mod client;
mod completion;
mod types;

pub use client::GeminiCliClient;
pub use completion::GeminiCliCompletionModel;
pub use types::*;
