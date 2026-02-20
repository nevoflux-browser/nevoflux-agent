//! Response types for parsing Gemini CLI output.

use rig::completion::{
    AssistantContent, CompletionError, CompletionResponse, ToolDefinition, Usage,
};
use rig::OneOrMany;
use serde::{Deserialize, Serialize};

/// Token usage statistics from Gemini CLI.
///
/// The Gemini CLI does not report usage by default, so this is often zero.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeminiCliUsage {
    /// Number of input tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Number of output tokens.
    #[serde(default)]
    pub output_tokens: u64,
}

/// Wrapper response for rig CompletionResponse conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiCliCompletionResponse {
    /// The raw text content from the CLI.
    pub content: String,
    /// Token usage statistics.
    pub usage: GeminiCliUsage,
}

impl TryFrom<GeminiCliCompletionResponse> for CompletionResponse<GeminiCliCompletionResponse> {
    type Error = CompletionError;

    fn try_from(value: GeminiCliCompletionResponse) -> Result<Self, Self::Error> {
        let usage = Usage {
            input_tokens: value.usage.input_tokens,
            output_tokens: value.usage.output_tokens,
            total_tokens: value.usage.input_tokens + value.usage.output_tokens,
        };

        if value.content.is_empty() {
            return Err(CompletionError::ResponseError(
                "Empty response from Gemini CLI".into(),
            ));
        }

        Ok(CompletionResponse {
            choice: OneOrMany::one(AssistantContent::text(&value.content)),
            usage,
            raw_response: value,
        })
    }
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
    out.push_str("When you need to use a tool, output EXACTLY this XML format (do NOT execute the tool yourself):\n");
    out.push_str("<tool_call>\n");
    out.push_str("{\"id\":\"call_1\",\"name\":\"tool_name\",\"arguments\":{...}}\n");
    out.push_str("</tool_call>\n");
    out.push_str("After outputting a tool call, STOP and wait for the tool result.\n");
    out.push_str("Generate a unique id for each tool call (e.g., \"call_1\", \"call_2\").\n");
    out.push_str("Do NOT wrap tool_call in markdown code blocks.\n");
    out.push_str("Do NOT use shell, read_file, write_file, web fetch, or any other built-in tools. ONLY use the <tool_call> XML protocol above.");
    out
}

/// Extract tool calls from text containing `<tool_call>...</tool_call>` markers.
///
/// Returns `(cleaned_text, extracted_tool_calls)` where `cleaned_text` is the
/// original text with all tool call markers removed. Also strips any hallucinated
/// `<tool_result>` blocks from the content.
pub fn extract_tool_calls_from_text(text: &str) -> (String, Vec<ExtractedToolCall>) {
    let mut tool_calls = Vec::new();
    let mut cleaned = String::new();
    let mut remaining = text;

    loop {
        let Some(start_idx) = remaining.find("<tool_call>") else {
            // No more tool_call markers.
            // If we already extracted tool calls, discard any trailing text —
            // the model should have stopped after </tool_call> but may have
            // hallucinated tool results, explanations, or other garbage.
            if tool_calls.is_empty() {
                cleaned.push_str(remaining);
            }
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

        // Use streaming deserializer to tolerate trailing garbage (e.g. extra `}`)
        // that LLMs sometimes produce.
        let parsed_result = serde_json::from_str::<serde_json::Value>(json_str).or_else(|_| {
            let mut de = serde_json::Deserializer::from_str(json_str);
            serde_json::Value::deserialize(&mut de)
        });

        if let Ok(parsed) = parsed_result {
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

/// Parse the Gemini CLI text output into a completion response.
///
/// The Gemini CLI outputs plain text to stdout. Lines starting with
/// "Loaded cached credentials" are filtered out.
pub fn parse_gemini_output(output: &str) -> Result<GeminiCliCompletionResponse, String> {
    let content: String = output
        .lines()
        .filter(|l| {
            let trimmed = l.trim();
            !trimmed.is_empty() && !trimmed.starts_with("Loaded cached credentials")
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(GeminiCliCompletionResponse {
        content,
        usage: GeminiCliUsage::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_text() {
        let text = "Hello, world!";
        let resp = parse_gemini_output(text).unwrap();
        assert_eq!(resp.content, "Hello, world!");
        assert_eq!(resp.usage.input_tokens, 0);
    }

    #[test]
    fn test_parse_filters_cached_credentials() {
        let text = "Loaded cached credentials for project\nHello, world!";
        let resp = parse_gemini_output(text).unwrap();
        assert_eq!(resp.content, "Hello, world!");
    }

    #[test]
    fn test_parse_filters_empty_lines() {
        let text = "\n\nHello\n\nWorld\n\n";
        let resp = parse_gemini_output(text).unwrap();
        assert_eq!(resp.content, "Hello\nWorld");
    }

    #[test]
    fn test_parse_empty_output() {
        let text = "";
        let resp = parse_gemini_output(text).unwrap();
        assert!(resp.content.is_empty());
    }

    #[test]
    fn test_parse_multiline() {
        let text = "Line 1\nLine 2\nLine 3";
        let resp = parse_gemini_output(text).unwrap();
        assert_eq!(resp.content, "Line 1\nLine 2\nLine 3");
    }

    #[test]
    fn test_completion_response_conversion() {
        let resp = GeminiCliCompletionResponse {
            content: "Test response".to_string(),
            usage: GeminiCliUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        };

        let rig_resp: CompletionResponse<GeminiCliCompletionResponse> = resp.try_into().unwrap();
        let first = rig_resp.choice.first();
        assert!(matches!(first, AssistantContent::Text(_)));
        assert_eq!(rig_resp.usage.total_tokens, 15);
    }

    #[test]
    fn test_empty_response_returns_error() {
        let resp = GeminiCliCompletionResponse {
            content: String::new(),
            usage: GeminiCliUsage::default(),
        };

        let result: Result<CompletionResponse<GeminiCliCompletionResponse>, _> = resp.try_into();
        assert!(result.is_err());
    }

    #[test]
    fn test_usage_default() {
        let usage = GeminiCliUsage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
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
        // Text after </tool_call> is discarded (model should stop after tool call)
        assert!(!cleaned.contains("Done!"));
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
