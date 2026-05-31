//! Protocol the upstream LLM endpoint speaks.
//!
//! Added in M4-2.6 to let the daemon route to either an Anthropic
//! Messages API upstream (existing M2 translator path) or an OpenAI
//! Chat Completions API upstream (new passthrough path). The choice is
//! made at [`crate::GatewayConfig`] construction time and dispatched
//! inside `chat_completions`.

use serde::{Deserialize, Serialize};

/// Protocol the upstream LLM endpoint speaks. Determines whether the
/// gateway translates (Anthropic) or passes through (OpenAI).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamProtocol {
    /// Upstream speaks Anthropic Messages API (`POST /v1/messages`,
    /// `x-api-key` header, `anthropic-version` header, response shape
    /// `content: [{type:"text"|"tool_use", ...}]`). Gateway runs the
    /// OpenAI ↔ Anthropic translator path from M2.
    Anthropic,
    /// Upstream speaks OpenAI Chat Completions API
    /// (`POST /v1/chat/completions`, `Authorization: Bearer ...` header,
    /// response shape `choices: [{message:{content,tool_calls}}]`).
    /// Gateway forwards the client request unchanged except for an auth
    /// swap and an optional model remap.
    #[serde(rename = "openai")]
    OpenAi,
    /// Upstream is a Claude Code **ACP** session driven in-process by
    /// the gateway. Chat requests are served by spawning/reusing a single
    /// `claude-agent-acp` subprocess and opening a new ACP session per
    /// request (tool-less, headless), reusing Claude Code's own auth.
    /// See `acp_upstream.rs`. The `/v1/embeddings` path never uses this.
    Acp,
}

impl Default for UpstreamProtocol {
    fn default() -> Self {
        // Maintain back-compat with M2: pre-existing GatewayConfig users
        // were always Anthropic.
        UpstreamProtocol::Anthropic
    }
}

impl UpstreamProtocol {
    /// Map a nevoflux provider name (from `[llm].provider`) to its
    /// canonical protocol. `anthropic` uses the Anthropic Messages API;
    /// `claude_code`/`claude-code`/`acp` drive an in-process Claude Code
    /// ACP session; everything else is OpenAI-compatible by convention.
    pub fn from_provider_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "anthropic" => UpstreamProtocol::Anthropic,
            "claude_code" | "claude-code" | "acp" => UpstreamProtocol::Acp,
            _ => UpstreamProtocol::OpenAi,
        }
    }

    /// Parse a string label coming from a config file or env var into a
    /// protocol enum. Accepts `"anthropic"` and the canonical OpenAI
    /// aliases (`"openai"`, `"open_ai"`). Anything else falls back to
    /// the default ([`UpstreamProtocol::Anthropic`]).
    pub fn parse_label(label: &str) -> Self {
        match label.trim().to_ascii_lowercase().as_str() {
            "anthropic" => UpstreamProtocol::Anthropic,
            "openai" | "open_ai" | "open-ai" => UpstreamProtocol::OpenAi,
            "acp" => UpstreamProtocol::Acp,
            _ => UpstreamProtocol::default(),
        }
    }
}
