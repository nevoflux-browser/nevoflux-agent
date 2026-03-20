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
use rig::message::{
    DocumentSourceKind, Image, Message, MimeType, ToolCall, ToolFunction, ToolResultContent,
    UserContent,
};
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
    ///
    /// Tool definitions are injected as text in the system prompt; the model
    /// outputs `<tool_call>` XML markers which the daemon extracts and executes.
    /// The streaming code stops the CLI subprocess as soon as tool calls are
    /// detected, preventing the model from hallucinating `<tool_result>` blocks.
    fn build_command(&self, system_prompt: Option<&str>, tools: &[ToolDefinition]) -> Command {
        // On Windows, npm installs CLI tools as .ps1/.cmd scripts (e.g. claude.ps1).
        // Command::new("claude") only finds .exe files, so we need cmd.exe /C to
        // resolve the script via PATHEXT.
        #[cfg(target_os = "windows")]
        let mut cmd = {
            let mut c = Command::new("cmd.exe");
            c.arg("/C").arg(self.client.command());
            c
        };
        #[cfg(not(target_os = "windows"))]
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

        // On Windows, hide the console window for the CLI subprocess
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
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
///
/// Supports text and image content blocks for the Claude CLI stream-json format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum CliContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: CliImageSource },
}

/// Image source for CLI serialization (base64 encoded).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CliImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
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

        // Build content blocks: image blocks first, then conversation text
        let mut content_blocks = collect_image_blocks(messages);
        content_blocks.push(CliContent::Text { text: combined });

        let input = StreamJsonInput {
            msg_type: "user".to_string(),
            message: CliMessage {
                role: "user".to_string(),
                content: content_blocks,
            },
        };
        serde_json::to_string(&input).unwrap_or_default()
    } else {
        // Single or multiple user-only messages: send each as a separate line
        let mut lines = Vec::new();

        for (i, msg) in messages.iter().enumerate() {
            if let Message::User { content } = msg {
                let mut content_blocks = build_cli_content_blocks(content);

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
                    // Prepend document text block before existing content
                    content_blocks.insert(0, CliContent::Text { text: doc_text });
                }

                let input = StreamJsonInput {
                    msg_type: "user".to_string(),
                    message: CliMessage {
                        role: "user".to_string(),
                        content: content_blocks,
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

/// Extract text content from UserContent (text-only, for multi-turn serialization).
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

/// Build CLI content blocks from UserContent, including image blocks.
fn build_cli_content_blocks(content: &OneOrMany<UserContent>) -> Vec<CliContent> {
    let mut blocks = Vec::new();
    for c in content.iter() {
        match c {
            UserContent::Text(t) => {
                blocks.push(CliContent::Text {
                    text: t.text.clone(),
                });
            }
            UserContent::ToolResult(tr) => {
                let result_text: Vec<String> = tr
                    .content
                    .iter()
                    .filter_map(|rc| match rc {
                        ToolResultContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect();
                blocks.push(CliContent::Text {
                    text: format_tool_result_as_xml(&tr.id, &result_text.join("")),
                });
            }
            UserContent::Image(img) => {
                if let Some(cli_img) = image_to_cli_content(img) {
                    blocks.push(cli_img);
                }
            }
            _ => {}
        }
    }
    blocks
}

/// Convert a rig Image to a CLI image content block.
fn image_to_cli_content(img: &Image) -> Option<CliContent> {
    let base64_data = match &img.data {
        DocumentSourceKind::Base64(data) => data.clone(),
        _ => return None,
    };
    let media_type = img
        .media_type
        .as_ref()
        .map(|m| m.to_mime_type().to_string())
        .unwrap_or_else(|| "image/png".to_string());

    Some(CliContent::Image {
        source: CliImageSource {
            source_type: "base64".to_string(),
            media_type,
            data: base64_data,
        },
    })
}

/// Collect image content blocks from all user messages.
fn collect_image_blocks(messages: &[Message]) -> Vec<CliContent> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content),
            _ => None,
        })
        .flat_map(|content| {
            content.iter().filter_map(|c| match c {
                UserContent::Image(img) => image_to_cli_content(img),
                _ => None,
            })
        })
        .collect()
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

/// Process a single stream-json event from the Claude CLI.
///
/// `saw_native_tool_use` is mutable state carried across events. Once an `assistant`
/// event with native `tool_use` content blocks is seen, all subsequent `<tool_call>`
/// XML extraction from text/delta events is suppressed to prevent duplicate tool calls.
fn process_stream_event(
    entry: serde_json::Value,
    saw_native_tool_use: &mut bool,
) -> Vec<Result<RawStreamingChoice<ClaudeCodeStreamingResponse>, CompletionError>> {
    let event_type = entry.get("type").and_then(|t| t.as_str());

    // Handle stream-json "assistant" events
    if event_type == Some("assistant") {
        if let Some(contents) = entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            let mut items = Vec::new();

            // Check if there are native tool_use items
            let has_native_tool_use = contents
                .iter()
                .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"));

            if has_native_tool_use {
                *saw_native_tool_use = true;
            }

            // Collect text content
            let text: String = contents
                .iter()
                .filter_map(|c| {
                    if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                        c.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                if *saw_native_tool_use {
                    // Clean <tool_call> markers from text but don't emit text-extracted
                    // tool calls — native tool_use items are preferred to avoid duplicates.
                    let (cleaned, _) = extract_tool_calls_from_text(&text);
                    if !cleaned.is_empty() {
                        items.push(Ok(RawStreamingChoice::Message(cleaned)));
                    }
                } else {
                    // No native tool_use seen — extract tool calls from text as fallback
                    items.extend(emit_text_and_tool_calls(&text));
                }
            }

            // Collect native tool_use content items
            for c in contents {
                if c.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let id = c
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = c
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = c
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                    items.push(Ok(RawStreamingChoice::ToolCall(RawStreamingToolCall::new(
                        id, name, arguments,
                    ))));
                }
            }

            if !items.is_empty() {
                return items;
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
            if *saw_native_tool_use {
                // Suppress <tool_call> XML extraction — native tool_use already covers these
                let (cleaned, _) = extract_tool_calls_from_text(text);
                if !cleaned.is_empty() {
                    return vec![Ok(RawStreamingChoice::Message(cleaned))];
                }
                return vec![];
            }
            return emit_text_and_tool_calls(text);
        }
    }

    // Handle plain text output at top level
    if let Some(text) = entry.get("text").and_then(|t| t.as_str()) {
        if !text.is_empty() {
            if *saw_native_tool_use {
                // Suppress <tool_call> XML extraction — native tool_use already covers these
                let (cleaned, _) = extract_tool_calls_from_text(text);
                if !cleaned.is_empty() {
                    return vec![Ok(RawStreamingChoice::Message(cleaned))];
                }
                return vec![];
            }
            return emit_text_and_tool_calls(text);
        }
    }

    vec![]
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

        // Extract <tool_call> XML markers from text (also cleans the text).
        // Prefer native tool_use items over text-extracted ones to avoid duplicates —
        // Claude CLI may return the same call as both a native tool_use content block
        // AND as <tool_call> XML in the text output.
        let (cleaned_text, text_tool_calls) = extract_tool_calls_from_text(&response.content);
        let tool_calls = if response.tool_calls.is_empty() {
            text_tool_calls
        } else {
            response.tool_calls.clone()
        };

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

        // Spawn a background task to read and log stderr so CLI errors are visible
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let reader = tokio::io::BufReader::new(stderr);
                let mut lines = tokio::io::AsyncBufReadExt::lines(reader);
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        eprintln!("[claude-cli stderr] {}", line);
                    }
                }
            });
        }

        let reader = tokio::io::BufReader::new(stdout);
        let lines =
            tokio_stream::wrappers::LinesStream::new(tokio::io::AsyncBufReadExt::lines(reader));

        // Use scan + flat_map to emit multiple items (text + tool calls) from stream events.
        //
        // State tuple: (saw_native_tool_use, tool_call_emitted)
        //   - saw_native_tool_use: Once an `assistant` event with native `tool_use`
        //     content blocks is seen, <tool_call> XML extraction from text/delta events
        //     is suppressed to avoid duplicate tool calls.
        //   - tool_call_emitted: Once a tool call is emitted (native or text-extracted),
        //     the stream stops on the next iteration. This prevents the model from
        //     wasting tokens on hallucinated <tool_result> blocks that it generates
        //     instead of stopping and waiting for actual tool execution.
        type StreamItems =
            Vec<Result<RawStreamingChoice<ClaudeCodeStreamingResponse>, CompletionError>>;
        let stream = lines
            .scan(
                (false, false),
                |(saw_native_tool_use, tool_call_emitted), line_result| {
                    // Stop the stream if a tool call was already emitted.
                    // The ChildGuardStream will kill the CLI subprocess.
                    if *tool_call_emitted {
                        return futures::future::ready(None);
                    }

                    let items: StreamItems = match line_result {
                        Ok(line) => {
                            if line.trim().is_empty() {
                                vec![]
                            } else if let Ok(entry) =
                                serde_json::from_str::<serde_json::Value>(&line)
                            {
                                process_stream_event(entry, saw_native_tool_use)
                            } else if !line.starts_with('{') {
                                // Non-JSON line — pass through as text
                                vec![Ok(RawStreamingChoice::Message(line))]
                            } else {
                                vec![]
                            }
                        }
                        Err(e) => vec![Err(CompletionError::ProviderError(e.to_string()))],
                    };

                    // Check if any tool calls were emitted — stop on next iteration
                    if items
                        .iter()
                        .any(|item| matches!(item, Ok(RawStreamingChoice::ToolCall(_))))
                    {
                        *tool_call_emitted = true;
                    }

                    futures::future::ready(Some(items))
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
                content: vec![CliContent::Text {
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

    #[test]
    fn test_build_stdin_input_with_image() {
        use rig::message::{DocumentSourceKind, Image};

        let messages = vec![Message::User {
            content: OneOrMany::many(vec![
                UserContent::text("Describe this image"),
                UserContent::Image(Image {
                    data: DocumentSourceKind::Base64("iVBOR...".to_string()),
                    media_type: Some(rig::message::ImageMediaType::PNG),
                    detail: None,
                    additional_params: None,
                }),
            ])
            .unwrap(),
        }];
        let result = build_stdin_input(&messages, &[]);

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let content = parsed["message"]["content"].as_array().unwrap();

        // Should have both text and image content blocks
        assert_eq!(content.len(), 2);

        // First block: text
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Describe this image");

        // Second block: image
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "iVBOR...");
    }

    #[test]
    fn test_build_stdin_input_multi_turn_with_image() {
        use rig::message::{DocumentSourceKind, Image};

        let messages = vec![
            Message::User {
                content: OneOrMany::many(vec![
                    UserContent::text("What's in this screenshot?"),
                    UserContent::Image(Image {
                        data: DocumentSourceKind::Base64("AAAA".to_string()),
                        media_type: Some(rig::message::ImageMediaType::JPEG),
                        detail: None,
                        additional_params: None,
                    }),
                ])
                .unwrap(),
            },
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text("It's a dashboard.")),
            },
            Message::User {
                content: OneOrMany::one(UserContent::text("Make it better")),
            },
        ];
        let result = build_stdin_input(&messages, &[]);

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let content = parsed["message"]["content"].as_array().unwrap();

        // Should have image block + text block
        assert!(content.len() >= 2);

        // Image block should be present
        let has_image = content
            .iter()
            .any(|c| c["type"] == "image" && c["source"]["data"] == "AAAA");
        assert!(has_image, "Image block should be included in multi-turn");

        // Conversation text should be present
        let text_block = content.iter().find(|c| c["type"] == "text").unwrap();
        let text = text_block["text"].as_str().unwrap();
        assert!(text.contains("<conversation_history>"));
        assert!(text.contains("[user]: What's in this screenshot?"));
        assert!(text.contains("[assistant]: It's a dashboard."));
    }

    #[test]
    fn test_process_stream_event_native_tool_use_suppresses_text_xml() {
        // Simulate: assistant event with native tool_use sets the flag
        let mut saw_native = false;
        let assistant_event = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "Let me take a screenshot."},
                    {"type": "tool_use", "id": "toolu_abc", "name": "browser_screenshot", "input": {"tab_id": 5}}
                ]
            }
        });
        let items = process_stream_event(assistant_event, &mut saw_native);
        assert!(saw_native, "Flag should be set after native tool_use");

        // Should emit text + 1 native tool call (not text-extracted)
        let tool_calls: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Ok(RawStreamingChoice::ToolCall(_))))
            .collect();
        assert_eq!(tool_calls.len(), 1, "Only 1 native tool call");

        // Now simulate: a later text event with <tool_call> XML should be suppressed
        let text_event = serde_json::json!({
            "type": "content_block_delta",
            "delta": {
                "text": "<tool_call>\n{\"id\":\"call_1\",\"name\":\"browser_screenshot\",\"arguments\":{\"tab_id\":5}}\n</tool_call>"
            }
        });
        let items2 = process_stream_event(text_event, &mut saw_native);
        let tool_calls2: Vec<_> = items2
            .iter()
            .filter(|i| matches!(i, Ok(RawStreamingChoice::ToolCall(_))))
            .collect();
        assert_eq!(
            tool_calls2.len(),
            0,
            "Text-extracted tool calls should be suppressed after native tool_use"
        );
    }

    #[test]
    fn test_process_stream_event_no_native_allows_text_xml() {
        // When no native tool_use is seen, text XML extraction should work
        let mut saw_native = false;
        let text_event = serde_json::json!({
            "text": "I'll help.\n<tool_call>\n{\"id\":\"call_1\",\"name\":\"screenshot\",\"arguments\":{}}\n</tool_call>"
        });
        let items = process_stream_event(text_event, &mut saw_native);
        assert!(!saw_native);
        let tool_calls: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Ok(RawStreamingChoice::ToolCall(_))))
            .collect();
        assert_eq!(tool_calls.len(), 1, "Text-extracted tool call should work");
    }

    #[test]
    fn test_process_stream_event_assistant_with_only_text_tool_calls() {
        // assistant event with NO native tool_use — text extraction is the fallback
        let mut saw_native = false;
        let event = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "Sure.\n<tool_call>\n{\"id\":\"call_1\",\"name\":\"read\",\"arguments\":{\"path\":\"a.txt\"}}\n</tool_call>"}
                ]
            }
        });
        let items = process_stream_event(event, &mut saw_native);
        assert!(
            !saw_native,
            "Flag should NOT be set without native tool_use"
        );
        let tool_calls: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Ok(RawStreamingChoice::ToolCall(_))))
            .collect();
        assert_eq!(
            tool_calls.len(),
            1,
            "Text-extracted tool call should be used as fallback"
        );
    }
}
