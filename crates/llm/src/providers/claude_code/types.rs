//! Response types for parsing Claude Code CLI JSON output.

use rig::completion::{
    AssistantContent, CompletionError, CompletionResponse, ToolDefinition, Usage,
};
use rig::OneOrMany;
use serde::{Deserialize, Serialize};

/// An entry in the Claude CLI JSON array output.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ClaudeOutputEntry {
    /// An assistant message with content.
    #[serde(rename = "assistant")]
    Assistant { message: ClaudeAssistantMessage },
    /// The final result entry with usage info.
    #[serde(rename = "result")]
    Result {
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        usage: Option<ClaudeUsage>,
    },
}

/// An assistant message from the Claude CLI.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeAssistantMessage {
    /// Content items in the message.
    pub content: Vec<ClaudeContentItem>,
    /// Token usage for this message.
    #[serde(default)]
    pub usage: Option<ClaudeUsage>,
}

/// A content item in a Claude CLI response.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ClaudeContentItem {
    /// Text content.
    #[serde(rename = "text")]
    Text { text: String },
    /// Tool use content.
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

/// Token usage statistics from Claude CLI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeUsage {
    /// Number of input tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Number of output tokens.
    #[serde(default)]
    pub output_tokens: u64,
}

/// Wrapper response for rig CompletionResponse conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCodeCompletionResponse {
    /// The raw text content from the CLI.
    pub content: String,
    /// Token usage statistics.
    pub usage: ClaudeUsage,
}

impl TryFrom<ClaudeCodeCompletionResponse> for CompletionResponse<ClaudeCodeCompletionResponse> {
    type Error = CompletionError;

    fn try_from(value: ClaudeCodeCompletionResponse) -> Result<Self, Self::Error> {
        let usage = Usage {
            input_tokens: value.usage.input_tokens,
            output_tokens: value.usage.output_tokens,
            total_tokens: value.usage.input_tokens + value.usage.output_tokens,
        };

        if value.content.is_empty() {
            return Err(CompletionError::ResponseError(
                "Empty response from Claude Code CLI".into(),
            ));
        }

        Ok(CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text(&value.content)),
            usage,
            raw_response: value,
        })
    }
}

/// Collect text and usage from a sequence of JSON values representing CLI output entries.
///
/// Extracts text from assistant messages. The `result` entry's text field is only
/// used as a fallback when no assistant text was found (it duplicates assistant content).
fn collect_from_entries(values: &[serde_json::Value]) -> (String, ClaudeUsage) {
    let mut assistant_text = Vec::new();
    let mut result_text: Option<String> = None;
    let mut usage = ClaudeUsage::default();

    for value in values {
        // Try to parse as a known entry type; skip unknown types (e.g. "system")
        let Ok(entry) = serde_json::from_value::<ClaudeOutputEntry>(value.clone()) else {
            continue;
        };
        match &entry {
            ClaudeOutputEntry::Assistant { message } => {
                for item in &message.content {
                    if let ClaudeContentItem::Text { text } = item {
                        assistant_text.push(text.clone());
                    }
                }
                if let Some(u) = &message.usage {
                    usage.input_tokens += u.input_tokens;
                    usage.output_tokens += u.output_tokens;
                }
            }
            ClaudeOutputEntry::Result {
                result,
                usage: entry_usage,
            } => {
                if let Some(r) = result {
                    if !r.is_empty() {
                        result_text = Some(r.clone());
                    }
                }
                if let Some(u) = entry_usage {
                    usage.input_tokens += u.input_tokens;
                    usage.output_tokens += u.output_tokens;
                }
            }
        }
    }

    // Prefer assistant text; fall back to result text if no assistant content
    let content = if assistant_text.is_empty() {
        result_text.unwrap_or_default()
    } else {
        assistant_text.join("")
    };

    (content, usage)
}

/// Parse the Claude CLI JSON output into text content and usage.
///
/// Supports three formats:
/// 1. JSON array (standard `--output-format json`)
/// 2. Newline-delimited JSON (from `--output-format stream-json`)
/// 3. Single JSON object
/// 4. Plain text fallback
pub fn parse_claude_output(output: &str) -> Result<ClaudeCodeCompletionResponse, String> {
    // Try parsing as a JSON array first (standard --output-format json).
    // We parse as Vec<Value> to tolerate unknown entry types like "system".
    if let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(output) {
        let (content, usage) = collect_from_entries(&values);
        return Ok(ClaudeCodeCompletionResponse { content, usage });
    }

    // Try newline-delimited JSON (stream-json output format)
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() > 1 || lines.first().map(|l| l.starts_with('{')).unwrap_or(false) {
        let values: Vec<serde_json::Value> = lines
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if !values.is_empty() {
            let (content, usage) = collect_from_entries(&values);
            if !content.is_empty() || values.len() > 1 {
                return Ok(ClaudeCodeCompletionResponse { content, usage });
            }
        }
    }

    // Try parsing as a single JSON object (some CLI versions)
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output) {
        let (content, usage) = collect_from_entries(&[value]);
        return Ok(ClaudeCodeCompletionResponse { content, usage });
    }

    // Fall back to treating the entire output as plain text
    Ok(ClaudeCodeCompletionResponse {
        content: output.trim().to_string(),
        usage: ClaudeUsage::default(),
    })
}

/// A tool call extracted from text-injected XML markers.
#[derive(Debug, Clone)]
pub struct ExtractedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Format tool definitions as XML for injection into the system prompt.
///
/// Returns an empty string if `tools` is empty.
pub fn format_tool_definitions_prompt(tools: &[ToolDefinition]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "\n\n# Available Tools\n\nYou have access to the following tools:\n\n<tools>\n",
    );
    for tool in tools {
        let params = serde_json::to_string(&tool.parameters).unwrap_or_default();
        out.push_str(&format!(
            "<tool name=\"{}\" description=\"{}\">\n{}\n</tool>\n",
            tool.name, tool.description, params
        ));
    }
    out.push_str("</tools>\n\n");
    out.push_str("When you need to use a tool, output EXACTLY this format:\n");
    out.push_str("<tool_call>\n");
    out.push_str("{\"id\":\"call_1\",\"name\":\"tool_name\",\"arguments\":{...}}\n");
    out.push_str("</tool_call>\n");
    out.push_str("After outputting a tool call, STOP and wait for the tool result.\n");
    out.push_str("Generate a unique id for each tool call (e.g., \"call_1\", \"call_2\").\n");
    out.push_str("Do NOT wrap tool_call in markdown code blocks.");
    out
}

/// Extract tool calls from text containing `<tool_call>...</tool_call>` markers.
///
/// Returns `(cleaned_text, extracted_tool_calls)` where `cleaned_text` is the
/// original text with all tool call markers removed.
pub fn extract_tool_calls_from_text(text: &str) -> (String, Vec<ExtractedToolCall>) {
    let mut tool_calls = Vec::new();
    let mut cleaned = String::new();
    let mut remaining = text;

    loop {
        let Some(start_idx) = remaining.find("<tool_call>") else {
            cleaned.push_str(remaining);
            break;
        };

        // Add text before the marker
        cleaned.push_str(&remaining[..start_idx]);

        let after_start = &remaining[start_idx + "<tool_call>".len()..];

        let Some(end_idx) = after_start.find("</tool_call>") else {
            // No closing tag — keep the raw text as-is
            cleaned.push_str(&remaining[start_idx..]);
            break;
        };

        let json_str = after_start[..end_idx].trim();

        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
            let id = parsed
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = parsed
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = parsed
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
            tool_calls.push(ExtractedToolCall {
                id,
                name,
                arguments,
            });
        } else {
            // Malformed JSON — keep the raw text
            cleaned.push_str(
                &remaining
                    [start_idx..start_idx + "<tool_call>".len() + end_idx + "</tool_call>".len()],
            );
        }

        remaining = &after_start[end_idx + "</tool_call>".len()..];
    }

    let cleaned = cleaned.trim().to_string();
    (cleaned, tool_calls)
}

/// Format a tool call as XML for inclusion in conversation history.
pub fn format_tool_call_as_xml(id: &str, name: &str, arguments: &serde_json::Value) -> String {
    let args_str = serde_json::to_string(arguments).unwrap_or_default();
    format!(
        "<tool_call>\n{{\"id\":\"{}\",\"name\":\"{}\",\"arguments\":{}}}\n</tool_call>",
        id, name, args_str
    )
}

/// Format a tool result as XML for inclusion in conversation history.
pub fn format_tool_result_as_xml(tool_call_id: &str, content: &str) -> String {
    format!(
        "<tool_result call_id=\"{}\">\n{}\n</tool_result>",
        tool_call_id, content
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json_array_output() {
        let json = r#"[
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {"type": "text", "text": "Hello, world!"}
                    ],
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                }
            },
            {
                "type": "result",
                "result": "",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        ]"#;

        let resp = parse_claude_output(json).unwrap();
        assert_eq!(resp.content, "Hello, world!");
        assert_eq!(resp.usage.input_tokens, 20);
        assert_eq!(resp.usage.output_tokens, 10);
    }

    #[test]
    fn test_parse_single_assistant_entry() {
        let json = r#"{
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "Hi there!"}
                ],
                "usage": {"input_tokens": 5, "output_tokens": 3}
            }
        }"#;

        let resp = parse_claude_output(json).unwrap();
        assert_eq!(resp.content, "Hi there!");
        assert_eq!(resp.usage.input_tokens, 5);
    }

    #[test]
    fn test_parse_plain_text_fallback() {
        let text = "Just some plain text output";
        let resp = parse_claude_output(text).unwrap();
        assert_eq!(resp.content, "Just some plain text output");
        assert_eq!(resp.usage.input_tokens, 0);
    }

    #[test]
    fn test_parse_multiple_text_items() {
        let json = r#"[
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {"type": "text", "text": "Hello "},
                        {"type": "text", "text": "world!"}
                    ]
                }
            }
        ]"#;

        let resp = parse_claude_output(json).unwrap();
        assert_eq!(resp.content, "Hello world!");
    }

    #[test]
    fn test_completion_response_conversion() {
        let resp = ClaudeCodeCompletionResponse {
            content: "Test response".to_string(),
            usage: ClaudeUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        };

        let rig_resp: CompletionResponse<ClaudeCodeCompletionResponse> = resp.try_into().unwrap();
        let first = rig_resp.choice.first();
        assert!(matches!(first, AssistantContent::Text(_)));
        assert_eq!(rig_resp.usage.total_tokens, 15);
    }

    #[test]
    fn test_empty_response_returns_error() {
        let resp = ClaudeCodeCompletionResponse {
            content: String::new(),
            usage: ClaudeUsage::default(),
        };

        let result: Result<CompletionResponse<ClaudeCodeCompletionResponse>, _> = resp.try_into();
        assert!(result.is_err());
    }

    #[test]
    fn test_claude_usage_default() {
        let usage = ClaudeUsage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn test_parse_array_with_system_entry() {
        // Real CLI --verbose output starts with a "system" init entry
        let json = r#"[
            {
                "type": "system",
                "subtype": "init",
                "cwd": "/tmp",
                "session_id": "abc123",
                "tools": []
            },
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {"type": "text", "text": "pong"}
                    ],
                    "usage": {"input_tokens": 10, "output_tokens": 3}
                }
            },
            {
                "type": "result",
                "result": "",
                "usage": {"input_tokens": 10, "output_tokens": 3}
            }
        ]"#;

        let resp = parse_claude_output(json).unwrap();
        assert_eq!(resp.content, "pong");
        assert_eq!(resp.usage.input_tokens, 20);
        assert_eq!(resp.usage.output_tokens, 6);
    }

    #[test]
    fn test_parse_array_with_unknown_entry_types() {
        // Ensure any unknown entry types are gracefully skipped
        let json = r#"[
            {"type": "system", "subtype": "init", "data": {}},
            {"type": "unknown_future_type", "foo": "bar"},
            {
                "type": "assistant",
                "message": {
                    "content": [{"type": "text", "text": "hello"}]
                }
            },
            {"type": "result", "result": ""}
        ]"#;

        let resp = parse_claude_output(json).unwrap();
        assert_eq!(resp.content, "hello");
    }

    #[test]
    fn test_parse_newline_delimited_json() {
        // Simulate stream-json output: each entry is a separate JSON line
        let output = concat!(
            r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"abc","tools":[]}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"pong"}],"usage":{"input_tokens":10,"output_tokens":3}}}"#,
            "\n",
            r#"{"type":"result","result":"","usage":{"input_tokens":10,"output_tokens":3}}"#,
        );

        let resp = parse_claude_output(output).unwrap();
        assert_eq!(resp.content, "pong");
        assert_eq!(resp.usage.input_tokens, 20);
        assert_eq!(resp.usage.output_tokens, 6);
    }

    #[test]
    fn test_parse_newline_delimited_json_with_empty_lines() {
        let output = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":5,"output_tokens":2}}}"#,
            "\n\n",
            r#"{"type":"result","result":"","usage":{"input_tokens":5,"output_tokens":2}}"#,
            "\n",
        );

        let resp = parse_claude_output(output).unwrap();
        assert_eq!(resp.content, "hello");
    }

    #[test]
    fn test_tool_use_content_item_deserialization() {
        let json = r#"{
            "type": "tool_use",
            "id": "tool_123",
            "name": "get_weather",
            "input": {"location": "Tokyo"}
        }"#;

        let item: ClaudeContentItem = serde_json::from_str(json).unwrap();
        match item {
            ClaudeContentItem::ToolUse { id, name, input } => {
                assert_eq!(id, "tool_123");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Tokyo");
            }
            _ => panic!("Expected ToolUse"),
        }
    }

    #[test]
    fn test_format_tool_definitions_prompt() {
        let tools = vec![ToolDefinition {
            name: "screenshot".to_string(),
            description: "Take a screenshot".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"url": {"type": "string"}}}),
        }];
        let prompt = format_tool_definitions_prompt(&tools);
        assert!(prompt.contains("<tools>"));
        assert!(prompt.contains("</tools>"));
        assert!(prompt.contains(r#"<tool name="screenshot" description="Take a screenshot">"#));
        assert!(prompt.contains("<tool_call>"));
        assert!(prompt.contains("STOP and wait"));
    }

    #[test]
    fn test_format_tool_definitions_empty() {
        let prompt = format_tool_definitions_prompt(&[]);
        assert!(prompt.is_empty());
    }

    #[test]
    fn test_extract_tool_calls_simple() {
        let text = r#"<tool_call>
{"id":"call_1","name":"screenshot","arguments":{"url":"https://example.com"}}
</tool_call>"#;
        let (cleaned, calls) = extract_tool_calls_from_text(text);
        assert!(cleaned.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "screenshot");
        assert_eq!(calls[0].arguments["url"], "https://example.com");
    }

    #[test]
    fn test_extract_tool_calls_multiple() {
        let text = r#"<tool_call>
{"id":"call_1","name":"read","arguments":{"path":"a.txt"}}
</tool_call>
<tool_call>
{"id":"call_2","name":"write","arguments":{"path":"b.txt","content":"hi"}}
</tool_call>"#;
        let (cleaned, calls) = extract_tool_calls_from_text(text);
        assert!(cleaned.is_empty());
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[1].name, "write");
    }

    #[test]
    fn test_extract_tool_calls_with_text() {
        let text = r#"Let me take a screenshot for you.
<tool_call>
{"id":"call_1","name":"screenshot","arguments":{}}
</tool_call>
Done!"#;
        let (cleaned, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "screenshot");
        assert!(cleaned.contains("Let me take a screenshot"));
        assert!(cleaned.contains("Done!"));
        assert!(!cleaned.contains("<tool_call>"));
    }

    #[test]
    fn test_extract_tool_calls_none() {
        let text = "Just a regular response with no tools.";
        let (cleaned, calls) = extract_tool_calls_from_text(text);
        assert!(calls.is_empty());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn test_extract_tool_calls_malformed_json() {
        let text = r#"<tool_call>
{not valid json}
</tool_call>"#;
        let (cleaned, calls) = extract_tool_calls_from_text(text);
        assert!(calls.is_empty());
        // Malformed JSON is kept in cleaned text
        assert!(cleaned.contains("{not valid json}"));
    }

    #[test]
    fn test_extract_tool_calls_missing_closing_tag() {
        let text = "Hello <tool_call>\n{\"id\":\"call_1\",\"name\":\"test\",\"arguments\":{}}";
        let (cleaned, calls) = extract_tool_calls_from_text(text);
        assert!(calls.is_empty());
        // Raw text preserved when no closing tag
        assert!(cleaned.contains("<tool_call>"));
    }

    #[test]
    fn test_format_tool_result_as_xml() {
        let result = format_tool_result_as_xml("call_1", "file contents here");
        assert_eq!(
            result,
            "<tool_result call_id=\"call_1\">\nfile contents here\n</tool_result>"
        );
    }

    #[test]
    fn test_format_tool_call_as_xml() {
        let args = serde_json::json!({"path": "config.toml"});
        let result = format_tool_call_as_xml("call_1", "read", &args);
        assert!(result.contains("<tool_call>"));
        assert!(result.contains("</tool_call>"));
        assert!(result.contains("\"name\":\"read\""));
        assert!(result.contains("\"id\":\"call_1\""));
        assert!(result.contains("config.toml"));
    }
}
