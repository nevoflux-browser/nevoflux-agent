//! Context compression for LLM requests.
//!
//! This module provides automatic context compression using LLM summarization
//! when the conversation history exceeds a configurable token threshold.

use crate::config::AgentConfig;
use crate::context::ContextMessage;
use crate::error::{DaemonError, Result};
use crate::wasm::llm::{execute_llm_chat, LlmChatRequest, LlmMessage};
use nevoflux_llm::ProviderType;
use std::str::FromStr;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::{debug, warn};

/// Result of a compression attempt.
#[derive(Debug)]
pub enum CompressionResult {
    /// Compression was performed successfully.
    Compressed {
        /// The generated summary of older messages.
        summary: String,
        /// Recent messages that were kept uncompressed.
        recent: Vec<ContextMessage>,
        /// Estimated tokens saved by compression.
        saved: u32,
    },
    /// Compression was not needed (under threshold).
    NotNeeded,
    /// Compression was skipped for a specific reason.
    Skipped {
        /// Reason compression was skipped.
        reason: String,
    },
}

/// Context compressor that summarizes old messages when token budget is exceeded.
pub struct ContextCompressor {
    config: Arc<AgentConfig>,
    runtime: Handle,
}

impl ContextCompressor {
    /// Create a new context compressor.
    pub fn new(config: Arc<AgentConfig>, runtime: Handle) -> Self {
        Self { config, runtime }
    }

    /// Check if compression should trigger based on current and threshold values.
    ///
    /// Returns true if estimated_tokens > budget * threshold_percent / 100.
    pub fn should_compress(
        estimated_tokens: u32,
        token_budget: u32,
        threshold_percent: u32,
    ) -> bool {
        let threshold = (token_budget as u64 * threshold_percent as u64 / 100) as u32;
        estimated_tokens > threshold
    }

    /// Estimate token count for a slice of messages.
    ///
    /// Uses a rough approximation of 4 characters per token.
    pub fn estimate_tokens(messages: &[ContextMessage]) -> u32 {
        let chars: usize = messages
            .iter()
            .map(|m| m.role.len() + m.content.len())
            .sum();
        (chars / 4) as u32
    }

    /// Compress context if needed.
    ///
    /// This is a blocking operation that runs async code internally.
    pub fn compress_if_needed(
        &self,
        messages: &[ContextMessage],
        estimated_tokens: u32,
        token_budget: u32,
    ) -> CompressionResult {
        let context_config = &self.config.daemon.context;

        // Check if compression is enabled
        if !context_config.enable_compression {
            return CompressionResult::Skipped {
                reason: "Compression disabled in config".into(),
            };
        }

        // Check if we're over threshold
        if !Self::should_compress(
            estimated_tokens,
            token_budget,
            context_config.compression_threshold_percent,
        ) {
            return CompressionResult::NotNeeded;
        }

        // Check if we have enough messages to compress
        let keep_recent = context_config.keep_recent_messages as usize;
        if messages.len() <= keep_recent {
            return CompressionResult::Skipped {
                reason: format!(
                    "Not enough messages to compress ({} <= {})",
                    messages.len(),
                    keep_recent
                ),
            };
        }

        // Split messages: older ones to summarize, recent ones to keep
        let split_point = messages.len().saturating_sub(keep_recent);
        let to_summarize = &messages[..split_point];
        let recent = messages[split_point..].to_vec();

        // Calculate tokens before compression
        let tokens_before = Self::estimate_tokens(to_summarize);

        // Generate summary
        let summary = match self.generate_summary_blocking(to_summarize) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to generate summary: {}", e);
                return CompressionResult::Skipped {
                    reason: format!("Summary generation failed: {}", e),
                };
            }
        };

        // Calculate tokens after compression (summary token estimate)
        let tokens_after = (summary.len() / 4) as u32;
        let saved = tokens_before.saturating_sub(tokens_after);

        debug!(
            "Compressed {} messages ({} tokens) into summary ({} tokens), saved {} tokens",
            to_summarize.len(),
            tokens_before,
            tokens_after,
            saved
        );

        CompressionResult::Compressed {
            summary,
            recent,
            saved,
        }
    }

    /// Generate a summary of messages using blocking execution.
    fn generate_summary_blocking(&self, messages: &[ContextMessage]) -> Result<String> {
        let runtime = self.runtime.clone();
        let config = self.config.clone();
        let messages = messages.to_vec();

        tokio::task::block_in_place(|| {
            runtime.block_on(async move { generate_summary(&config, &messages).await })
        })
    }
}

/// Generate a summary of messages using the configured summarization model.
async fn generate_summary(config: &AgentConfig, messages: &[ContextMessage]) -> Result<String> {
    // Get provider configuration for summarization
    // Default to OpenAI for gpt-4o-mini if no provider is set
    let summarization_model = config
        .daemon
        .context
        .summarization_model
        .as_deref()
        .unwrap_or("gpt-4o-mini");

    // Determine provider based on model name
    let (provider, api_key) = get_summarization_provider(config, summarization_model)?;

    // Build the summarization request
    let conversation_text = format_messages_for_summary(messages);

    let system_prompt = r#"You are a conversation summarizer. Create a brief summary that captures:
1. Main topics discussed
2. Decisions or outcomes reached
3. Key facts the assistant needs to remember
Be concise (2-4 sentences)."#;

    let user_prompt = format!("Summarize this conversation:\n\n{}", conversation_text);

    let request = LlmChatRequest {
        messages: vec![LlmMessage::user(user_prompt)],
        system: Some(system_prompt.into()),
        temperature: Some(0.3), // Lower temperature for more consistent summaries
        max_tokens: Some(config.daemon.context.summary_max_tokens),
        tools: None,
    };

    debug!(
        "Generating summary using model={}, provider={:?}",
        summarization_model, provider
    );

    let response = execute_llm_chat(provider, &api_key, summarization_model, request).await?;

    Ok(response.content)
}

/// Get the appropriate provider and API key for summarization.
fn get_summarization_provider(config: &AgentConfig, model: &str) -> Result<(ProviderType, String)> {
    // Try to infer provider from model name
    let provider = if model.starts_with("gpt-") || model.starts_with("o1") {
        ProviderType::OpenAi
    } else if model.starts_with("claude-") {
        ProviderType::Anthropic
    } else if model.starts_with("qwen") {
        ProviderType::Qwen
    } else if model.starts_with("deepseek") {
        ProviderType::DeepSeek
    } else if model.starts_with("gemini") {
        ProviderType::Gemini
    } else {
        // Fall back to configured provider
        config
            .llm
            .active_provider()
            .and_then(|p| ProviderType::from_str(p).ok())
            .ok_or_else(|| {
                DaemonError::InternalError(
                    "No provider configured and cannot infer from model name".into(),
                )
            })?
    };

    // Get API key for the provider
    let api_key = match provider {
        ProviderType::OpenAi => config
            .llm
            .openai
            .api_key
            .clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok()),
        ProviderType::Anthropic => config
            .llm
            .anthropic
            .api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok()),
        ProviderType::Qwen => config
            .llm
            .qwen
            .api_key
            .clone()
            .or_else(|| std::env::var("DASHSCOPE_API_KEY").ok()),
        ProviderType::DeepSeek => config
            .llm
            .deepseek
            .api_key
            .clone()
            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok()),
        _ => config.llm.active_api_key().map(|s| s.to_string()),
    };

    let api_key = api_key.ok_or_else(|| {
        DaemonError::InternalError(format!(
            "No API key configured for summarization provider {:?}",
            provider
        ))
    })?;

    Ok((provider, api_key))
}

/// Format messages for the summary prompt.
fn format_messages_for_summary(messages: &[ContextMessage]) -> String {
    messages
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_compress_over_threshold() {
        // 8000 tokens, 10000 budget, 80% threshold (8000)
        // 8000 > 8000 is false, so not triggered
        assert!(!ContextCompressor::should_compress(8000, 10000, 80));

        // 8001 tokens, 10000 budget, 80% threshold (8000)
        // 8001 > 8000 is true
        assert!(ContextCompressor::should_compress(8001, 10000, 80));
    }

    #[test]
    fn test_should_compress_under_threshold() {
        // 5000 tokens, 10000 budget, 80% threshold
        assert!(!ContextCompressor::should_compress(5000, 10000, 80));
    }

    #[test]
    fn test_should_compress_at_threshold() {
        // Exactly at threshold should not trigger
        assert!(!ContextCompressor::should_compress(8000, 10000, 80));
    }

    #[test]
    fn test_estimate_tokens() {
        let messages = vec![
            ContextMessage {
                role: "user".into(),      // 4 chars
                content: "Hello!".into(), // 6 chars
            },
            ContextMessage {
                role: "assistant".into(),    // 9 chars
                content: "Hi there!".into(), // 9 chars
            },
        ];
        // Total = 28 chars, 28 / 4 = 7 tokens
        assert_eq!(ContextCompressor::estimate_tokens(&messages), 7);
    }

    #[test]
    fn test_estimate_tokens_empty() {
        let messages: Vec<ContextMessage> = vec![];
        assert_eq!(ContextCompressor::estimate_tokens(&messages), 0);
    }

    #[test]
    fn test_format_messages_for_summary() {
        let messages = vec![
            ContextMessage {
                role: "user".into(),
                content: "What's the weather?".into(),
            },
            ContextMessage {
                role: "assistant".into(),
                content: "It's sunny today.".into(),
            },
        ];

        let formatted = format_messages_for_summary(&messages);
        assert!(formatted.contains("user: What's the weather?"));
        assert!(formatted.contains("assistant: It's sunny today."));
    }

    #[test]
    fn test_compression_result_debug() {
        let result = CompressionResult::NotNeeded;
        let debug = format!("{:?}", result);
        assert!(debug.contains("NotNeeded"));

        let result = CompressionResult::Skipped {
            reason: "test".into(),
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("Skipped"));

        let result = CompressionResult::Compressed {
            summary: "Summary".into(),
            recent: vec![],
            saved: 100,
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("Compressed"));
    }

    #[test]
    fn test_compress_if_needed_disabled() {
        let mut config = AgentConfig::default();
        config.daemon.context.enable_compression = false;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let compressor = ContextCompressor::new(Arc::new(config), rt.handle().clone());

        let messages = vec![ContextMessage {
            role: "user".into(),
            content: "test".into(),
        }];

        let result = compressor.compress_if_needed(&messages, 10000, 5000);
        match result {
            CompressionResult::Skipped { reason } => {
                assert!(reason.contains("disabled"));
            }
            _ => panic!("Expected Skipped result"),
        }
    }

    #[test]
    fn test_compress_if_needed_under_threshold() {
        let config = AgentConfig::default();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let compressor = ContextCompressor::new(Arc::new(config), rt.handle().clone());

        let messages = vec![ContextMessage {
            role: "user".into(),
            content: "test".into(),
        }];

        // Under threshold: 1000 tokens, 10000 budget, 80% threshold = 8000
        let result = compressor.compress_if_needed(&messages, 1000, 10000);
        match result {
            CompressionResult::NotNeeded => {}
            _ => panic!("Expected NotNeeded result"),
        }
    }

    #[test]
    fn test_compress_if_needed_too_few_messages() {
        let mut config = AgentConfig::default();
        config.daemon.context.keep_recent_messages = 6;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let compressor = ContextCompressor::new(Arc::new(config), rt.handle().clone());

        // Only 5 messages, keep_recent is 6
        let messages: Vec<ContextMessage> = (0..5)
            .map(|i| ContextMessage {
                role: "user".into(),
                content: format!("Message {}", i),
            })
            .collect();

        // Over threshold to trigger compression attempt
        let result = compressor.compress_if_needed(&messages, 9000, 10000);
        match result {
            CompressionResult::Skipped { reason } => {
                assert!(reason.contains("Not enough messages"));
            }
            _ => panic!("Expected Skipped result, got {:?}", result),
        }
    }

    #[test]
    fn test_get_summarization_provider_openai() {
        let config = AgentConfig::default();
        // Without API key, should fail
        let result = get_summarization_provider(&config, "gpt-4o-mini");
        assert!(result.is_err());
    }

    #[test]
    fn test_get_summarization_provider_with_env() {
        // This test checks the logic but may not actually have env vars set
        let mut config = AgentConfig::default();
        config.llm.openai.api_key = Some("test-key".into());

        let result = get_summarization_provider(&config, "gpt-4o-mini");
        assert!(result.is_ok());
        let (provider, key) = result.unwrap();
        assert!(matches!(provider, ProviderType::OpenAi));
        assert_eq!(key, "test-key");
    }

    #[test]
    fn test_get_summarization_provider_anthropic() {
        let mut config = AgentConfig::default();
        config.llm.anthropic.api_key = Some("sk-ant-test".into());

        let result = get_summarization_provider(&config, "claude-3-haiku");
        assert!(result.is_ok());
        let (provider, _) = result.unwrap();
        assert!(matches!(provider, ProviderType::Anthropic));
    }
}
