//! LLM host function implementation.
//!
//! This module provides the infrastructure for calling LLM providers
//! from Wasm guest modules via host functions.

use crate::error::{DaemonError, Result};
use nevoflux_llm::providers::qwen::QwenClient;
use nevoflux_llm::ProviderType;
use rig::completion::{CompletionModel, Message};
use serde::{Deserialize, Serialize};

/// Request structure for LLM chat operations.
///
/// This is the JSON structure that Wasm guests send to the `llm_chat` host function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmChatRequest {
    /// The messages to send to the LLM.
    pub messages: Vec<LlmMessage>,
    /// Optional system prompt.
    pub system: Option<String>,
    /// Optional temperature for response generation (0.0 - 1.0).
    pub temperature: Option<f32>,
    /// Optional maximum tokens to generate.
    pub max_tokens: Option<u32>,
}

/// A single message in an LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    /// The role of the message sender (e.g., "user", "assistant", "system").
    pub role: String,
    /// The content of the message.
    pub content: String,
}

/// Response structure from LLM chat operations.
///
/// This is the JSON structure returned to Wasm guests from the `llm_chat` host function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmChatResponse {
    /// The generated content from the LLM.
    pub content: String,
    /// The reason the generation stopped (e.g., "stop", "length").
    pub finish_reason: String,
    /// Optional usage statistics.
    pub usage: Option<LlmUsage>,
}

/// Token usage statistics for an LLM request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmUsage {
    /// Number of tokens in the prompt.
    pub prompt_tokens: u32,
    /// Number of tokens in the completion.
    pub completion_tokens: u32,
    /// Total tokens used (prompt + completion).
    pub total_tokens: u32,
}

/// Execute an LLM chat request.
///
/// This function routes the request to the appropriate provider based on the
/// configured provider type.
///
/// # Arguments
///
/// * `provider` - The type of LLM provider to use.
/// * `api_key` - The API key for authentication.
/// * `model` - The model name to use.
/// * `request` - The chat request containing messages.
///
/// # Returns
///
/// Returns a `LlmChatResponse` on success, or a `DaemonError` on failure.
///
/// # Errors
///
/// Returns an error if:
/// - The provider is not implemented
/// - The LLM API call fails
/// - Response parsing fails
pub async fn execute_llm_chat(
    provider: ProviderType,
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    match provider {
        ProviderType::Qwen => execute_qwen_chat(api_key, model, request).await,
        _ => Err(DaemonError::InternalError(format!(
            "Provider {:?} not yet implemented",
            provider
        ))),
    }
}

/// Execute a chat request using the Qwen provider.
async fn execute_qwen_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client = QwenClient::new(api_key);
    let completion_model = client.completion_model(model);

    // Convert request messages to rig messages, separating system from chat history
    let mut chat_history: Vec<Message> = Vec::new();
    let mut system_prompt: Option<String> = request.system.clone();

    for msg in &request.messages {
        if msg.role == "system" {
            // Use the last system message if multiple are provided
            system_prompt = Some(msg.content.clone());
        } else {
            chat_history.push(Message {
                role: msg.role.clone(),
                content: msg.content.clone(),
            });
        }
    }

    // Get the last user message as the prompt
    let (prompt, chat_history) = if chat_history.is_empty() {
        return Err(DaemonError::InternalError(
            "LLM chat requires at least one user message".into(),
        ));
    } else {
        // Find the last user message to use as the prompt
        let last_user_idx = chat_history
            .iter()
            .rposition(|m| m.role == "user")
            .unwrap_or(chat_history.len() - 1);

        let prompt = chat_history[last_user_idx].content.clone();
        let mut history = chat_history;
        history.remove(last_user_idx);
        (prompt, history)
    };

    // Build the completion request using the builder pattern
    let mut builder = completion_model.completion_request(&prompt);

    // Add system prompt (preamble) if present
    if let Some(preamble) = system_prompt {
        builder = builder.preamble(preamble);
    }

    // Add chat history
    builder = builder.messages(chat_history);

    // Execute the request
    let completion_response = builder
        .send()
        .await
        .map_err(|e| DaemonError::InternalError(format!("LLM chat failed: {}", e)))?;

    // Extract the response content
    let content = match completion_response.choice {
        rig::completion::ModelChoice::Message(msg) => msg,
        rig::completion::ModelChoice::ToolCall(name, _) => {
            format!("Tool call: {}", name)
        }
    };

    Ok(LlmChatResponse {
        content,
        finish_reason: "stop".into(),
        usage: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_message_serialization() {
        let message = LlmMessage {
            role: "user".into(),
            content: "Hello, world!".into(),
        };

        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("user"));
        assert!(json.contains("Hello, world!"));

        let parsed: LlmMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, "user");
        assert_eq!(parsed.content, "Hello, world!");
    }

    #[test]
    fn test_llm_request_serialization() {
        let request = LlmChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "Hello".into(),
            }],
            system: Some("You are helpful.".into()),
            temperature: Some(0.7),
            max_tokens: Some(1000),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("Hello"));
        assert!(json.contains("You are helpful."));
        assert!(json.contains("0.7"));
        assert!(json.contains("1000"));

        let parsed: LlmChatRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.system, Some("You are helpful.".into()));
        assert_eq!(parsed.temperature, Some(0.7));
        assert_eq!(parsed.max_tokens, Some(1000));
    }

    #[test]
    fn test_llm_request_minimal() {
        let request = LlmChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "Hi".into(),
            }],
            system: None,
            temperature: None,
            max_tokens: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: LlmChatRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.messages.len(), 1);
        assert!(parsed.system.is_none());
        assert!(parsed.temperature.is_none());
        assert!(parsed.max_tokens.is_none());
    }

    #[test]
    fn test_llm_response_serialization() {
        let response = LlmChatResponse {
            content: "Hi there!".into(),
            finish_reason: "stop".into(),
            usage: Some(LlmUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("Hi there!"));
        assert!(json.contains("stop"));
        assert!(json.contains("15"));

        let parsed: LlmChatResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content, "Hi there!");
        assert_eq!(parsed.finish_reason, "stop");
        assert!(parsed.usage.is_some());
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn test_llm_response_without_usage() {
        let response = LlmChatResponse {
            content: "Hello!".into(),
            finish_reason: "stop".into(),
            usage: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: LlmChatResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content, "Hello!");
        assert!(parsed.usage.is_none());
    }

    #[test]
    fn test_llm_usage_serialization() {
        let usage = LlmUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
        };

        let json = serde_json::to_string(&usage).unwrap();
        let parsed: LlmUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.prompt_tokens, 100);
        assert_eq!(parsed.completion_tokens, 50);
        assert_eq!(parsed.total_tokens, 150);
    }

    #[test]
    fn test_llm_message_clone() {
        let message = LlmMessage {
            role: "assistant".into(),
            content: "Test".into(),
        };
        let cloned = message.clone();
        assert_eq!(cloned.role, message.role);
        assert_eq!(cloned.content, message.content);
    }

    #[test]
    fn test_llm_request_clone() {
        let request = LlmChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "Test".into(),
            }],
            system: Some("System".into()),
            temperature: Some(0.5),
            max_tokens: Some(500),
        };
        let cloned = request.clone();
        assert_eq!(cloned.messages.len(), 1);
        assert_eq!(cloned.system, Some("System".into()));
    }

    #[test]
    fn test_llm_response_clone() {
        let response = LlmChatResponse {
            content: "Response".into(),
            finish_reason: "stop".into(),
            usage: None,
        };
        let cloned = response.clone();
        assert_eq!(cloned.content, "Response");
    }

    #[test]
    fn test_llm_usage_clone() {
        let usage = LlmUsage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        };
        let cloned = usage.clone();
        assert_eq!(cloned.total_tokens, 3);
    }

    #[test]
    fn test_llm_message_debug() {
        let message = LlmMessage {
            role: "user".into(),
            content: "Debug test".into(),
        };
        let debug = format!("{:?}", message);
        assert!(debug.contains("LlmMessage"));
        assert!(debug.contains("user"));
    }

    #[test]
    fn test_llm_request_debug() {
        let request = LlmChatRequest {
            messages: vec![],
            system: None,
            temperature: None,
            max_tokens: None,
        };
        let debug = format!("{:?}", request);
        assert!(debug.contains("LlmChatRequest"));
    }

    #[test]
    fn test_llm_response_debug() {
        let response = LlmChatResponse {
            content: "Test".into(),
            finish_reason: "stop".into(),
            usage: None,
        };
        let debug = format!("{:?}", response);
        assert!(debug.contains("LlmChatResponse"));
    }

    #[test]
    fn test_llm_usage_debug() {
        let usage = LlmUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        };
        let debug = format!("{:?}", usage);
        assert!(debug.contains("LlmUsage"));
    }

    #[test]
    fn test_llm_request_multiple_messages() {
        let request = LlmChatRequest {
            messages: vec![
                LlmMessage {
                    role: "user".into(),
                    content: "Hello".into(),
                },
                LlmMessage {
                    role: "assistant".into(),
                    content: "Hi!".into(),
                },
                LlmMessage {
                    role: "user".into(),
                    content: "How are you?".into(),
                },
            ],
            system: None,
            temperature: None,
            max_tokens: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: LlmChatRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].role, "user");
        assert_eq!(parsed.messages[1].role, "assistant");
        assert_eq!(parsed.messages[2].role, "user");
    }

    #[test]
    fn test_llm_request_from_json_string() {
        let json = r#"{
            "messages": [
                {"role": "user", "content": "What is 2+2?"}
            ],
            "system": "You are a math tutor.",
            "temperature": 0.0,
            "max_tokens": 100
        }"#;

        let request: LlmChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].content, "What is 2+2?");
        assert_eq!(request.system, Some("You are a math tutor.".into()));
        assert_eq!(request.temperature, Some(0.0));
        assert_eq!(request.max_tokens, Some(100));
    }

    #[test]
    fn test_llm_response_to_json_string() {
        let response = LlmChatResponse {
            content: "The answer is 4.".into(),
            finish_reason: "stop".into(),
            usage: Some(LlmUsage {
                prompt_tokens: 20,
                completion_tokens: 10,
                total_tokens: 30,
            }),
        };

        let json = serde_json::to_string_pretty(&response).unwrap();
        assert!(json.contains("The answer is 4."));
        assert!(json.contains("\"total_tokens\": 30"));
    }
}
