//! Claude Code CLI provider implementation.
//!
//! Provides access to Claude models via the Claude Code CLI (`claude` command)
//! as a subprocess.
//!
//! # Example
//! ```ignore
//! use nevoflux_llm::providers::claude_code::ClaudeCodeClient;
//!
//! let client = ClaudeCodeClient::new("claude");
//! let model = client.completion_model("sonnet");
//! ```

mod client;
mod completion;
mod types;

pub use client::ClaudeCodeClient;
pub use completion::ClaudeCodeCompletionModel;
pub use types::*;
