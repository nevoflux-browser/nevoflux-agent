//! Integration tests for the Gemini CLI provider.
//!
//! These tests spawn the real `gemini` CLI as a subprocess.
//! Requirements:
//! - `gemini` CLI installed and on PATH
//! - `gemini` CLI authenticated
//!
//! The model used by real CLI tests is read from ~/.config/nevoflux/config.toml
//! (`llm.gemini_cli.model`). Falls back to "gemini-2.5-pro" if not configured.
//!
//! Run with: `cargo test -p nevoflux-llm --test gemini_cli_provider -- --nocapture`

use std::str::FromStr;

use nevoflux_llm::providers::gemini_cli::{
    extract_tool_calls_from_text, format_tool_call_as_xml, format_tool_definitions_prompt,
    format_tool_result_as_xml, parse_gemini_output, GeminiCliClient, GeminiCliCompletionModel,
    GeminiCliCompletionResponse, GeminiCliUsage,
};
use nevoflux_llm::{api_key_env_var, default_context_window_for, default_model_for, ProviderType};
use rig::completion::{AssistantContent, CompletionModel, CompletionResponse, ToolDefinition};
use rig::message::{Message, UserContent};
use rig::OneOrMany;
use serde::Deserialize;

// =============================================================================
// Config Loading Helper
// =============================================================================

/// Minimal config structures for reading llm.gemini_cli.model from config.toml.
#[derive(Debug, Deserialize, Default)]
struct TestConfig {
    #[serde(default)]
    llm: TestLlmConfig,
}

#[derive(Debug, Deserialize, Default)]
struct TestLlmConfig {
    #[serde(default)]
    gemini_cli: TestGeminiCliConfig,
}

#[derive(Debug, Deserialize, Default)]
struct TestGeminiCliConfig {
    model: Option<String>,
}

/// Load the Gemini CLI model name from ~/.config/nevoflux/config.toml.
/// Falls back to "gemini-2.5-pro" if the config file is missing or the field is not set.
fn configured_model() -> String {
    let config_path = dirs::config_dir()
        .map(|d| d.join("nevoflux").join("config.toml"))
        .unwrap_or_default();

    if let Ok(content) = std::fs::read_to_string(&config_path) {
        if let Ok(config) = toml::from_str::<TestConfig>(&content) {
            if let Some(model) = config.llm.gemini_cli.model {
                if !model.is_empty() {
                    eprintln!("Using model from config: {}", model);
                    return model;
                }
            }
        }
    }

    let fallback = "gemini-2.5-pro".to_string();
    eprintln!(
        "No gemini_cli model in config, using fallback: {}",
        fallback
    );
    fallback
}

// =============================================================================
// Factory / ProviderType Integration Tests
// =============================================================================

#[test]
fn test_gemini_cli_provider_type_from_str() {
    assert_eq!(
        ProviderType::from_str("gemini-cli").unwrap(),
        ProviderType::GeminiCli
    );
    assert_eq!(
        ProviderType::from_str("gemini_cli").unwrap(),
        ProviderType::GeminiCli
    );
    // case-insensitive
    assert_eq!(
        ProviderType::from_str("Gemini-Cli").unwrap(),
        ProviderType::GeminiCli
    );
}

#[test]
fn test_gemini_cli_default_model() {
    let model = default_model_for(ProviderType::GeminiCli);
    assert_eq!(model, "gemini-2.5-pro");
}

#[test]
fn test_gemini_cli_default_context_window() {
    let window = default_context_window_for(ProviderType::GeminiCli);
    assert_eq!(window, 1_000_000);
}

#[test]
fn test_gemini_cli_api_key_env_var() {
    let env_var = api_key_env_var(ProviderType::GeminiCli);
    assert_eq!(env_var, "GEMINI_API_KEY");
}

#[test]
fn test_gemini_cli_provider_type_debug() {
    assert_eq!(format!("{:?}", ProviderType::GeminiCli), "GeminiCli");
}

#[test]
fn test_gemini_cli_provider_type_copy_clone() {
    let p = ProviderType::GeminiCli;
    let copied = p;
    let cloned = p;
    assert_eq!(p, copied);
    assert_eq!(p, cloned);
}

// =============================================================================
// Client Construction Tests
// =============================================================================

#[test]
fn test_client_default_command() {
    let client = GeminiCliClient::new("gemini");
    assert_eq!(client.command(), "gemini");
    // No API key by default — verify via Debug output
    let debug = format!("{:?}", client);
    assert!(debug.contains("None"), "Expected no api_key by default");
}

#[test]
fn test_client_custom_command_path() {
    let client = GeminiCliClient::new("/usr/local/bin/gemini");
    assert_eq!(client.command(), "/usr/local/bin/gemini");
}

#[test]
fn test_client_with_api_key_chain() {
    let client = GeminiCliClient::new("gemini").with_api_key("gemini-key-test123");
    // api_key is pub(crate), so verify via Debug redaction
    let debug = format!("{:?}", client);
    assert!(debug.contains("REDACTED"), "API key should be redacted");
    assert!(
        !debug.contains("gemini-key-test123"),
        "API key must not leak"
    );
    assert_eq!(client.command(), "gemini");
}

#[test]
fn test_client_clone_preserves_state() {
    let client = GeminiCliClient::new("gemini").with_api_key("key-abc");
    let cloned = client.clone();
    assert_eq!(cloned.command(), "gemini");
    // Both original and clone should show redacted key
    let debug = format!("{:?}", cloned);
    assert!(debug.contains("REDACTED"));
}

#[test]
fn test_client_with_working_dir_builder() {
    let client = GeminiCliClient::new("gemini").with_working_dir("/tmp/workspace");
    let debug = format!("{:?}", client);
    assert!(debug.contains("/tmp/workspace"));
}

#[test]
fn test_client_working_dir_none_by_default() {
    let client = GeminiCliClient::new("gemini");
    let debug = format!("{:?}", client);
    // working_dir should be None
    assert!(
        debug.contains("working_dir: None"),
        "Expected working_dir: None, got: {}",
        debug
    );
}

#[test]
fn test_client_debug_never_leaks_api_key() {
    let client = GeminiCliClient::new("gemini").with_api_key("super-secret-gemini-key-12345");
    let debug = format!("{:?}", client);
    assert!(
        !debug.contains("super-secret-gemini-key-12345"),
        "API key must not appear in Debug output"
    );
    assert!(debug.contains("REDACTED"));
    assert!(debug.contains("gemini"));
}

#[test]
fn test_completion_model_creation_various_models() {
    let client = GeminiCliClient::new("gemini");

    let pro = client.completion_model("gemini-2.5-pro");
    assert_eq!(pro.model(), "gemini-2.5-pro");

    let flash = client.completion_model("gemini-2.5-flash");
    assert_eq!(flash.model(), "gemini-2.5-flash");

    let lite = client.completion_model("gemini-2.5-flash-lite");
    assert_eq!(lite.model(), "gemini-2.5-flash-lite");

    let custom = client.completion_model("some-custom-model");
    assert_eq!(custom.model(), "some-custom-model");
}

// =============================================================================
// Config Loading Tests
// =============================================================================

#[test]
fn test_configured_model_returns_value() {
    // configured_model() should always return a non-empty string
    let model = configured_model();
    assert!(!model.is_empty(), "configured_model() must not be empty");
}

// =============================================================================
// Response Parsing Tests (Realistic CLI Output)
// =============================================================================

#[test]
fn test_parse_simple_text_output() {
    let resp = parse_gemini_output("Hello, world!").unwrap();
    assert_eq!(resp.content, "Hello, world!");
    assert_eq!(resp.usage.input_tokens, 0);
    assert_eq!(resp.usage.output_tokens, 0);
}

#[test]
fn test_parse_realistic_multiline_output() {
    // Simulate realistic Gemini CLI output with markdown
    let output = r#"Here's a simple Python function:

```python
def greet(name):
    return f"Hello, {name}!"
```

This function takes a name parameter and returns a greeting string."#;

    let resp = parse_gemini_output(output).unwrap();
    assert!(resp.content.contains("def greet(name)"));
    assert!(resp.content.contains("```python"));
    assert!(resp.content.contains("greeting string"));
}

#[test]
fn test_parse_filters_cached_credentials_prefix() {
    let output = "Loaded cached credentials for project\nActual response text here";
    let resp = parse_gemini_output(output).unwrap();
    assert_eq!(resp.content, "Actual response text here");
    assert!(!resp.content.contains("Loaded cached credentials"));
}

#[test]
fn test_parse_filters_multiple_cached_credentials_lines() {
    let output =
        "Loaded cached credentials for project foo\nLoaded cached credentials for bar\nReal output";
    let resp = parse_gemini_output(output).unwrap();
    assert_eq!(resp.content, "Real output");
}

#[test]
fn test_parse_empty_output() {
    let resp = parse_gemini_output("").unwrap();
    assert!(resp.content.is_empty());
}

#[test]
fn test_parse_only_whitespace() {
    let resp = parse_gemini_output("  \n  \n  ").unwrap();
    assert!(resp.content.is_empty());
}

#[test]
fn test_parse_filters_empty_lines_between_content() {
    let output = "\n\nHello\n\nWorld\n\n";
    let resp = parse_gemini_output(output).unwrap();
    assert_eq!(resp.content, "Hello\nWorld");
}

#[test]
fn test_parse_preserves_content_lines_order() {
    let output = "Line 1\nLine 2\nLine 3\nLine 4";
    let resp = parse_gemini_output(output).unwrap();
    assert_eq!(resp.content, "Line 1\nLine 2\nLine 3\nLine 4");
}

#[test]
fn test_parse_mixed_noise_and_content() {
    let output = "Loaded cached credentials for default\n\nThe answer is 42.\n\n";
    let resp = parse_gemini_output(output).unwrap();
    assert_eq!(resp.content, "The answer is 42.");
}

#[test]
fn test_parse_unicode_content() {
    let output = "你好世界\nこんにちは\n🌍🌎🌏";
    let resp = parse_gemini_output(output).unwrap();
    assert!(resp.content.contains("你好世界"));
    assert!(resp.content.contains("こんにちは"));
    assert!(resp.content.contains("🌍🌎🌏"));
}

#[test]
fn test_parse_output_with_tool_call_markers() {
    // Gemini CLI text output that happens to contain tool call markers
    let output = r#"I'll read that file for you.
<tool_call>
{"id":"call_1","name":"read_file","arguments":{"path":"config.toml"}}
</tool_call>"#;
    let resp = parse_gemini_output(output).unwrap();
    // parse_gemini_output itself just joins non-empty lines — tool extraction happens later
    assert!(resp.content.contains("<tool_call>"));
    assert!(resp.content.contains("read_file"));
}

// =============================================================================
// Tool Extraction Tests
// =============================================================================

#[test]
fn test_extract_tool_calls_from_gemini_output() {
    let output = r#"I'll read that file for you.
<tool_call>
{"id":"call_1","name":"read_file","arguments":{"path":"config.toml"}}
</tool_call>"#;
    let resp = parse_gemini_output(output).unwrap();
    let (cleaned, calls) = extract_tool_calls_from_text(&resp.content);

    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments["path"], "config.toml");
    assert!(cleaned.contains("I'll read that file"));
    assert!(!cleaned.contains("<tool_call>"));
}

#[test]
fn test_extract_multiple_tool_calls() {
    let text = r#"Let me search and then read.
<tool_call>
{"id":"call_1","name":"search","arguments":{"query":"rust"}}
</tool_call>
<tool_call>
{"id":"call_2","name":"read_file","arguments":{"path":"src/main.rs"}}
</tool_call>"#;
    let (cleaned, calls) = extract_tool_calls_from_text(text);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "search");
    assert_eq!(calls[1].name, "read_file");
    assert!(cleaned.contains("Let me search"));
    assert!(!cleaned.contains("<tool_call>"));
}

#[test]
fn test_extract_tool_calls_malformed_json() {
    let text = "<tool_call>\n{not valid json}\n</tool_call>";
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
fn test_extract_tool_calls_no_markers() {
    let text = "Just a regular response with no tools at all.";
    let (cleaned, calls) = extract_tool_calls_from_text(text);
    assert!(calls.is_empty());
    assert_eq!(cleaned, text);
}

#[test]
fn test_extract_tool_calls_missing_fields_defaults() {
    // Missing id and arguments — should default to empty string / empty object
    let text = "<tool_call>\n{\"name\":\"screenshot\"}\n</tool_call>";
    let (_, calls) = extract_tool_calls_from_text(text);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "");
    assert_eq!(calls[0].name, "screenshot");
    assert!(calls[0].arguments.is_object());
}

// =============================================================================
// Tool Prompt Formatting Tests
// =============================================================================

#[test]
fn test_format_tool_definitions_single_tool() {
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
    assert!(prompt.contains("unique id"));
}

#[test]
fn test_format_tool_definitions_multiple_tools() {
    let tools = vec![
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        },
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Write a file".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}}),
        },
    ];
    let prompt = format_tool_definitions_prompt(&tools);
    assert!(prompt.contains("read_file"));
    assert!(prompt.contains("write_file"));
    assert!(prompt.contains("Read a file"));
    assert!(prompt.contains("Write a file"));
}

#[test]
fn test_format_tool_definitions_empty_returns_empty() {
    let prompt = format_tool_definitions_prompt(&[]);
    assert!(prompt.is_empty());
}

#[test]
fn test_format_tool_call_as_xml_roundtrip() {
    let args = serde_json::json!({"path": "config.toml", "content": "data"});
    let xml = format_tool_call_as_xml("call_42", "write_file", &args);
    assert!(xml.contains("<tool_call>"));
    assert!(xml.contains("</tool_call>"));
    assert!(xml.contains("\"id\":\"call_42\""));
    assert!(xml.contains("\"name\":\"write_file\""));
    assert!(xml.contains("config.toml"));

    // It should be extractable back
    let (_, calls) = extract_tool_calls_from_text(&xml);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_42");
    assert_eq!(calls[0].name, "write_file");
    assert_eq!(calls[0].arguments["path"], "config.toml");
}

#[test]
fn test_format_tool_result_as_xml() {
    let result = format_tool_result_as_xml("call_1", "file contents here");
    assert_eq!(
        result,
        "<tool_result call_id=\"call_1\">\nfile contents here\n</tool_result>"
    );
}

// =============================================================================
// Type Serialization / Deserialization Tests
// =============================================================================

#[test]
fn test_gemini_cli_usage_default() {
    let usage = GeminiCliUsage::default();
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 0);
}

#[test]
fn test_gemini_cli_usage_serialization_roundtrip() {
    let usage = GeminiCliUsage {
        input_tokens: 123,
        output_tokens: 456,
    };
    let json = serde_json::to_string(&usage).unwrap();
    let deserialized: GeminiCliUsage = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.input_tokens, 123);
    assert_eq!(deserialized.output_tokens, 456);
}

#[test]
fn test_gemini_cli_usage_deserialize_with_defaults() {
    // Empty JSON object — fields should default to 0
    let usage: GeminiCliUsage = serde_json::from_str("{}").unwrap();
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 0);
}

#[test]
fn test_gemini_cli_completion_response_serialization() {
    let resp = GeminiCliCompletionResponse {
        content: "Hello world".to_string(),
        usage: GeminiCliUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
    };
    let json = serde_json::to_string(&resp).unwrap();
    let deserialized: GeminiCliCompletionResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.content, "Hello world");
    assert_eq!(deserialized.usage.input_tokens, 10);
    assert_eq!(deserialized.usage.output_tokens, 5);
}

// =============================================================================
// CompletionResponse Conversion Tests
// =============================================================================

#[test]
fn test_completion_response_conversion_with_usage() {
    let resp = GeminiCliCompletionResponse {
        content: "The answer is 42.".to_string(),
        usage: GeminiCliUsage {
            input_tokens: 100,
            output_tokens: 20,
        },
    };

    let rig_resp: CompletionResponse<GeminiCliCompletionResponse> = resp.try_into().unwrap();

    // Check usage
    assert_eq!(rig_resp.usage.input_tokens, 100);
    assert_eq!(rig_resp.usage.output_tokens, 20);
    assert_eq!(rig_resp.usage.total_tokens, 120);

    // Check content
    match rig_resp.choice.first() {
        AssistantContent::Text(t) => assert_eq!(t.text, "The answer is 42."),
        other => panic!("Expected Text, got {:?}", other),
    }
}

#[test]
fn test_completion_response_conversion_empty_is_error() {
    let resp = GeminiCliCompletionResponse {
        content: String::new(),
        usage: GeminiCliUsage::default(),
    };

    let result: Result<CompletionResponse<GeminiCliCompletionResponse>, _> = resp.try_into();
    assert!(result.is_err());
}

#[test]
fn test_completion_response_raw_response_preserved() {
    let resp = GeminiCliCompletionResponse {
        content: "test output".to_string(),
        usage: GeminiCliUsage {
            input_tokens: 5,
            output_tokens: 1,
        },
    };

    let rig_resp: CompletionResponse<GeminiCliCompletionResponse> = resp.try_into().unwrap();
    assert_eq!(rig_resp.raw_response.content, "test output");
    assert_eq!(rig_resp.raw_response.usage.input_tokens, 5);
    assert_eq!(rig_resp.raw_response.usage.output_tokens, 1);
}

#[test]
fn test_completion_response_zero_usage() {
    // Gemini CLI typically returns zero usage
    let resp = GeminiCliCompletionResponse {
        content: "Hello".to_string(),
        usage: GeminiCliUsage::default(),
    };

    let rig_resp: CompletionResponse<GeminiCliCompletionResponse> = resp.try_into().unwrap();
    assert_eq!(rig_resp.usage.input_tokens, 0);
    assert_eq!(rig_resp.usage.output_tokens, 0);
    assert_eq!(rig_resp.usage.total_tokens, 0);
}

// =============================================================================
// Trait Verification Tests
// =============================================================================

#[test]
fn test_completion_model_trait_implemented() {
    fn assert_completion_model<T: CompletionModel>() {}
    assert_completion_model::<GeminiCliCompletionModel>();
}

#[test]
fn test_send_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<GeminiCliClient>();
    assert_sync::<GeminiCliClient>();
    assert_send::<GeminiCliCompletionModel>();
    assert_sync::<GeminiCliCompletionModel>();
}

// =============================================================================
// Real Gemini CLI Subprocess Tests
// =============================================================================

/// Helper: check whether the `gemini` CLI is available.
fn gemini_cli_available() -> bool {
    gemini_command()
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build a `std::process::Command` for the `gemini` CLI, handling Windows
/// where npm installs it as a `.ps1`/`.cmd` script.
fn gemini_command() -> std::process::Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.args(["/C", "gemini"]);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("gemini")
    }
}

/// Async version of [`gemini_command`].
fn gemini_command_async() -> tokio::process::Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = tokio::process::Command::new("cmd.exe");
        cmd.args(["/C", "gemini"]);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        tokio::process::Command::new("gemini")
    }
}

#[tokio::test]
async fn test_real_cli_nonexistent_command_returns_error() {
    let client = GeminiCliClient::new("nonexistent-gemini-binary-xyz");
    let model = client.completion_model("gemini-2.5-pro");

    let result = model.completion_request("hello").send().await;

    assert!(result.is_err(), "Should fail when CLI binary not found");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("Failed to run gemini CLI"),
        "Error should mention CLI failure, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_real_cli_simple_completion() {
    if !gemini_cli_available() {
        eprintln!("SKIP: gemini CLI not available");
        return;
    }

    let model_name = configured_model();
    let client = GeminiCliClient::new("gemini");
    let model = client.completion_model(&model_name);

    let response = model
        .completion_request("Reply with exactly the word 'pong' and nothing else.")
        .send()
        .await;

    match response {
        Ok(resp) => {
            let text = resp
                .choice
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            println!("Response: {}", text);
            assert!(!text.is_empty(), "Response should not be empty");
            assert!(
                text.to_lowercase().contains("pong"),
                "Expected 'pong' in response, got: {}",
                text
            );
        }
        Err(e) => {
            panic!("Completion failed: {}", e);
        }
    }
}

#[tokio::test]
async fn test_real_cli_with_system_prompt() {
    if !gemini_cli_available() {
        eprintln!("SKIP: gemini CLI not available");
        return;
    }

    let model_name = configured_model();
    let client = GeminiCliClient::new("gemini");
    let model = client.completion_model(&model_name);

    let response = model
        .completion_request("What is your name?")
        .preamble(
            "You are a helpful assistant named GemBot. Always introduce yourself by name."
                .to_string(),
        )
        .send()
        .await;

    match response {
        Ok(resp) => {
            let text = resp
                .choice
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            println!("Response: {}", text);
            assert!(!text.is_empty());
            assert!(
                text.contains("GemBot"),
                "Expected system prompt identity 'GemBot' in response, got: {}",
                text
            );
        }
        Err(e) => {
            panic!("Completion with system prompt failed: {}", e);
        }
    }
}

#[tokio::test]
async fn test_real_cli_multi_turn_conversation() {
    if !gemini_cli_available() {
        eprintln!("SKIP: gemini CLI not available");
        return;
    }

    let model_name = configured_model();
    let client = GeminiCliClient::new("gemini");
    let model = client.completion_model(&model_name);

    // Build a multi-turn conversation where the assistant already knows the topic
    let chat_history = vec![
        Message::User {
            content: OneOrMany::one(UserContent::text("What is the capital of France?")),
        },
        Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text("The capital of France is Paris.")),
        },
    ];

    let response = model
        .completion_request(
            "Based on our previous exchange, what city did you mention? Reply with only the city name.",
        )
        .messages(chat_history)
        .send()
        .await;

    match response {
        Ok(resp) => {
            let text = resp
                .choice
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            println!("Response: {}", text);
            assert!(!text.is_empty());
            // The response should reference Paris since it's in the conversation history
            assert!(
                text.to_lowercase().contains("paris"),
                "Expected 'Paris' in response, got: {}",
                text
            );
        }
        Err(e) => {
            panic!("Multi-turn completion failed: {}", e);
        }
    }
}

#[tokio::test]
async fn test_real_cli_math_computation() {
    if !gemini_cli_available() {
        eprintln!("SKIP: gemini CLI not available");
        return;
    }

    let model_name = configured_model();
    let client = GeminiCliClient::new("gemini");
    let model = client.completion_model(&model_name);

    let response = model
        .completion_request("What is 13 * 29? Reply with only the number, nothing else.")
        .send()
        .await;

    match response {
        Ok(resp) => {
            let text = resp
                .choice
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            println!("Response: {}", text);
            assert!(
                text.contains("377"),
                "Expected '377' (13*29) in response, got: {}",
                text
            );
        }
        Err(e) => {
            panic!("Math computation failed: {}", e);
        }
    }
}

#[tokio::test]
async fn test_real_cli_raw_stdout_is_parseable() {
    if !gemini_cli_available() {
        eprintln!("SKIP: gemini CLI not available");
        return;
    }

    let model_name = configured_model();

    // Run the CLI directly to verify raw output can be parsed
    let output = gemini_command_async()
        .args(["-m", &model_name, "-p", "Say exactly: test123", "--yolo"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .expect("Failed to run gemini CLI");

    assert!(
        output.status.success(),
        "CLI exited with error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Raw CLI output:\n{}", stdout);

    // Verify parse_gemini_output handles the raw output correctly
    let resp = parse_gemini_output(&stdout).unwrap();
    assert!(
        !resp.content.is_empty(),
        "Parsed content should not be empty"
    );
    println!("Parsed content: {}", resp.content);
}
