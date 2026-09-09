//! Recognising a provider's "your context is too big" error (design spec §5.1).
//!
//! # Why this is string matching
//!
//! There is no shared error code for this across providers. Anthropic says the
//! prompt is too long, OpenAI raises `context_length_exceeded`, DeepSeek and the
//! OpenAI-compatible endpoints echo variations, and Gemini talks about input
//! token counts. Matching on wording is unlovely, but the alternative — treating
//! every 400 as an overflow — would silently compact the conversation in
//! response to a malformed request, which loses user content to fix nothing.
//!
//! # Deliberately narrow
//!
//! A phrase only earns its place here if it cannot plausibly mean anything else.
//! A false positive costs real conversation history; a false negative costs one
//! failed turn, which is what happens today anyway. When in doubt, leave it out.

/// Phrases that mean "the prompt did not fit", lowercase.
///
/// Kept as one list rather than per-provider: an OpenAI-compatible endpoint may
/// be fronting any model, so the provider name is a poor key. Every phrase here
/// is specific enough to stand on its own.
const OVERFLOW_PHRASES: &[&str] = &[
    // OpenAI and OpenAI-compatible endpoints
    "context_length_exceeded",
    "maximum context length",
    "reduce the length of the messages",
    // Anthropic
    "prompt is too long",
    // Gemini
    "input token count",
    "exceeds the maximum number of tokens",
    // Common phrasings across compatible endpoints
    "context length",
    "context window",
    "too many tokens",
    "maximum context",
];

/// Whether this error says the request was too large to fit.
///
/// Case-insensitive substring matching over [`OVERFLOW_PHRASES`].
pub fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    OVERFLOW_PHRASES.iter().any(|p| lower.contains(p))
}

/// What a shrunk tool result is replaced with.
const ELIDED: &str = "[earlier tool result elided to fit the context window]";

/// Shrink old tool results in place until the request plausibly fits.
///
/// # Why this and not compression
///
/// The compressor is skipped whenever tool results are present, because
/// summarising them destroys the tool_call/tool_result chain the provider
/// requires — and a long agent loop full of tool results is exactly the shape
/// that overflows. So the overflow path cannot use it.
///
/// This only ever shortens the `content` of `tool` messages. No message is
/// removed and no id is touched, so every tool_call keeps its matching result
/// and the request stays well-formed. It is a smaller hammer than compression
/// on purpose: it can always be applied, including mid-loop.
///
/// Returns how many results were elided. Oldest first, keeping the most recent
/// `keep_recent` intact — the recent ones are what the model is reasoning about.
pub fn shrink_tool_results(
    messages: &mut [crate::wasm::llm::LlmMessage],
    keep_recent: usize,
    min_len: usize,
) -> usize {
    let tool_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "tool" && m.content.len() > min_len)
        .map(|(i, _)| i)
        .collect();

    let elidable = tool_positions.len().saturating_sub(keep_recent);
    for &i in tool_positions.iter().take(elidable) {
        messages[i].content = ELIDED.to_string();
    }
    elidable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_wordings_providers_send_are_recognised() {
        for msg in [
            // OpenAI
            "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.",
            "{\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"...\"}}",
            "Please reduce the length of the messages.",
            // Anthropic
            "prompt is too long: 210000 tokens > 200000 maximum",
            // Gemini
            "The input token count (1050000) exceeds the maximum number of tokens allowed",
            // Compatible endpoints
            "Error: context window exceeded for this model",
            "too many tokens in request",
        ] {
            assert!(is_context_overflow(msg), "not recognised: {msg}");
        }
    }

    /// A false positive costs real conversation history, so the ordinary
    /// failures must not look like an overflow.
    #[test]
    fn ordinary_failures_are_not_mistaken_for_an_overflow() {
        for msg in [
            "401 Unauthorized: invalid api key",
            "429 Too Many Requests: rate limit exceeded",
            "Connection reset by peer",
            "model `claude-opus-5` not found",
            "invalid_request_error: messages: at least one message is required",
            "500 Internal Server Error",
            "tool_use ids must be unique",
            "request timed out",
        ] {
            assert!(!is_context_overflow(msg), "false positive on: {msg}");
        }
    }

    #[test]
    fn matching_ignores_case() {
        assert!(is_context_overflow("CONTEXT_LENGTH_EXCEEDED"));
        assert!(is_context_overflow("Prompt Is Too Long"));
    }

    fn msg(role: &str, content: &str) -> crate::wasm::llm::LlmMessage {
        crate::wasm::llm::LlmMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some("t1".into()),
            attachments: vec![],
            reasoning: None,
        }
    }

    #[test]
    fn shrinking_elides_the_oldest_results_and_keeps_the_recent_ones() {
        let big = "x".repeat(5000);
        let mut messages = vec![
            msg("user", "go"),
            msg("tool", &big),
            msg("tool", &big),
            msg("tool", &big),
        ];

        let elided = shrink_tool_results(&mut messages, 1, 1000);
        assert_eq!(elided, 2);
        assert_eq!(messages[1].content, ELIDED);
        assert_eq!(messages[2].content, ELIDED);
        assert_eq!(messages[3].content.len(), 5000, "the newest survives");
        assert_eq!(messages[0].content, "go", "non-tool messages are untouched");
    }

    /// The whole point of shrinking rather than compressing: every message
    /// stays, so a tool_call keeps its matching result and the request is still
    /// well-formed.
    #[test]
    fn shrinking_removes_no_message_and_no_id() {
        let mut messages = vec![
            msg("tool", &"y".repeat(9000)),
            msg("tool", &"z".repeat(9000)),
        ];
        let before = messages.len();
        shrink_tool_results(&mut messages, 0, 100);
        assert_eq!(messages.len(), before);
        for m in &messages {
            assert_eq!(m.tool_call_id.as_deref(), Some("t1"));
            assert_eq!(m.role, "tool");
        }
    }

    #[test]
    fn a_small_result_is_left_alone() {
        let mut messages = vec![msg("tool", "ok")];
        assert_eq!(shrink_tool_results(&mut messages, 0, 1000), 0);
        assert_eq!(messages[0].content, "ok");
    }

    #[test]
    fn shrinking_twice_changes_nothing_the_second_time() {
        let mut messages = vec![
            msg("tool", &"x".repeat(5000)),
            msg("tool", &"x".repeat(5000)),
        ];
        let first = shrink_tool_results(&mut messages, 0, 1000);
        let second = shrink_tool_results(&mut messages, 0, 1000);
        assert_eq!(first, 2);
        assert_eq!(second, 0, "already elided results are below the threshold");
    }

    /// Rate limiting mentions tokens too, and compacting the conversation would
    /// do nothing for it — the request has to wait, not shrink.
    #[test]
    fn a_token_rate_limit_is_not_an_overflow() {
        assert!(!is_context_overflow(
            "rate_limit_error: request would exceed your organization's tokens per minute"
        ));
    }
}
