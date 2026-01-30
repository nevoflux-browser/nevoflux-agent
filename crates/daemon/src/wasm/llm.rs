//! LLM host function implementation.
//!
//! This module provides the infrastructure for calling LLM providers
//! from Wasm guest modules via host functions.

use crate::error::{DaemonError, Result};
use futures::StreamExt;
use nevoflux_llm::providers::qwen::QwenClient;
use nevoflux_llm::ProviderType;
use rig::client::CompletionClient;
use rig::client::Nothing;
use rig::completion::{CompletionModel, ToolDefinition};
use rig::message::{
    AssistantContent, DocumentSourceKind, Image, ImageMediaType, Message, UserContent,
};
use rig::providers::{
    anthropic, cohere, deepseek, gemini, groq, mistral, ollama, openai, openrouter, perplexity,
    together, xai,
};
use rig::streaming::{StreamedAssistantContent, ToolCallDeltaContent};
use rig::OneOrMany;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

/// Tool definition for LLM function calling.
///
/// Defines a tool that the LLM can invoke during the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolDefinition {
    /// The name of the tool/function.
    pub name: String,
    /// A description of what the tool does.
    pub description: String,
    /// JSON Schema defining the tool's parameters.
    pub parameters: Value,
}

impl From<LlmToolDefinition> for ToolDefinition {
    fn from(tool: LlmToolDefinition) -> Self {
        ToolDefinition {
            name: tool.name,
            description: tool.description,
            parameters: tool.parameters,
        }
    }
}

/// A tool call made by the LLM.
///
/// Represents a request from the LLM to invoke a specific tool with given arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolCall {
    /// Unique identifier for this tool call (used to match with tool results).
    pub id: String,
    /// The name of the tool to invoke.
    pub name: String,
    /// The arguments to pass to the tool (JSON object).
    pub arguments: Value,
}

/// Request structure for LLM chat operations.
///
/// This is the JSON structure that Wasm guests send to the `llm_chat` host function.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmChatRequest {
    /// The messages to send to the LLM.
    pub messages: Vec<LlmMessage>,
    /// Optional system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Optional temperature for response generation (0.0 - 1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Optional maximum tokens to generate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Optional list of tools the LLM can use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<LlmToolDefinition>>,
}

/// A single message in an LLM conversation.
///
/// Supports multiple roles:
/// - `user`: A message from the user
/// - `assistant`: A response from the LLM (may include tool_calls)
/// - `system`: A system instruction
/// - `tool`: A tool execution result (requires tool_call_id)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    /// The role of the message sender.
    pub role: String,
    /// The text content of the message (may be empty for tool-calling assistant messages).
    #[serde(default)]
    pub content: String,
    /// Tool calls made by the assistant (only present when role is "assistant").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<LlmToolCall>>,
    /// The ID of the tool call this message is responding to (only when role is "tool").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Attachments for multimodal messages (images, files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<LlmAttachment>,
}

impl LlmMessage {
    /// Create a new user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: Vec::new(),
        }
    }

    /// Create a new user message with attachments.
    pub fn user_with_attachments(
        content: impl Into<String>,
        attachments: Vec<LlmAttachment>,
    ) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            attachments,
        }
    }

    /// Create a new assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: Vec::new(),
        }
    }

    /// Create an assistant message with tool calls.
    pub fn assistant_with_tool_calls(tool_calls: Vec<LlmToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            attachments: Vec::new(),
        }
    }

    /// Create a tool result message.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            attachments: Vec::new(),
        }
    }
}

/// Response structure from LLM chat operations.
///
/// This is the JSON structure returned to Wasm guests from the `llm_chat` host function.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmChatResponse {
    /// The generated text content from the LLM (may be empty if tool_calls is present).
    #[serde(default)]
    pub content: String,
    /// The reason the generation stopped (e.g., "stop", "tool_calls", "length").
    #[serde(default)]
    pub finish_reason: String,
    /// Tool calls requested by the LLM (present when finish_reason is "tool_calls").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<LlmToolCall>>,
    /// Optional usage statistics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// Attachment for multimodal messages (images, files, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmAttachment {
    /// Attachment name/filename.
    pub name: String,
    /// MIME type (e.g., "image/png", "image/jpeg", "application/pdf").
    pub mime_type: String,
    /// Base64 encoded data.
    pub data: String,
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
        ProviderType::Anthropic => execute_anthropic_chat(api_key, model, request).await,
        ProviderType::OpenAi => execute_openai_chat(api_key, model, request).await,
        ProviderType::OpenRouter => execute_openrouter_chat(api_key, model, request).await,
        ProviderType::DeepSeek => execute_deepseek_chat(api_key, model, request).await,
        ProviderType::Qwen => execute_qwen_chat(api_key, model, request).await,
        ProviderType::Gemini => execute_gemini_chat(api_key, model, request).await,
        ProviderType::Groq => execute_groq_chat(api_key, model, request).await,
        ProviderType::Ollama => execute_ollama_chat(api_key, model, request).await,
        ProviderType::Mistral => execute_mistral_chat(api_key, model, request).await,
        ProviderType::XAi => execute_xai_chat(api_key, model, request).await,
        ProviderType::Cohere => execute_cohere_chat(api_key, model, request).await,
        ProviderType::Perplexity => execute_perplexity_chat(api_key, model, request).await,
        ProviderType::Together => execute_together_chat(api_key, model, request).await,
    }
}

/// Execute a chat request using the Anthropic provider.
async fn execute_anthropic_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: anthropic::Client = anthropic::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create Anthropic client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the OpenAI provider.
async fn execute_openai_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: openai::Client =
        openai::Client::builder()
            .api_key(api_key)
            .build()
            .map_err(|e| {
                DaemonError::InternalError(format!("Failed to create OpenAI client: {}", e))
            })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the OpenRouter provider (native rig provider).
async fn execute_openrouter_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: openrouter::Client = openrouter::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create OpenRouter client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the DeepSeek provider (native rig provider).
async fn execute_deepseek_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: deepseek::Client = deepseek::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create DeepSeek client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the Qwen provider (custom implementation).
async fn execute_qwen_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client = QwenClient::new(api_key);
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the Google Gemini provider.
async fn execute_gemini_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: gemini::Client =
        gemini::Client::builder()
            .api_key(api_key)
            .build()
            .map_err(|e| {
                DaemonError::InternalError(format!("Failed to create Gemini client: {}", e))
            })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the Groq provider.
async fn execute_groq_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: groq::Client = groq::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| DaemonError::InternalError(format!("Failed to create Groq client: {}", e)))?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the Ollama provider (local models).
async fn execute_ollama_chat(
    _api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    // Ollama doesn't need an API key for local usage, use Nothing
    let client: ollama::Client =
        ollama::Client::builder()
            .api_key(Nothing)
            .build()
            .map_err(|e| {
                DaemonError::InternalError(format!("Failed to create Ollama client: {}", e))
            })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the Mistral provider.
async fn execute_mistral_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: mistral::Client = mistral::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create Mistral client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the xAI (Grok) provider.
async fn execute_xai_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: xai::Client = xai::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| DaemonError::InternalError(format!("Failed to create xAI client: {}", e)))?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the Cohere provider.
async fn execute_cohere_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: cohere::Client =
        cohere::Client::builder()
            .api_key(api_key)
            .build()
            .map_err(|e| {
                DaemonError::InternalError(format!("Failed to create Cohere client: {}", e))
            })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the Perplexity provider.
async fn execute_perplexity_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: perplexity::Client = perplexity::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create Perplexity client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Execute a chat request using the Together AI provider.
async fn execute_together_chat(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
) -> Result<LlmChatResponse> {
    let client: together::Client = together::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create Together client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    execute_rig_completion(completion_model, request).await
}

/// Generic completion execution for any rig-compatible model.
async fn execute_rig_completion<M>(
    completion_model: M,
    request: LlmChatRequest,
) -> Result<LlmChatResponse>
where
    M: CompletionModel,
{
    // Convert request messages to rig messages, separating system from chat history
    let mut chat_history: Vec<Message> = Vec::new();
    let mut system_prompt: Option<String> = request.system.clone();

    for msg in &request.messages {
        match msg.role.as_str() {
            "system" => {
                // Use the last system message if multiple are provided
                system_prompt = Some(msg.content.clone());
            }
            "tool" => {
                // Tool results are included as user messages with tool_result content
                let tool_content = if let Some(ref id) = msg.tool_call_id {
                    format!("[Tool Result for {}]: {}", id, msg.content)
                } else {
                    msg.content.clone()
                };
                chat_history.push(Message::User {
                    content: OneOrMany::one(UserContent::text(tool_content)),
                });
            }
            "assistant" => {
                // For assistant messages with tool_calls, format them for context
                let content = if let Some(ref tool_calls) = msg.tool_calls {
                    if msg.content.is_empty() {
                        // Format tool calls for context
                        let calls: Vec<String> = tool_calls
                            .iter()
                            .map(|tc| format!("{}({})", tc.name, tc.arguments))
                            .collect();
                        format!("[Tool Calls]: {}", calls.join(", "))
                    } else {
                        msg.content.clone()
                    }
                } else {
                    msg.content.clone()
                };
                chat_history.push(Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text(content)),
                });
            }
            _ => {
                // Treat as user message, with optional attachments
                let mut user_content: Vec<UserContent> = Vec::new();

                // Add text content if present
                if !msg.content.is_empty() {
                    user_content.push(UserContent::text(&msg.content));
                }

                // Add image attachments
                for attachment in &msg.attachments {
                    if let Some(media_type) = mime_to_image_media_type(&attachment.mime_type) {
                        user_content.push(UserContent::Image(Image {
                            data: DocumentSourceKind::Base64(attachment.data.clone()),
                            media_type: Some(media_type),
                            detail: None,
                            additional_params: None,
                        }));
                    }
                }

                if !user_content.is_empty() {
                    chat_history.push(Message::User {
                        content: OneOrMany::many(user_content)
                            .unwrap_or_else(|_| OneOrMany::one(UserContent::text(""))),
                    });
                }
            }
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
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(chat_history.len() - 1);

        let prompt = extract_text_from_message(&chat_history[last_user_idx]);
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

    // Add tools if provided
    if let Some(tools) = request.tools {
        let rig_tools: Vec<ToolDefinition> = tools.into_iter().map(|t| t.into()).collect();
        builder = builder.tools(rig_tools);
    }

    // Execute the request
    let completion_response = builder
        .send()
        .await
        .map_err(|e| DaemonError::InternalError(format!("LLM chat failed: {}", e)))?;

    // Extract the response content and handle tool calls
    process_completion_response(completion_response.choice)
}

/// Convert MIME type string to rig ImageMediaType.
fn mime_to_image_media_type(mime: &str) -> Option<ImageMediaType> {
    match mime.to_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Some(ImageMediaType::JPEG),
        "image/png" => Some(ImageMediaType::PNG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        _ => None,
    }
}

/// Extract text content from a rig Message.
fn extract_text_from_message(msg: &Message) -> String {
    match msg {
        Message::User { content } => content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::Assistant { content, .. } => content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Process the completion response and convert to LlmChatResponse.
fn process_completion_response(choice: OneOrMany<AssistantContent>) -> Result<LlmChatResponse> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<LlmToolCall> = Vec::new();

    for content in choice.iter() {
        match content {
            AssistantContent::Text(t) => {
                text_parts.push(t.text.clone());
            }
            AssistantContent::ToolCall(tc) => {
                tool_calls.push(LlmToolCall {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                });
            }
            AssistantContent::Reasoning(r) => {
                // Include reasoning as text with a prefix
                let reasoning_text = r.reasoning.join(" ");
                if !reasoning_text.is_empty() {
                    text_parts.push(format!("[Reasoning]: {}", reasoning_text));
                }
            }
            AssistantContent::Image(_) => {
                // Skip images for now
            }
        }
    }

    if !tool_calls.is_empty() {
        Ok(LlmChatResponse {
            content: text_parts.join("\n"),
            finish_reason: "tool_calls".into(),
            tool_calls: Some(tool_calls),
            usage: None,
        })
    } else {
        Ok(LlmChatResponse {
            content: text_parts.join("\n"),
            finish_reason: "stop".into(),
            tool_calls: None,
            usage: None,
        })
    }
}

// ============================================================================
// Streaming Support
// ============================================================================

/// A streaming chunk from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStreamChunk {
    /// Text delta (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Tool calls in this chunk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<LlmToolCall>,
    /// Whether this is the final chunk.
    #[serde(default)]
    pub done: bool,
}

/// Stream entry for tracking active LLM streams.
pub struct LlmStreamEntry {
    /// Receiver for chunks from the background task.
    pub receiver: mpsc::Receiver<LlmStreamChunk>,
    /// Whether the stream is done.
    pub done: bool,
}

/// Registry for managing active LLM streams.
pub struct LlmStreamRegistry {
    /// Next available stream ID.
    next_id: AtomicU64,
    /// Map of stream ID to entry.
    entries: RwLock<HashMap<u64, LlmStreamEntry>>,
}

impl LlmStreamRegistry {
    /// Create a new stream registry.
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Allocate a new stream ID.
    pub fn allocate_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Register a stream receiver.
    pub fn register(&self, id: u64, receiver: mpsc::Receiver<LlmStreamChunk>) {
        let mut entries = self.entries.write().unwrap();
        entries.insert(
            id,
            LlmStreamEntry {
                receiver,
                done: false,
            },
        );
    }

    /// Get the next chunk from a stream.
    pub fn next_chunk(&self, id: u64) -> Result<Option<LlmStreamChunk>> {
        let mut entries = self.entries.write().unwrap();
        let entry = entries
            .get_mut(&id)
            .ok_or_else(|| DaemonError::InternalError(format!("Stream {} not found", id)))?;

        if entry.done {
            return Ok(None);
        }

        // Try to receive the next chunk (non-blocking would require async)
        // For now, we block briefly
        match entry.receiver.try_recv() {
            Ok(chunk) => {
                if chunk.done {
                    entry.done = true;
                }
                Ok(Some(chunk))
            }
            Err(mpsc::error::TryRecvError::Empty) => {
                // No chunk available yet
                Ok(None)
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                entry.done = true;
                Ok(None)
            }
        }
    }

    /// Close and remove a stream.
    pub fn close(&self, id: u64) {
        let mut entries = self.entries.write().unwrap();
        entries.remove(&id);
    }
}

impl Default for LlmStreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Start a streaming LLM chat request.
///
/// Returns a stream ID and spawns a background task to process the stream.
pub async fn start_llm_stream(
    provider: ProviderType,
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    registry: Arc<LlmStreamRegistry>,
) -> Result<u64> {
    let stream_id = registry.allocate_id();
    let (tx, rx) = mpsc::channel(32);

    // Register the receiver
    registry.register(stream_id, rx);

    // Clone values for the spawned task
    let api_key = api_key.to_string();
    let model = model.to_string();

    // Spawn background task to process the stream
    tokio::spawn(async move {
        let result =
            execute_llm_stream_inner(provider, &api_key, &model, request, tx.clone()).await;
        if let Err(e) = result {
            tracing::error!("Stream error: {}", e);
            // Send error as final chunk
            let _ = tx
                .send(LlmStreamChunk {
                    text: Some(format!("[Error: {}]", e)),
                    tool_calls: vec![],
                    done: true,
                })
                .await;
        }
    });

    Ok(stream_id)
}

/// Internal function to execute a streaming LLM request.
async fn execute_llm_stream_inner(
    provider: ProviderType,
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    match provider {
        ProviderType::Anthropic => stream_anthropic(api_key, model, request, tx).await,
        ProviderType::OpenAi => stream_openai(api_key, model, request, tx).await,
        ProviderType::OpenRouter => stream_openrouter(api_key, model, request, tx).await,
        ProviderType::DeepSeek => stream_deepseek(api_key, model, request, tx).await,
        ProviderType::Gemini => stream_gemini(api_key, model, request, tx).await,
        ProviderType::Groq => stream_groq(api_key, model, request, tx).await,
        ProviderType::Mistral => stream_mistral(api_key, model, request, tx).await,
        ProviderType::XAi => stream_xai(api_key, model, request, tx).await,
        ProviderType::Cohere => stream_cohere(api_key, model, request, tx).await,
        ProviderType::Perplexity => stream_perplexity(api_key, model, request, tx).await,
        ProviderType::Together => stream_together(api_key, model, request, tx).await,
        // Qwen and Ollama don't support streaming in rig yet
        ProviderType::Qwen | ProviderType::Ollama => Err(DaemonError::InternalError(format!(
            "Streaming not supported for provider {:?}",
            provider
        ))),
    }
}

/// Stream from Anthropic provider.
async fn stream_anthropic(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: anthropic::Client = anthropic::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create Anthropic client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from OpenAI provider.
async fn stream_openai(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: openai::Client =
        openai::Client::builder()
            .api_key(api_key)
            .build()
            .map_err(|e| {
                DaemonError::InternalError(format!("Failed to create OpenAI client: {}", e))
            })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from OpenRouter provider.
async fn stream_openrouter(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: openrouter::Client = openrouter::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create OpenRouter client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from DeepSeek provider.
async fn stream_deepseek(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: deepseek::Client = deepseek::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create DeepSeek client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from Gemini provider.
async fn stream_gemini(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: gemini::Client =
        gemini::Client::builder()
            .api_key(api_key)
            .build()
            .map_err(|e| {
                DaemonError::InternalError(format!("Failed to create Gemini client: {}", e))
            })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from Groq provider.
async fn stream_groq(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: groq::Client = groq::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| DaemonError::InternalError(format!("Failed to create Groq client: {}", e)))?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from Mistral provider.
async fn stream_mistral(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: mistral::Client = mistral::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create Mistral client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from xAI provider.
async fn stream_xai(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: xai::Client = xai::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| DaemonError::InternalError(format!("Failed to create xAI client: {}", e)))?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from Cohere provider.
async fn stream_cohere(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: cohere::Client =
        cohere::Client::builder()
            .api_key(api_key)
            .build()
            .map_err(|e| {
                DaemonError::InternalError(format!("Failed to create Cohere client: {}", e))
            })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from Perplexity provider.
async fn stream_perplexity(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: perplexity::Client = perplexity::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create Perplexity client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Stream from Together provider.
async fn stream_together(
    api_key: &str,
    model: &str,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()> {
    let client: together::Client = together::Client::builder()
        .api_key(api_key)
        .build()
        .map_err(|e| {
            DaemonError::InternalError(format!("Failed to create Together client: {}", e))
        })?;
    let completion_model = client.completion_model(model);
    stream_rig_completion(completion_model, request, tx).await
}

/// Generic streaming completion for any rig-compatible model.
async fn stream_rig_completion<M>(
    completion_model: M,
    request: LlmChatRequest,
    tx: mpsc::Sender<LlmStreamChunk>,
) -> Result<()>
where
    M: CompletionModel,
    M::StreamingResponse: Clone + Unpin + rig::completion::GetTokenUsage,
{
    // Convert request messages to rig messages (same as non-streaming)
    let mut chat_history: Vec<Message> = Vec::new();
    let mut system_prompt: Option<String> = request.system.clone();

    for msg in &request.messages {
        match msg.role.as_str() {
            "system" => {
                system_prompt = Some(msg.content.clone());
            }
            "tool" => {
                let tool_content = if let Some(ref id) = msg.tool_call_id {
                    format!("[Tool Result for {}]: {}", id, msg.content)
                } else {
                    msg.content.clone()
                };
                chat_history.push(Message::User {
                    content: OneOrMany::one(UserContent::text(tool_content)),
                });
            }
            "assistant" => {
                let content = if let Some(ref tool_calls) = msg.tool_calls {
                    if msg.content.is_empty() {
                        let calls: Vec<String> = tool_calls
                            .iter()
                            .map(|tc| format!("{}({})", tc.name, tc.arguments))
                            .collect();
                        format!("[Tool Calls]: {}", calls.join(", "))
                    } else {
                        msg.content.clone()
                    }
                } else {
                    msg.content.clone()
                };
                chat_history.push(Message::Assistant {
                    id: None,
                    content: OneOrMany::one(rig::message::AssistantContent::text(content)),
                });
            }
            _ => {
                let mut user_content: Vec<UserContent> = Vec::new();
                if !msg.content.is_empty() {
                    user_content.push(UserContent::text(&msg.content));
                }
                for attachment in &msg.attachments {
                    if let Some(media_type) = mime_to_image_media_type(&attachment.mime_type) {
                        user_content.push(UserContent::Image(Image {
                            data: DocumentSourceKind::Base64(attachment.data.clone()),
                            media_type: Some(media_type),
                            detail: None,
                            additional_params: None,
                        }));
                    }
                }
                if !user_content.is_empty() {
                    chat_history.push(Message::User {
                        content: OneOrMany::many(user_content)
                            .unwrap_or_else(|_| OneOrMany::one(UserContent::text(""))),
                    });
                }
            }
        }
    }

    // Get the last user message as the prompt
    let (prompt, chat_history) = if chat_history.is_empty() {
        return Err(DaemonError::InternalError(
            "LLM stream requires at least one user message".into(),
        ));
    } else {
        let last_user_idx = chat_history
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(chat_history.len() - 1);

        let prompt = extract_text_from_message(&chat_history[last_user_idx]);
        let mut history = chat_history;
        history.remove(last_user_idx);
        (prompt, history)
    };

    // Build the completion request
    let mut builder = completion_model.completion_request(&prompt);

    if let Some(preamble) = system_prompt {
        builder = builder.preamble(preamble);
    }

    builder = builder.messages(chat_history);

    if let Some(tools) = request.tools {
        let rig_tools: Vec<ToolDefinition> = tools.into_iter().map(|t| t.into()).collect();
        builder = builder.tools(rig_tools);
    }

    // Execute streaming request
    let mut stream_response = builder
        .stream()
        .await
        .map_err(|e| DaemonError::InternalError(format!("LLM stream failed: {}", e)))?;

    // Process stream chunks
    let mut accumulated_tool_calls: HashMap<String, LlmToolCall> = HashMap::new();

    while let Some(chunk_result) = stream_response.next().await {
        match chunk_result {
            Ok(choice) => {
                let chunk = match choice {
                    StreamedAssistantContent::Text(text) => LlmStreamChunk {
                        text: Some(text.text),
                        tool_calls: vec![],
                        done: false,
                    },
                    StreamedAssistantContent::ToolCall(tc) => {
                        let tool_call = LlmToolCall {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                        };
                        accumulated_tool_calls.insert(tc.id.clone(), tool_call.clone());
                        LlmStreamChunk {
                            text: None,
                            tool_calls: vec![tool_call],
                            done: false,
                        }
                    }
                    StreamedAssistantContent::ToolCallDelta { id, content } => {
                        // Handle tool call deltas (accumulate)
                        if let Some(tc) = accumulated_tool_calls.get_mut(&id) {
                            match content {
                                ToolCallDeltaContent::Name(name) => {
                                    tc.name = name;
                                }
                                ToolCallDeltaContent::Delta(delta) => {
                                    // Append to arguments string representation
                                    if let Some(s) = tc.arguments.as_str() {
                                        tc.arguments =
                                            serde_json::Value::String(format!("{}{}", s, delta));
                                    }
                                }
                            }
                        }
                        continue; // Don't send delta chunks
                    }
                    StreamedAssistantContent::Reasoning(reasoning) => LlmStreamChunk {
                        text: Some(format!("[Reasoning]: {}", reasoning.reasoning.join(" "))),
                        tool_calls: vec![],
                        done: false,
                    },
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. } => LlmStreamChunk {
                        text: Some(reasoning),
                        tool_calls: vec![],
                        done: false,
                    },
                    StreamedAssistantContent::Final(_) => {
                        continue; // Skip final response, we'll send done chunk below
                    }
                };

                if tx.send(chunk).await.is_err() {
                    // Receiver dropped, stop streaming
                    break;
                }
            }
            Err(e) => {
                tracing::error!("Stream chunk error: {}", e);
                break;
            }
        }
    }

    // Send final chunk with accumulated tool calls
    let final_tool_calls: Vec<LlmToolCall> = accumulated_tool_calls.into_values().collect();
    let _ = tx
        .send(LlmStreamChunk {
            text: None,
            tool_calls: final_tool_calls,
            done: true,
        })
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_message_serialization() {
        let message = LlmMessage::user("Hello, world!");

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
            messages: vec![LlmMessage::user("Hello")],
            system: Some("You are helpful.".into()),
            temperature: Some(0.7),
            max_tokens: Some(1000),
            tools: None,
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
            messages: vec![LlmMessage::user("Hi")],
            ..Default::default()
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
            tool_calls: None,
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
            ..Default::default()
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
        let message = LlmMessage::assistant("Test");
        let cloned = message.clone();
        assert_eq!(cloned.role, message.role);
        assert_eq!(cloned.content, message.content);
    }

    #[test]
    fn test_llm_request_clone() {
        let request = LlmChatRequest {
            messages: vec![LlmMessage::user("Test")],
            system: Some("System".into()),
            temperature: Some(0.5),
            max_tokens: Some(500),
            tools: None,
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
            ..Default::default()
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
        let message = LlmMessage::user("Debug test");
        let debug = format!("{:?}", message);
        assert!(debug.contains("LlmMessage"));
        assert!(debug.contains("user"));
    }

    #[test]
    fn test_llm_request_debug() {
        let request = LlmChatRequest::default();
        let debug = format!("{:?}", request);
        assert!(debug.contains("LlmChatRequest"));
    }

    #[test]
    fn test_llm_response_debug() {
        let response = LlmChatResponse {
            content: "Test".into(),
            finish_reason: "stop".into(),
            ..Default::default()
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
                LlmMessage::user("Hello"),
                LlmMessage::assistant("Hi!"),
                LlmMessage::user("How are you?"),
            ],
            ..Default::default()
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
            tool_calls: None,
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

    #[test]
    fn test_llm_tool_definition() {
        let tool = LlmToolDefinition {
            name: "get_weather".into(),
            description: "Get the current weather".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"}
                },
                "required": ["location"]
            }),
        };

        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("get_weather"));
        assert!(json.contains("location"));

        // Test conversion to rig::ToolDefinition
        let rig_tool: ToolDefinition = tool.into();
        assert_eq!(rig_tool.name, "get_weather");
    }

    #[test]
    fn test_llm_tool_call() {
        let tool_call = LlmToolCall {
            id: "call_123".into(),
            name: "get_weather".into(),
            arguments: serde_json::json!({"location": "Tokyo"}),
        };

        let json = serde_json::to_string(&tool_call).unwrap();
        assert!(json.contains("call_123"));
        assert!(json.contains("get_weather"));
        assert!(json.contains("Tokyo"));
    }

    #[test]
    fn test_llm_message_with_tool_calls() {
        let tool_call = LlmToolCall {
            id: "call_abc".into(),
            name: "search".into(),
            arguments: serde_json::json!({"query": "rust"}),
        };
        let message = LlmMessage::assistant_with_tool_calls(vec![tool_call]);

        assert_eq!(message.role, "assistant");
        assert!(message.content.is_empty());
        assert!(message.tool_calls.is_some());
        assert_eq!(message.tool_calls.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_llm_message_tool_result() {
        let message =
            LlmMessage::tool_result("call_abc", "Search result: Rust is a programming language");

        assert_eq!(message.role, "tool");
        assert!(message.content.contains("Rust"));
        assert_eq!(message.tool_call_id, Some("call_abc".into()));
    }

    #[test]
    fn test_llm_response_with_tool_calls() {
        let response = LlmChatResponse {
            content: String::new(),
            finish_reason: "tool_calls".into(),
            tool_calls: Some(vec![LlmToolCall {
                id: "call_xyz".into(),
                name: "calculator".into(),
                arguments: serde_json::json!({"expression": "2+2"}),
            }]),
            usage: None,
        };

        assert_eq!(response.finish_reason, "tool_calls");
        assert!(response.tool_calls.is_some());
        let calls = response.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "calculator");
    }

    #[test]
    fn test_llm_request_with_tools() {
        let request = LlmChatRequest {
            messages: vec![LlmMessage::user("What's the weather?")],
            tools: Some(vec![LlmToolDefinition {
                name: "get_weather".into(),
                description: "Get weather info".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            ..Default::default()
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("get_weather"));
        assert!(json.contains("Get weather info"));
    }

    // =========================================================================
    // Streaming Tests
    // =========================================================================

    #[test]
    fn test_stream_chunk_serialization() {
        let chunk = LlmStreamChunk {
            text: Some("Hello".into()),
            tool_calls: vec![],
            done: false,
        };

        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("Hello"));
        assert!(json.contains("false"));

        let parsed: LlmStreamChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.text, Some("Hello".into()));
        assert!(!parsed.done);
    }

    #[test]
    fn test_stream_chunk_with_tool_calls() {
        let chunk = LlmStreamChunk {
            text: None,
            tool_calls: vec![LlmToolCall {
                id: "call_123".into(),
                name: "search".into(),
                arguments: serde_json::json!({"query": "rust"}),
            }],
            done: false,
        };

        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains("call_123"));
        assert!(json.contains("search"));
    }

    #[test]
    fn test_stream_chunk_done() {
        let chunk = LlmStreamChunk {
            text: None,
            tool_calls: vec![],
            done: true,
        };

        assert!(chunk.done);
        assert!(chunk.text.is_none());
    }

    #[test]
    fn test_stream_registry_allocate_id() {
        let registry = LlmStreamRegistry::new();
        let id1 = registry.allocate_id();
        let id2 = registry.allocate_id();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn test_stream_registry_close_nonexistent() {
        let registry = LlmStreamRegistry::new();
        // Should not panic
        registry.close(999);
    }

    #[tokio::test]
    async fn test_stream_registry_register_and_close() {
        let registry = LlmStreamRegistry::new();
        let (tx, rx) = mpsc::channel(16);
        let id = registry.allocate_id();

        registry.register(id, rx);

        // Send a chunk
        tx.send(LlmStreamChunk {
            text: Some("Hello".into()),
            tool_calls: vec![],
            done: false,
        })
        .await
        .unwrap();

        // Get chunk
        let chunk = registry.next_chunk(id).unwrap();
        assert!(chunk.is_some());
        assert_eq!(chunk.unwrap().text, Some("Hello".into()));

        // Close
        registry.close(id);

        // Should fail to get next chunk
        let result = registry.next_chunk(id);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stream_registry_done_flag() {
        let registry = LlmStreamRegistry::new();
        let (tx, rx) = mpsc::channel(16);
        let id = registry.allocate_id();

        registry.register(id, rx);

        // Send done chunk
        tx.send(LlmStreamChunk {
            text: None,
            tool_calls: vec![],
            done: true,
        })
        .await
        .unwrap();

        // Get chunk - should mark stream as done
        let chunk = registry.next_chunk(id).unwrap();
        assert!(chunk.is_some());
        assert!(chunk.unwrap().done);

        // Next call should return None (stream is done)
        let chunk = registry.next_chunk(id).unwrap();
        assert!(chunk.is_none());
    }
}
