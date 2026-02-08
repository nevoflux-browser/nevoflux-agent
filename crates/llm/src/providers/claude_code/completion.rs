//! ClaudeCodeCompletionModel implementation.
//!
//! Implements the rig-core CompletionModel trait for Claude Code CLI.
//! Spawns the `claude` CLI as a subprocess for each completion request,
//! using `--input-format stream-json` for stdin-based structured input.

use futures::stream::StreamExt;
use rig::completion::{
    self, AssistantContent, CompletionError, CompletionRequest, CompletionResponse, Document,
    ToolDefinition, Usage,
};
use rig::message::{Message, ToolCall, ToolFunction, ToolResultContent, UserContent};
use rig::streaming::{RawStreamingChoice, RawStreamingToolCall, StreamingCompletionResponse};
use rig::OneOrMany;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::super::ChildGuardStream;
use super::types::{
    extract_tool_calls_from_text, format_tool_call_as_xml, format_tool_definitions_prompt,
    format_tool_result_as_xml, parse_claude_output, ClaudeCodeCompletionResponse, ClaudeUsage,
};
use super::ClaudeCodeClient;

/// Completion model for Claude Code CLI.
///
/// Implements the rig-core `CompletionModel` trait by spawning the
/// `claude` CLI as a subprocess for each request.
#[derive(Clone)]
pub struct ClaudeCodeCompletionModel {
    client: ClaudeCodeClient,
    model: String,
}

impl ClaudeCodeCompletionModel {
    /// Create a new completion model.
    pub fn new(client: ClaudeCodeClient, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
        }
    }

    /// Get the model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Build the CLI command with common arguments for stream-json mode.
    ///
    /// If `tools` is non-empty, tool definitions are appended to the system prompt
    /// as structured XML so the CLI can produce `<tool_call>` markers in its output.
    fn build_command(&self, system_prompt: Option<&str>, tools: &[ToolDefinition]) -> Command {
        let mut cmd = Command::new(self.client.command());

        cmd.arg("-p");
        cmd.arg("--input-format").arg("stream-json");
        cmd.arg("--output-format").arg("stream-json");
        cmd.arg("--verbose");
        cmd.arg("--dangerously-skip-permissions");

        if !self.model.is_empty() {
            cmd.arg("--model").arg(&self.model);
        }

        // Build enhanced preamble with tool definitions
        let tool_prompt = format_tool_definitions_prompt(tools);
        let preamble = match (system_prompt, tool_prompt.is_empty()) {
            (Some(p), true) if !p.is_empty() => Some(p.to_string()),
            (Some(p), false) if !p.is_empty() => Some(format!("{}{}", p, tool_prompt)),
            (None, false) | (Some(""), false) => Some(tool_prompt),
            _ => None,
        };

        if let Some(ref preamble) = preamble {
            cmd.arg("--system-prompt").arg(preamble);
        }

        // Set CWD to isolated workspace (prevents damaging daemon's own directory)
        if let Some(cwd) = self.client.working_dir() {
            cmd.current_dir(cwd);
        }

        // Additional directories beyond the CWD
        for dir in self.client.add_dirs() {
            cmd.arg("--add-dir").arg(dir);
        }

        // Pass API key via environment variable if set
        if let Some(api_key) = self.client.api_key() {
            cmd.env("ANTHROPIC_API_KEY", api_key);
        }

        cmd
    }
}

/// Simple message structure for Claude CLI serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CliMessage {
    role: String,
    content: Vec<CliContent>,
}

/// Content item for CLI message serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CliContent {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

/// A single stream-json input line for the Claude CLI stdin.
#[derive(Debug, Clone, Serialize)]
struct StreamJsonInput {
    #[serde(rename = "type")]
    msg_type: String,
    message: CliMessage,
}

/// Build newline-delimited JSON stdin input from rig Messages and Documents.
///
/// The Claude CLI stream-json format only accepts `user` type messages on stdin.
/// For multi-turn conversations with assistant messages, the entire conversation
/// history is serialized into a single user message with clearly labeled turns.
fn build_stdin_input(messages: &[Message], documents: &[Document]) -> String {
    if messages.is_empty() {
        return String::new();
    }

    // Check if there are any assistant messages in the history
    let has_assistant = messages
        .iter()
        .any(|m| matches!(m, Message::Assistant { .. }));

    if has_assistant {
        // Multi-turn: combine all messages into a single user message with labeled turns
        let mut combined = String::new();

        // Prepend documents if present
        if !documents.is_empty() {
            combined.push_str(&format!(
                "<attachments>\n{}</attachments>\n\n",
                documents
                    .iter()
                    .map(|doc| doc.to_string())
                    .collect::<Vec<_>>()
                    .join("")
            ));
        }

        combined.push_str("<conversation_history>\n");
        for msg in messages {
            match msg {
                Message::User { content } => {
                    let text = extract_text_from_user_content(content);
                    combined.push_str(&format!("[user]: {}\n", text));
                }
                Message::Assistant { content, .. } => {
                    let text = extract_text_from_assistant_content(content);
                    combined.push_str(&format!("[assistant]: {}\n", text));
                }
            }
        }
        combined.push_str(
            "</conversation_history>\n\nContinue the conversation based on the history above.",
        );

        let input = StreamJsonInput {
            msg_type: "user".to_string(),
            message: CliMessage {
                role: "user".to_string(),
                content: vec![CliContent {
                    content_type: "text".to_string(),
                    text: combined,
                }],
            },
        };
        serde_json::to_string(&input).unwrap_or_default()
    } else {
        // Single or multiple user-only messages: send each as a separate line
        let mut lines = Vec::new();

        for (i, msg) in messages.iter().enumerate() {
            if let Message::User { content } = msg {
                let mut text = extract_text_from_user_content(content);

                // Prepend documents to the first user message
                if i == 0 && !documents.is_empty() {
                    let doc_text = format!(
                        "<attachments>\n{}</attachments>",
                        documents
                            .iter()
                            .map(|doc| doc.to_string())
                            .collect::<Vec<_>>()
                            .join("")
                    );
                    text = format!("{}\n\n{}", doc_text, text);
                }

                let input = StreamJsonInput {
                    msg_type: "user".to_string(),
                    message: CliMessage {
                        role: "user".to_string(),
                        content: vec![CliContent {
                            content_type: "text".to_string(),
                            text,
                        }],
                    },
                };
                if let Ok(json) = serde_json::to_string(&input) {
                    lines.push(json);
                }
            }
        }

        lines.join("\n")
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

/// Streaming response for Claude Code CLI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeCodeStreamingResponse {
    pub usage: Option<ClaudeUsage>,
}

impl completion::GetTokenUsage for ClaudeCodeStreamingResponse {
    fn token_usage(&self) -> Option<rig::completion::Usage> {
        self.usage.as_ref().map(|u| Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
        })
    }
}

/// Parse text for tool call markers and emit streaming choices.
///
/// If the text contains `<tool_call>` markers, emits cleaned text as a Message
/// followed by each extracted tool call as a ToolCall choice. Otherwise emits
/// the text as a single Message.
fn emit_text_and_tool_calls(
    text: &str,
) -> Vec<Result<RawStreamingChoice<ClaudeCodeStreamingResponse>, CompletionError>> {
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

    // If nothing was extracted (no tool calls and empty cleaned text), return original
    if items.is_empty() && !text.is_empty() {
        items.push(Ok(RawStreamingChoice::Message(text.to_string())));
    }

    items
}

impl completion::CompletionModel for ClaudeCodeCompletionModel {
    type Response = ClaudeCodeCompletionResponse;
    type StreamingResponse = ClaudeCodeStreamingResponse;
    type Client = ClaudeCodeClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self::new(client.clone(), model)
    }

    async fn completion(
        &self,
        completion_request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        let messages: Vec<Message> = completion_request.chat_history.iter().cloned().collect();
        let stdin_input = build_stdin_input(&messages, &completion_request.documents);

        let mut cmd = self.build_command(
            completion_request.preamble.as_deref(),
            &completion_request.tools,
        );

        // Spawn with piped stdin/stdout/stderr
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                CompletionError::ProviderError(format!("Failed to run claude CLI: {}", e))
            })?;

        // Write stdin input and close
        if let Some(mut stdin) = child.stdin.take() {
            if !stdin_input.is_empty() {
                stdin.write_all(stdin_input.as_bytes()).await.map_err(|e| {
                    CompletionError::ProviderError(format!("Failed to write stdin: {}", e))
                })?;
                stdin.write_all(b"\n").await.map_err(|e| {
                    CompletionError::ProviderError(format!("Failed to write stdin newline: {}", e))
                })?;
            }
            drop(stdin);
        }

        let output = child.wait_with_output().await.map_err(|e| {
            CompletionError::ProviderError(format!("Failed to wait for claude CLI: {}", e))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CompletionError::ProviderError(format!(
                "Claude CLI exited with status {}: {}",
                output.status, stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let response = parse_claude_output(&stdout).map_err(|e| {
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
                CompletionError::ResponseError("Empty response from Claude Code CLI".into())
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
        let stdin_input = build_stdin_input(&messages, &completion_request.documents);

        let mut cmd = self.build_command(
            completion_request.preamble.as_deref(),
            &completion_request.tools,
        );

        // Spawn with piped stdin/stdout/stderr
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                CompletionError::ProviderError(format!("Failed to spawn claude CLI: {}", e))
            })?;

        // Write stdin input and close
        if let Some(mut stdin) = child.stdin.take() {
            if !stdin_input.is_empty() {
                stdin.write_all(stdin_input.as_bytes()).await.map_err(|e| {
                    CompletionError::ProviderError(format!("Failed to write stdin: {}", e))
                })?;
                stdin.write_all(b"\n").await.map_err(|e| {
                    CompletionError::ProviderError(format!("Failed to write stdin newline: {}", e))
                })?;
            }
            drop(stdin);
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CompletionError::ProviderError("No stdout from CLI".into()))?;

        let reader = tokio::io::BufReader::new(stdout);
        let lines =
            tokio_stream::wrappers::LinesStream::new(tokio::io::AsyncBufReadExt::lines(reader));

        // Use flat_map to emit multiple items (text + tool calls) from a single assistant message
        let stream = lines
            .map(
                |line_result| -> Vec<
                    Result<RawStreamingChoice<ClaudeCodeStreamingResponse>, CompletionError>,
                > {
                    match line_result {
                        Ok(line) => {
                            if line.trim().is_empty() {
                                return vec![];
                            }
                            // Try to parse each line as a JSON event
                            if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
                                let event_type = entry.get("type").and_then(|t| t.as_str());

                                // Handle stream-json "assistant" events
                                if event_type == Some("assistant") {
                                    if let Some(contents) = entry
                                        .get("message")
                                        .and_then(|m| m.get("content"))
                                        .and_then(|c| c.as_array())
                                    {
                                        let text: String = contents
                                            .iter()
                                            .filter_map(|c| {
                                                if c.get("type").and_then(|t| t.as_str())
                                                    == Some("text")
                                                {
                                                    c.get("text").and_then(|t| t.as_str())
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect::<Vec<_>>()
                                            .join("");
                                        if !text.is_empty() {
                                            return emit_text_and_tool_calls(&text);
                                        }
                                    }
                                }

                                // Handle content_block_delta events
                                if let Some(text) = entry
                                    .get("delta")
                                    .and_then(|d| d.get("text"))
                                    .and_then(|t| t.as_str())
                                {
                                    if !text.is_empty() {
                                        return emit_text_and_tool_calls(text);
                                    }
                                }

                                // Handle plain text output at top level
                                if let Some(text) = entry.get("text").and_then(|t| t.as_str()) {
                                    if !text.is_empty() {
                                        return emit_text_and_tool_calls(text);
                                    }
                                }
                            }
                            // If not JSON, pass through as text
                            if !line.starts_with('{') {
                                return vec![Ok(RawStreamingChoice::Message(line))];
                            }
                            vec![]
                        }
                        Err(e) => vec![Err(CompletionError::ProviderError(e.to_string()))],
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
    use crate::providers::claude_code::ClaudeCodeClient;

    #[test]
    fn test_completion_model_new() {
        let client = ClaudeCodeClient::new("claude");
        let model = ClaudeCodeCompletionModel::new(client, "sonnet");
        assert_eq!(model.model(), "sonnet");
    }

    #[test]
    fn test_completion_model_clone() {
        let client = ClaudeCodeClient::new("claude");
        let model = ClaudeCodeCompletionModel::new(client, "opus");
        let cloned = model.clone();
        assert_eq!(cloned.model(), "opus");
    }

    #[test]
    fn test_build_stdin_input_empty() {
        let result = build_stdin_input(&[], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_stdin_input_single_user() {
        let messages = vec![Message::User {
            content: OneOrMany::one(UserContent::text("Hello")),
        }];
        let result = build_stdin_input(&messages, &[]);
        assert!(result.contains("\"type\":\"user\""));
        assert!(result.contains("Hello"));
        // Should be a single JSON line
        assert_eq!(result.lines().count(), 1);
    }

    #[test]
    fn test_build_stdin_input_conversation() {
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
        let result = build_stdin_input(&messages, &[]);

        // Multi-turn with assistant messages gets combined into a single user message
        assert_eq!(result.lines().count(), 1);

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["type"], "user");

        // The combined text should contain the conversation history
        let text = parsed["message"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[user]: Hi"));
        assert!(text.contains("[assistant]: Hello!"));
        assert!(text.contains("[user]: How are you?"));
        assert!(text.contains("<conversation_history>"));
    }

    #[test]
    fn test_implements_completion_model_trait() {
        fn assert_completion_model<T: completion::CompletionModel>() {}
        assert_completion_model::<ClaudeCodeCompletionModel>();
    }

    #[test]
    fn test_completion_model_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ClaudeCodeCompletionModel>();
    }

    #[test]
    fn test_stream_json_input_serialization() {
        let input = StreamJsonInput {
            msg_type: "user".to_string(),
            message: CliMessage {
                role: "user".to_string(),
                content: vec![CliContent {
                    content_type: "text".to_string(),
                    text: "Hello".to_string(),
                }],
            },
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("\"type\":\"user\""));
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"text\":\"Hello\""));
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
    fn test_conversation_with_tool_results() {
        use rig::message::{ToolResult, ToolResultContent};

        let messages = vec![
            Message::User {
                content: OneOrMany::one(UserContent::text("Take a screenshot")),
            },
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(ToolCall::new(
                    "call_1".to_string(),
                    ToolFunction::new(
                        "screenshot".to_string(),
                        serde_json::json!({"url": "https://example.com"}),
                    ),
                ))),
            },
            Message::User {
                content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                    id: "call_1".to_string(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::text(
                        "screenshot taken successfully",
                    )),
                })),
            },
        ];
        let result = build_stdin_input(&messages, &[]);

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let text = parsed["message"]["content"][0]["text"].as_str().unwrap();

        // Assistant tool call should use XML format
        assert!(text.contains("<tool_call>"));
        assert!(text.contains("\"name\":\"screenshot\""));

        // Tool result should use XML format
        assert!(text.contains("<tool_result call_id=\"call_1\">"));
        assert!(text.contains("screenshot taken successfully"));
        assert!(text.contains("</tool_result>"));
    }
}
