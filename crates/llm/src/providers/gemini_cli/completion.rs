//! GeminiCliCompletionModel implementation.
//!
//! Implements the rig-core CompletionModel trait for Gemini CLI.
//! Spawns the `gemini` CLI as a subprocess for each completion request,
//! using `-p` for prompt text.

use futures::stream::StreamExt;
use rig::completion::{
    self, AssistantContent, CompletionError, CompletionRequest, CompletionResponse, Document,
    ToolDefinition, Usage,
};
use rig::message::{Message, ToolCall, ToolFunction, ToolResultContent, UserContent};
use rig::streaming::{RawStreamingChoice, RawStreamingToolCall, StreamingCompletionResponse};
use rig::OneOrMany;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::super::ChildGuardStream;
use super::client::GEMINI_CLI_KNOWN_MODELS;
use super::types::{
    extract_tool_calls_from_text, format_tool_call_as_xml, format_tool_definitions_prompt,
    format_tool_result_as_xml, parse_gemini_output, GeminiCliCompletionResponse, GeminiCliUsage,
};
use super::GeminiCliClient;

/// Completion model for Gemini CLI.
///
/// Implements the rig-core `CompletionModel` trait by spawning the
/// `gemini` CLI as a subprocess for each request.
#[derive(Clone)]
pub struct GeminiCliCompletionModel {
    client: GeminiCliClient,
    model: String,
}

impl GeminiCliCompletionModel {
    /// Create a new completion model.
    pub fn new(client: GeminiCliClient, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
        }
    }

    /// Get the model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Build the full prompt string from system prompt, messages, documents, and tools.
    fn build_prompt(
        &self,
        system_prompt: Option<&str>,
        messages: &[Message],
        documents: &[Document],
        tools: &[ToolDefinition],
    ) -> String {
        let mut full_prompt = String::new();

        // Add system prompt
        if let Some(system) = system_prompt {
            if !system.is_empty() {
                full_prompt.push_str(system);
                full_prompt.push_str("\n\n");
            }
        }

        // Add tool definitions
        let tool_prompt = format_tool_definitions_prompt(tools);
        if !tool_prompt.is_empty() {
            full_prompt.push_str(&tool_prompt);
            full_prompt.push_str("\n\n");
        }

        // Add documents
        if !documents.is_empty() {
            full_prompt.push_str("<attachments>\n");
            for doc in documents {
                full_prompt.push_str(&doc.to_string());
            }
            full_prompt.push_str("</attachments>\n\n");
        }

        // Add conversation history
        for msg in messages {
            match msg {
                Message::User { content } => {
                    full_prompt.push_str("Human: ");
                    full_prompt.push_str(&extract_text_from_user_content(content));
                    full_prompt.push('\n');
                }
                Message::Assistant { content, .. } => {
                    full_prompt.push_str("Assistant: ");
                    full_prompt.push_str(&extract_text_from_assistant_content(content));
                    full_prompt.push('\n');
                }
            }
            full_prompt.push('\n');
        }

        full_prompt.push_str("Assistant: ");
        full_prompt
    }

    /// Build the CLI command with appropriate arguments.
    fn build_command(
        &self,
        system_prompt: Option<&str>,
        messages: &[Message],
        documents: &[Document],
        tools: &[ToolDefinition],
    ) -> Command {
        let prompt = self.build_prompt(system_prompt, messages, documents, tools);

        let mut cmd = Command::new(self.client.command());

        // Only pass model parameter if it's in the known models list
        if GEMINI_CLI_KNOWN_MODELS.contains(&self.model.as_str()) {
            cmd.arg("-m").arg(&self.model);
        }

        cmd.arg("-p").arg(&prompt);
        cmd.arg("--yolo");

        // Set CWD if configured
        if let Some(cwd) = self.client.working_dir() {
            cmd.current_dir(cwd);
        }

        // Additional directories to include in the workspace
        for dir in self.client.add_dirs() {
            cmd.arg("--include-directories").arg(dir);
        }

        // Pass API key via environment variable if set
        if let Some(api_key) = self.client.api_key() {
            cmd.env("GEMINI_API_KEY", api_key);
        }

        cmd
    }
}

/// Extract text content from UserContent.
fn extract_text_from_user_content(content: &OneOrMany<UserContent>) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.clone()),
            UserContent::ToolResult(tr) => {
                let result_text: Vec<String> = tr
                    .content
                    .iter()
                    .filter_map(|rc| match rc {
                        ToolResultContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect();
                Some(format_tool_result_as_xml(&tr.id, &result_text.join("")))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract text content from AssistantContent.
fn extract_text_from_assistant_content(content: &OneOrMany<AssistantContent>) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.clone()),
            AssistantContent::ToolCall(tc) => Some(format_tool_call_as_xml(
                &tc.id,
                &tc.function.name,
                &tc.function.arguments,
            )),
            AssistantContent::Reasoning(r) => {
                Some(format!("[Reasoning]: {}", r.reasoning.join(" ")))
            }
            AssistantContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Streaming response for Gemini CLI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeminiCliStreamingResponse {
    pub usage: Option<GeminiCliUsage>,
}

impl completion::GetTokenUsage for GeminiCliStreamingResponse {
    fn token_usage(&self) -> Option<rig::completion::Usage> {
        self.usage.as_ref().map(|u| Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
        })
    }
}

/// Parse text for tool call markers and emit streaming choices.
fn emit_text_and_tool_calls(
    text: &str,
) -> Vec<Result<RawStreamingChoice<GeminiCliStreamingResponse>, CompletionError>> {
    let (cleaned, tool_calls) = extract_tool_calls_from_text(text);
    let mut items = Vec::new();

    if !cleaned.is_empty() {
        items.push(Ok(RawStreamingChoice::Message(cleaned)));
    }
    for tc in tool_calls {
        items.push(Ok(RawStreamingChoice::ToolCall(RawStreamingToolCall::new(
            tc.id,
            tc.name,
            tc.arguments,
        ))));
    }

    if items.is_empty() && !text.is_empty() {
        items.push(Ok(RawStreamingChoice::Message(text.to_string())));
    }

    items
}

impl completion::CompletionModel for GeminiCliCompletionModel {
    type Response = GeminiCliCompletionResponse;
    type StreamingResponse = GeminiCliStreamingResponse;
    type Client = GeminiCliClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self::new(client.clone(), model)
    }

    async fn completion(
        &self,
        completion_request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        let messages: Vec<Message> = completion_request.chat_history.iter().cloned().collect();

        let mut cmd = self.build_command(
            completion_request.preamble.as_deref(),
            &messages,
            &completion_request.documents,
            &completion_request.tools,
        );

        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                CompletionError::ProviderError(format!(
                    "Failed to run gemini CLI '{}': {}. \
                     Make sure the Gemini CLI is installed and available on PATH.",
                    self.client.command(),
                    e
                ))
            })?;

        let output = child.wait_with_output().await.map_err(|e| {
            CompletionError::ProviderError(format!("Failed to wait for gemini CLI: {}", e))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CompletionError::ProviderError(format!(
                "Gemini CLI exited with status {}: {}",
                output.status, stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let response = parse_gemini_output(&stdout).map_err(|e| {
            CompletionError::ProviderError(format!("Failed to parse CLI output: {}", e))
        })?;

        // Check for text-injected tool calls
        let (cleaned_text, tool_calls) = extract_tool_calls_from_text(&response.content);

        if tool_calls.is_empty() {
            response.try_into()
        } else {
            let usage = Usage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                total_tokens: response.usage.input_tokens + response.usage.output_tokens,
            };

            let mut contents: Vec<AssistantContent> = Vec::new();
            if !cleaned_text.is_empty() {
                contents.push(AssistantContent::text(&cleaned_text));
            }
            for tc in tool_calls {
                contents.push(AssistantContent::ToolCall(ToolCall::new(
                    tc.id,
                    ToolFunction::new(tc.name, tc.arguments),
                )));
            }

            let choice = OneOrMany::many(contents).map_err(|_| {
                CompletionError::ResponseError("Empty response from Gemini CLI".into())
            })?;

            Ok(CompletionResponse {
                choice,
                usage,
                raw_response: response,
            })
        }
    }

    async fn stream(
        &self,
        completion_request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        let messages: Vec<Message> = completion_request.chat_history.iter().cloned().collect();

        let mut cmd = self.build_command(
            completion_request.preamble.as_deref(),
            &messages,
            &completion_request.documents,
            &completion_request.tools,
        );

        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                CompletionError::ProviderError(format!("Failed to spawn gemini CLI: {}", e))
            })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CompletionError::ProviderError("No stdout from CLI".into()))?;

        let reader = tokio::io::BufReader::new(stdout);
        let lines =
            tokio_stream::wrappers::LinesStream::new(tokio::io::AsyncBufReadExt::lines(reader));

        // Buffer multi-line <tool_call> blocks before extraction.
        // The Gemini CLI outputs plain text line-by-line, so <tool_call>...</tool_call>
        // blocks spanning multiple lines must be accumulated before tool call extraction.
        // Without buffering, each line is processed independently and multi-line tool
        // calls are never matched (extract_tool_calls_from_text requires both opening
        // and closing tags in the same string).
        type StreamItems =
            Vec<Result<RawStreamingChoice<GeminiCliStreamingResponse>, CompletionError>>;
        let stream = futures::stream::unfold(
            (lines, String::new(), false),
            |(mut lines, mut buffer, mut in_tool_call)| async move {
                loop {
                    match StreamExt::next(&mut lines).await {
                        Some(Ok(line)) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty()
                                || trimmed.starts_with("Loaded cached credentials")
                            {
                                continue;
                            }

                            if in_tool_call {
                                buffer.push_str(&line);
                                buffer.push('\n');
                                if line.contains("</tool_call>") {
                                    // Tool call block complete — extract and emit
                                    in_tool_call = false;
                                    let complete = std::mem::take(&mut buffer);
                                    let items: StreamItems =
                                        emit_text_and_tool_calls(&complete);
                                    return Some((items, (lines, buffer, in_tool_call)));
                                }
                                continue;
                            }

                            if line.contains("<tool_call>")
                                && !line.contains("</tool_call>")
                            {
                                // Start of multi-line tool call — buffer it
                                in_tool_call = true;
                                let idx = line.find("<tool_call>").unwrap();
                                let before = line[..idx].trim().to_string();
                                buffer = line[idx..].to_string();
                                buffer.push('\n');
                                if !before.is_empty() {
                                    let items: StreamItems =
                                        vec![Ok(RawStreamingChoice::Message(before))];
                                    return Some((items, (lines, buffer, in_tool_call)));
                                }
                                continue;
                            }

                            // Normal line or single-line tool call
                            let items: StreamItems = emit_text_and_tool_calls(&line);
                            if !items.is_empty() {
                                return Some((items, (lines, buffer, in_tool_call)));
                            }
                            continue;
                        }
                        Some(Err(e)) => {
                            let items: StreamItems =
                                vec![Err(CompletionError::ProviderError(e.to_string()))];
                            return Some((items, (lines, buffer, in_tool_call)));
                        }
                        None => {
                            // Stream ended — flush any remaining buffer
                            if !buffer.is_empty() {
                                let remaining = std::mem::take(&mut buffer);
                                let items: StreamItems =
                                    emit_text_and_tool_calls(&remaining);
                                return Some((items, (lines, buffer, in_tool_call)));
                            }
                            return None;
                        }
                    }
                }
            },
        )
        .flat_map(futures::stream::iter);

        // Wrap in ChildGuardStream to kill the CLI subprocess when the stream is dropped
        let guarded = ChildGuardStream::new(stream, child);

        Ok(StreamingCompletionResponse::stream(Box::pin(guarded)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::gemini_cli::GeminiCliClient;

    #[test]
    fn test_completion_model_new() {
        let client = GeminiCliClient::new("gemini");
        let model = GeminiCliCompletionModel::new(client, "gemini-2.5-pro");
        assert_eq!(model.model(), "gemini-2.5-pro");
    }

    #[test]
    fn test_completion_model_clone() {
        let client = GeminiCliClient::new("gemini");
        let model = GeminiCliCompletionModel::new(client, "gemini-2.5-flash");
        let cloned = model.clone();
        assert_eq!(cloned.model(), "gemini-2.5-flash");
    }

    #[test]
    fn test_implements_completion_model_trait() {
        fn assert_completion_model<T: completion::CompletionModel>() {}
        assert_completion_model::<GeminiCliCompletionModel>();
    }

    #[test]
    fn test_completion_model_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GeminiCliCompletionModel>();
    }

    #[test]
    fn test_emit_text_and_tool_calls_plain_text() {
        let items = emit_text_and_tool_calls("Hello world");
        assert_eq!(items.len(), 1);
        match &items[0] {
            Ok(RawStreamingChoice::Message(text)) => assert_eq!(text, "Hello world"),
            other => panic!("Expected Message, got {:?}", other),
        }
    }

    #[test]
    fn test_emit_text_and_tool_calls_with_tool() {
        let text = "I'll take a screenshot.\n<tool_call>\n{\"id\":\"call_1\",\"name\":\"screenshot\",\"arguments\":{}}\n</tool_call>";
        let items = emit_text_and_tool_calls(text);
        assert_eq!(items.len(), 2);
        match &items[0] {
            Ok(RawStreamingChoice::Message(msg)) => {
                assert!(msg.contains("screenshot"));
                assert!(!msg.contains("<tool_call>"));
            }
            other => panic!("Expected Message, got {:?}", other),
        }
        match &items[1] {
            Ok(RawStreamingChoice::ToolCall(tc)) => {
                assert_eq!(tc.name, "screenshot");
                assert_eq!(tc.id, "call_1");
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn test_build_prompt_simple() {
        let client = GeminiCliClient::new("gemini");
        let model = GeminiCliCompletionModel::new(client, "gemini-2.5-pro");

        let messages = vec![Message::User {
            content: OneOrMany::one(UserContent::text("Hello")),
        }];

        let prompt = model.build_prompt(Some("You are helpful"), &messages, &[], &[]);
        assert!(prompt.contains("You are helpful"));
        assert!(prompt.contains("Human: Hello"));
        assert!(prompt.contains("Assistant: "));
    }

    #[test]
    fn test_build_prompt_no_system() {
        let client = GeminiCliClient::new("gemini");
        let model = GeminiCliCompletionModel::new(client, "gemini-2.5-pro");

        let messages = vec![Message::User {
            content: OneOrMany::one(UserContent::text("Hi")),
        }];

        let prompt = model.build_prompt(None, &messages, &[], &[]);
        assert!(prompt.contains("Human: Hi"));
        assert!(!prompt.starts_with("\n\n"));
    }

    #[test]
    fn test_build_prompt_conversation() {
        let client = GeminiCliClient::new("gemini");
        let model = GeminiCliCompletionModel::new(client, "gemini-2.5-pro");

        let messages = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("Hi")),
            },
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text("Hello!")),
            },
            Message::User {
                content: OneOrMany::one(UserContent::text("How are you?")),
            },
        ];

        let prompt = model.build_prompt(None, &messages, &[], &[]);
        assert!(prompt.contains("Human: Hi"));
        assert!(prompt.contains("Assistant: Hello!"));
        assert!(prompt.contains("Human: How are you?"));
    }

    #[test]
    fn test_per_line_processing_misses_multiline_tool_call() {
        // Demonstrates the bug: processing each line independently fails to extract
        // multi-line <tool_call> blocks because extract_tool_calls_from_text requires
        // both <tool_call> and </tool_call> in the same string.
        let line1 = "<tool_call>";
        let line2 = r#"{"id":"call_1","name":"browser_get_markdown","arguments":{"tab_id":3}}"#;
        let line3 = "</tool_call>";

        for line in [line1, line2, line3] {
            let items = emit_text_and_tool_calls(line);
            for item in &items {
                assert!(
                    !matches!(item, Ok(RawStreamingChoice::ToolCall(_))),
                    "Per-line processing should NOT extract tool call from: {}",
                    line
                );
            }
        }
    }

    #[test]
    fn test_buffered_multiline_tool_call_extraction() {
        // Shows that buffering lines into a single string allows correct extraction.
        // This is what the stream() method's unfold-based buffering achieves.
        let buffered = "<tool_call>\n{\"id\":\"call_1\",\"name\":\"browser_get_markdown\",\"arguments\":{\"tab_id\":3}}\n</tool_call>";
        let items = emit_text_and_tool_calls(buffered);
        assert_eq!(items.len(), 1);
        match &items[0] {
            Ok(RawStreamingChoice::ToolCall(tc)) => {
                assert_eq!(tc.name, "browser_get_markdown");
                assert_eq!(tc.id, "call_1");
                assert_eq!(tc.arguments, serde_json::json!({"tab_id": 3}));
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn test_buffered_multiline_tool_call_with_preceding_text() {
        // Text before the tool call block should be emitted as a Message.
        let buffered = "Let me read the page.\n<tool_call>\n{\"id\":\"call_1\",\"name\":\"browser_get_markdown\",\"arguments\":{\"tab_id\":3}}\n</tool_call>";
        let items = emit_text_and_tool_calls(buffered);
        assert_eq!(items.len(), 2);
        match &items[0] {
            Ok(RawStreamingChoice::Message(text)) => {
                assert!(text.contains("Let me read the page"));
                assert!(!text.contains("<tool_call>"));
            }
            other => panic!("Expected Message, got {:?}", other),
        }
        match &items[1] {
            Ok(RawStreamingChoice::ToolCall(tc)) => {
                assert_eq!(tc.name, "browser_get_markdown");
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }
}
