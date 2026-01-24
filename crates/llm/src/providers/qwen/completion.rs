//! QwenCompletionModel implementation.
//!
//! Implements the rig-core CompletionModel trait for Qwen models via DashScope API.

use rig::completion::{
    self, CompletionError, CompletionRequest, CompletionResponse, Document, ModelChoice,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{QwenClient, QwenUsage};

/// Completion model for Qwen via DashScope API.
///
/// Implements the rig-core `CompletionModel` trait for seamless integration
/// with the rig framework.
///
/// # Example
/// ```ignore
/// use nevoflux_llm::providers::qwen::QwenClient;
///
/// let client = QwenClient::new("your-api-key");
/// let model = client.completion_model("qwen-turbo");
///
/// // Use with rig's completion API
/// let response = model.completion_request("Hello!")
///     .preamble("You are a helpful assistant.")
///     .send()
///     .await?;
/// ```
#[derive(Clone)]
pub struct QwenCompletionModel {
    client: QwenClient,
    model: String,
}

impl QwenCompletionModel {
    /// Create a new completion model.
    pub fn new(client: QwenClient, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
        }
    }

    /// Get the model name.
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// API error response from DashScope.
#[derive(Debug, Deserialize)]
struct ApiErrorResponse {
    message: String,
}

/// API response wrapper for handling success and error responses.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ApiResponse<T> {
    Ok(T),
    Err(ApiErrorResponse),
}

/// Tool call structure for function calling.
#[derive(Debug, Deserialize)]
pub struct ToolCall {
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// Function call details.
#[derive(Debug, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Tool definition for Qwen API format.
#[derive(Clone, Debug, Serialize)]
pub struct QwenToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: completion::ToolDefinition,
}

impl From<completion::ToolDefinition> for QwenToolDefinition {
    fn from(tool: completion::ToolDefinition) -> Self {
        Self {
            tool_type: "function".into(),
            function: tool,
        }
    }
}

/// Extended message for completion responses with optional tool calls.
#[derive(Debug, Deserialize)]
pub struct CompletionMessage {
    #[allow(dead_code)]
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Extended choice for completion responses.
#[derive(Debug, Deserialize)]
pub struct CompletionChoice {
    #[allow(dead_code)]
    pub index: u32,
    pub message: CompletionMessage,
    #[allow(dead_code)]
    pub finish_reason: Option<String>,
}

/// Extended completion response that supports tool calls.
#[derive(Debug, Deserialize)]
pub struct QwenCompletionResponse {
    pub id: String,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: QwenUsage,
}

impl TryFrom<QwenCompletionResponse> for CompletionResponse<QwenCompletionResponse> {
    type Error = CompletionError;

    fn try_from(value: QwenCompletionResponse) -> Result<Self, Self::Error> {
        match value.choices.as_slice() {
            // Handle tool calls
            [CompletionChoice {
                message:
                    CompletionMessage {
                        tool_calls: Some(calls),
                        ..
                    },
                ..
            }, ..]
                if !calls.is_empty() =>
            {
                let call = calls.first().unwrap();
                Ok(CompletionResponse {
                    choice: ModelChoice::ToolCall(
                        call.function.name.clone(),
                        serde_json::from_str(&call.function.arguments)?,
                    ),
                    raw_response: value,
                })
            }
            // Handle text message
            [CompletionChoice {
                message:
                    CompletionMessage {
                        content: Some(content),
                        ..
                    },
                ..
            }, ..] => Ok(CompletionResponse {
                choice: ModelChoice::Message(content.clone()),
                raw_response: value,
            }),
            _ => Err(CompletionError::ResponseError(
                "Response did not contain a message or tool call".into(),
            )),
        }
    }
}

/// Build prompt with context documents (similar to rig's internal method).
fn prompt_with_context(prompt: &str, documents: &[Document]) -> String {
    if !documents.is_empty() {
        format!(
            "<attachments>\n{}</attachments>\n\n{}",
            documents
                .iter()
                .map(|doc| doc.to_string())
                .collect::<Vec<_>>()
                .join(""),
            prompt
        )
    } else {
        prompt.to_string()
    }
}

impl completion::CompletionModel for QwenCompletionModel {
    type Response = QwenCompletionResponse;

    async fn completion(
        &self,
        mut completion_request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        // Build messages starting with preamble (system message)
        let mut messages = if let Some(preamble) = &completion_request.preamble {
            vec![completion::Message {
                role: "system".into(),
                content: preamble.clone(),
            }]
        } else {
            vec![]
        };

        // Append chat history
        messages.append(&mut completion_request.chat_history);

        // Add the user's prompt with context documents
        let user_prompt = prompt_with_context(&completion_request.prompt, &completion_request.documents);
        messages.push(completion::Message {
            role: "user".into(),
            content: user_prompt,
        });

        // Build the request JSON
        let request = if completion_request.tools.is_empty() {
            json!({
                "model": self.model,
                "messages": messages,
                "temperature": completion_request.temperature,
                "max_tokens": completion_request.max_tokens,
                "stream": false,
            })
        } else {
            json!({
                "model": self.model,
                "messages": messages,
                "temperature": completion_request.temperature,
                "max_tokens": completion_request.max_tokens,
                "stream": false,
                "tools": completion_request.tools.into_iter().map(QwenToolDefinition::from).collect::<Vec<_>>(),
                "tool_choice": "auto",
            })
        };

        // Merge additional params if provided
        let final_request = if let Some(params) = completion_request.additional_params {
            merge_json(request, params)
        } else {
            request
        };

        // Send the request
        let response = self
            .client
            .http_client()
            .post(format!("{}/chat/completions", self.client.base_url()))
            .bearer_auth(self.client.api_key())
            .json(&final_request)
            .send()
            .await
            .map_err(|e| CompletionError::ProviderError(e.to_string()))?;

        if response.status().is_success() {
            match response
                .json::<ApiResponse<QwenCompletionResponse>>()
                .await
                .map_err(|e| CompletionError::ProviderError(e.to_string()))?
            {
                ApiResponse::Ok(resp) => resp.try_into(),
                ApiResponse::Err(err) => Err(CompletionError::ProviderError(err.message)),
            }
        } else {
            let error_text = response
                .text()
                .await
                .map_err(|e| CompletionError::ProviderError(e.to_string()))?;
            Err(CompletionError::ProviderError(error_text))
        }
    }
}

/// Merge two JSON values, with the second taking precedence.
fn merge_json(base: serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(mut base_map), serde_json::Value::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                base_map.insert(key, value);
            }
            serde_json::Value::Object(base_map)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::qwen::QwenClient;

    #[test]
    fn test_completion_model_new() {
        let client = QwenClient::new("test-key");
        let model = QwenCompletionModel::new(client, "qwen-turbo");
        assert_eq!(model.model(), "qwen-turbo");
    }

    #[test]
    fn test_completion_model_clone() {
        let client = QwenClient::new("test-key");
        let model = QwenCompletionModel::new(client, "qwen-plus");
        let cloned = model.clone();
        assert_eq!(cloned.model(), "qwen-plus");
    }

    #[test]
    fn test_completion_model_with_different_models() {
        let client = QwenClient::new("test-key");

        let turbo = QwenCompletionModel::new(client.clone(), "qwen-turbo");
        let plus = QwenCompletionModel::new(client.clone(), "qwen-plus");
        let max = QwenCompletionModel::new(client.clone(), "qwen-max");

        assert_eq!(turbo.model(), "qwen-turbo");
        assert_eq!(plus.model(), "qwen-plus");
        assert_eq!(max.model(), "qwen-max");
    }

    #[test]
    fn test_tool_definition_conversion() {
        let rig_tool = completion::ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get weather information".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"}
                }
            }),
        };

        let qwen_tool: QwenToolDefinition = rig_tool.into();
        assert_eq!(qwen_tool.tool_type, "function");
        assert_eq!(qwen_tool.function.name, "get_weather");
    }

    #[test]
    fn test_completion_response_deserialization() {
        let json = r#"{
            "id": "chatcmpl-123",
            "model": "qwen-turbo",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;

        let resp: QwenCompletionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, "chatcmpl-123");
        assert_eq!(resp.model, "qwen-turbo");
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("Hello!")
        );
        assert_eq!(resp.usage.total_tokens, 15);
    }

    #[test]
    fn test_completion_response_conversion_message() {
        let json = r#"{
            "id": "chatcmpl-123",
            "model": "qwen-turbo",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello there!"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;

        let resp: QwenCompletionResponse = serde_json::from_str(json).unwrap();
        let rig_resp: CompletionResponse<QwenCompletionResponse> = resp.try_into().unwrap();

        match rig_resp.choice {
            ModelChoice::Message(msg) => assert_eq!(msg, "Hello there!"),
            _ => panic!("Expected Message choice"),
        }
    }

    #[test]
    fn test_completion_response_conversion_tool_call() {
        let json = r#"{
            "id": "chatcmpl-456",
            "model": "qwen-turbo",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"location\": \"Tokyo\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 15,
                "completion_tokens": 10,
                "total_tokens": 25
            }
        }"#;

        let resp: QwenCompletionResponse = serde_json::from_str(json).unwrap();
        let rig_resp: CompletionResponse<QwenCompletionResponse> = resp.try_into().unwrap();

        match rig_resp.choice {
            ModelChoice::ToolCall(name, params) => {
                assert_eq!(name, "get_weather");
                assert_eq!(params["location"], "Tokyo");
            }
            _ => panic!("Expected ToolCall choice"),
        }
    }

    #[test]
    fn test_merge_json() {
        let base = serde_json::json!({
            "model": "qwen-turbo",
            "temperature": 0.7
        });
        let overlay = serde_json::json!({
            "temperature": 0.5,
            "top_p": 0.9
        });

        let merged = merge_json(base, overlay);
        assert_eq!(merged["model"], "qwen-turbo");
        assert_eq!(merged["temperature"], 0.5); // overlay wins
        assert_eq!(merged["top_p"], 0.9);
    }

    #[test]
    fn test_api_error_response_deserialization() {
        let json = r#"{"message": "Invalid API key"}"#;
        let err: ApiErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(err.message, "Invalid API key");
    }

    #[test]
    fn test_implements_completion_model_trait() {
        // Verify that QwenCompletionModel implements the CompletionModel trait
        fn assert_completion_model<T: completion::CompletionModel>() {}
        assert_completion_model::<QwenCompletionModel>();
    }

    #[test]
    fn test_completion_model_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<QwenCompletionModel>();
    }

    #[test]
    fn test_prompt_with_context_no_documents() {
        let result = prompt_with_context("Hello", &[]);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn test_prompt_with_context_with_documents() {
        use std::collections::HashMap;
        let docs = vec![Document {
            id: "doc1".to_string(),
            text: "Some content".to_string(),
            additional_props: HashMap::new(),
        }];
        let result = prompt_with_context("Hello", &docs);
        assert!(result.contains("<attachments>"));
        assert!(result.contains("doc1"));
        assert!(result.contains("Some content"));
        assert!(result.contains("Hello"));
    }

    #[test]
    fn test_empty_choices_returns_error() {
        let json = r#"{
            "id": "chatcmpl-123",
            "model": "qwen-turbo",
            "choices": [],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 0,
                "total_tokens": 10
            }
        }"#;

        let resp: QwenCompletionResponse = serde_json::from_str(json).unwrap();
        let result: Result<CompletionResponse<QwenCompletionResponse>, _> = resp.try_into();
        assert!(result.is_err());
    }
}
