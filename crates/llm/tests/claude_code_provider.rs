//! Integration tests for the Claude Code CLI provider.
//!
//! These tests spawn the real `claude` CLI as a subprocess.
//! Requirements:
//! - `claude` CLI installed and on PATH
//! - `claude` CLI authenticated (run `claude` once interactively to log in)
//!
//! The model used by real CLI tests is read from ~/.config/nevoflux/config.toml
//! (`llm.claude_code.model`). Falls back to "sonnet" if not configured.
//!
//! Run with: `cargo test -p nevoflux-llm --test claude_code_provider -- --nocapture`

use std::str::FromStr;

use nevoflux_llm::providers::claude_code::{
    parse_claude_output, ClaudeCodeClient, ClaudeCodeCompletionModel, ClaudeCodeCompletionResponse,
    ClaudeContentItem, ClaudeOutputEntry, ClaudeUsage,
};
use nevoflux_llm::{api_key_env_var, default_context_window_for, default_model_for, ProviderType};
use rig::completion::{AssistantContent, CompletionModel, CompletionResponse};
use rig::message::{Message, UserContent};
use rig::OneOrMany;
use serde::Deserialize;

// =============================================================================
// Config Loading Helper
// =============================================================================

/// Minimal config structures for reading llm.claude_code.model from config.toml.
#[derive(Debug, Deserialize, Default)]
struct TestConfig {
    #[serde(default)]
    llm: TestLlmConfig,
}

#[derive(Debug, Deserialize, Default)]
struct TestLlmConfig {
    #[serde(default)]
    claude_code: TestClaudeCodeConfig,
}

#[derive(Debug, Deserialize, Default)]
struct TestClaudeCodeConfig {
    model: Option<String>,
}

/// Load the Claude Code model name from ~/.config/nevoflux/config.toml.
/// Falls back to "sonnet" if the config file is missing or the field is not set.
fn configured_model() -> String {
    let config_path = dirs::config_dir()
        .map(|d| d.join("nevoflux").join("config.toml"))
        .unwrap_or_default();

    if let Ok(content) = std::fs::read_to_string(&config_path) {
        if let Ok(config) = toml::from_str::<TestConfig>(&content) {
            if let Some(model) = config.llm.claude_code.model {
                if !model.is_empty() {
                    eprintln!("Using model from config: {}", model);
                    return model;
                }
            }
        }
    }

    let fallback = "sonnet".to_string();
    eprintln!(
        "No claude_code model in config, using fallback: {}",
        fallback
    );
    fallback
}

// =============================================================================
// Factory / ProviderType Integration Tests
// =============================================================================

#[test]
fn test_claude_code_provider_type_from_str() {
    assert_eq!(
        ProviderType::from_str("claude-code").unwrap(),
        ProviderType::ClaudeCode
    );
    assert_eq!(
        ProviderType::from_str("claude_code").unwrap(),
        ProviderType::ClaudeCode
    );
    // case-insensitive
    assert_eq!(
        ProviderType::from_str("Claude-Code").unwrap(),
        ProviderType::ClaudeCode
    );
}

#[test]
fn test_claude_code_default_model() {
    let model = default_model_for(ProviderType::ClaudeCode);
    assert_eq!(model, "sonnet");
}

#[test]
fn test_claude_code_default_context_window() {
    let window = default_context_window_for(ProviderType::ClaudeCode);
    assert_eq!(window, 200_000);
}

#[test]
fn test_claude_code_api_key_env_var() {
    let env_var = api_key_env_var(ProviderType::ClaudeCode);
    assert_eq!(env_var, "ANTHROPIC_API_KEY");
}

#[test]
fn test_claude_code_provider_type_debug() {
    assert_eq!(format!("{:?}", ProviderType::ClaudeCode), "ClaudeCode");
}

#[test]
fn test_claude_code_provider_type_copy_clone() {
    let p = ProviderType::ClaudeCode;
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
    let client = ClaudeCodeClient::new("claude");
    assert_eq!(client.command(), "claude");
    // No API key by default — verify via Debug output
    let debug = format!("{:?}", client);
    assert!(debug.contains("None"), "Expected no api_key by default");
}

#[test]
fn test_client_custom_command_path() {
    let client = ClaudeCodeClient::new("/usr/local/bin/claude");
    assert_eq!(client.command(), "/usr/local/bin/claude");
}

#[test]
fn test_client_with_api_key_chain() {
    let client = ClaudeCodeClient::new("claude").with_api_key("sk-ant-test123");
    // api_key is pub(crate), so verify via Debug redaction
    let debug = format!("{:?}", client);
    assert!(debug.contains("REDACTED"), "API key should be redacted");
    assert!(!debug.contains("sk-ant-test123"), "API key must not leak");
    assert_eq!(client.command(), "claude");
}

#[test]
fn test_client_clone_preserves_state() {
    let client = ClaudeCodeClient::new("claude").with_api_key("key-abc");
    let cloned = client.clone();
    assert_eq!(cloned.command(), "claude");
    // Both original and clone should show redacted key
    let debug = format!("{:?}", cloned);
    assert!(debug.contains("REDACTED"));
}

#[test]
fn test_client_with_add_dirs_builder() {
    let client = ClaudeCodeClient::new("claude")
        .with_working_dir("/tmp/workspace")
        .with_add_dirs(vec![
            "/home/user/project".to_string(),
            "/tmp/extra".to_string(),
        ]);
    let debug = format!("{:?}", client);
    assert!(debug.contains("/tmp/workspace"));
    assert!(debug.contains("/home/user/project"));
    assert!(debug.contains("/tmp/extra"));
}

#[test]
fn test_client_with_add_dirs_empty_by_default() {
    let client = ClaudeCodeClient::new("claude");
    let debug = format!("{:?}", client);
    assert!(debug.contains("add_dirs: []"));
}

#[test]
fn test_client_debug_never_leaks_api_key() {
    let client = ClaudeCodeClient::new("claude").with_api_key("sk-ant-super-secret-key-12345");
    let debug = format!("{:?}", client);
    assert!(
        !debug.contains("sk-ant-super-secret-key-12345"),
        "API key must not appear in Debug output"
    );
    assert!(debug.contains("REDACTED"));
    assert!(debug.contains("claude"));
}

#[test]
fn test_completion_model_creation_various_models() {
    let client = ClaudeCodeClient::new("claude");

    let sonnet = client.completion_model("sonnet");
    assert_eq!(sonnet.model(), "sonnet");

    let opus = client.completion_model("opus");
    assert_eq!(opus.model(), "opus");

    let haiku = client.completion_model("haiku");
    assert_eq!(haiku.model(), "haiku");

    let full_id = client.completion_model("claude-sonnet-4-20250514");
    assert_eq!(full_id.model(), "claude-sonnet-4-20250514");
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
fn test_parse_realistic_cli_json_output() {
    // Realistic output from: claude -p "say hello" --output-format json --verbose
    let json = r#"[
        {
            "type": "assistant",
            "message": {
                "id": "msg_01XYZ",
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "Hello! How can I help you today?"
                    }
                ],
                "model": "claude-sonnet-4-20250514",
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 9,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        },
        {
            "type": "result",
            "subtype": "success",
            "cost_usd": 0.000123,
            "is_error": false,
            "duration_ms": 1234,
            "duration_api_ms": 1100,
            "num_turns": 1,
            "result": "Hello! How can I help you today?",
            "session_id": "abc123",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 9,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        }
    ]"#;

    let resp = parse_claude_output(json).unwrap();
    assert_eq!(resp.content, "Hello! How can I help you today?");
    assert!(resp.usage.input_tokens > 0);
    assert!(resp.usage.output_tokens > 0);
}

#[test]
fn test_parse_multi_block_assistant_response() {
    let json = r#"[
        {
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "Here's the answer:\n\n"},
                    {"type": "text", "text": "The result is 42."}
                ],
                "usage": {"input_tokens": 50, "output_tokens": 20}
            }
        },
        {
            "type": "result",
            "usage": {"input_tokens": 50, "output_tokens": 20}
        }
    ]"#;

    let resp = parse_claude_output(json).unwrap();
    assert_eq!(resp.content, "Here's the answer:\n\nThe result is 42.");
}

#[test]
fn test_parse_response_with_tool_use_content() {
    // Claude CLI may return tool_use items alongside text
    let json = r#"[
        {
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "I'll help with that. Let me search."},
                    {"type": "tool_use", "id": "toolu_01ABC", "name": "web_search", "input": {"query": "rust language"}}
                ],
                "usage": {"input_tokens": 30, "output_tokens": 15}
            }
        }
    ]"#;

    let resp = parse_claude_output(json).unwrap();
    // parse_claude_output extracts text only
    assert_eq!(resp.content, "I'll help with that. Let me search.");
}

#[test]
fn test_parse_result_entry_only() {
    let json = r#"{
        "type": "result",
        "result": "Done!",
        "usage": {"input_tokens": 5, "output_tokens": 1}
    }"#;

    let resp = parse_claude_output(json).unwrap();
    assert_eq!(resp.content, "Done!");
}

#[test]
fn test_parse_empty_content_array() {
    let json = r#"[
        {
            "type": "assistant",
            "message": {
                "content": [],
                "usage": {"input_tokens": 10, "output_tokens": 0}
            }
        }
    ]"#;

    let resp = parse_claude_output(json).unwrap();
    assert!(resp.content.is_empty());
}

#[test]
fn test_parse_no_usage_field() {
    let json = r#"[
        {
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "OK"}
                ]
            }
        }
    ]"#;

    let resp = parse_claude_output(json).unwrap();
    assert_eq!(resp.content, "OK");
    assert_eq!(resp.usage.input_tokens, 0);
    assert_eq!(resp.usage.output_tokens, 0);
}

#[test]
fn test_parse_whitespace_plain_text() {
    let resp = parse_claude_output("  \n  Hello  \n  ").unwrap();
    assert_eq!(resp.content, "Hello");
}

// =============================================================================
// Type Deserialization Tests
// =============================================================================

#[test]
fn test_claude_output_entry_assistant_variant() {
    let json = r#"{
        "type": "assistant",
        "message": {
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }
    }"#;
    let entry: ClaudeOutputEntry = serde_json::from_str(json).unwrap();
    match entry {
        ClaudeOutputEntry::Assistant { message } => {
            assert_eq!(message.content.len(), 1);
            assert!(message.usage.is_some());
        }
        _ => panic!("Expected Assistant variant"),
    }
}

#[test]
fn test_claude_output_entry_result_variant() {
    let json = r#"{
        "type": "result",
        "result": "finished",
        "usage": {"input_tokens": 100, "output_tokens": 50}
    }"#;
    let entry: ClaudeOutputEntry = serde_json::from_str(json).unwrap();
    match entry {
        ClaudeOutputEntry::Result { result, usage } => {
            assert_eq!(result.as_deref(), Some("finished"));
            let u = usage.unwrap();
            assert_eq!(u.input_tokens, 100);
            assert_eq!(u.output_tokens, 50);
        }
        _ => panic!("Expected Result variant"),
    }
}

#[test]
fn test_claude_content_item_text() {
    let json = r#"{"type": "text", "text": "hello world"}"#;
    let item: ClaudeContentItem = serde_json::from_str(json).unwrap();
    match item {
        ClaudeContentItem::Text { text } => assert_eq!(text, "hello world"),
        _ => panic!("Expected Text"),
    }
}

#[test]
fn test_claude_content_item_tool_use() {
    let json = r#"{
        "type": "tool_use",
        "id": "toolu_ABC",
        "name": "bash",
        "input": {"command": "ls -la"}
    }"#;
    let item: ClaudeContentItem = serde_json::from_str(json).unwrap();
    match item {
        ClaudeContentItem::ToolUse { id, name, input } => {
            assert_eq!(id, "toolu_ABC");
            assert_eq!(name, "bash");
            assert_eq!(input["command"], "ls -la");
        }
        _ => panic!("Expected ToolUse"),
    }
}

#[test]
fn test_claude_usage_serialization_roundtrip() {
    let usage = ClaudeUsage {
        input_tokens: 123,
        output_tokens: 456,
    };
    let json = serde_json::to_string(&usage).unwrap();
    let deserialized: ClaudeUsage = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.input_tokens, 123);
    assert_eq!(deserialized.output_tokens, 456);
}

// =============================================================================
// CompletionResponse Conversion Tests
// =============================================================================

#[test]
fn test_completion_response_conversion_with_usage() {
    let resp = ClaudeCodeCompletionResponse {
        content: "The answer is 42.".to_string(),
        usage: ClaudeUsage {
            input_tokens: 100,
            output_tokens: 20,
        },
        tool_calls: Vec::new(),
    };

    let rig_resp: CompletionResponse<ClaudeCodeCompletionResponse> = resp.try_into().unwrap();

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
    let resp = ClaudeCodeCompletionResponse {
        content: String::new(),
        usage: ClaudeUsage::default(),
        tool_calls: Vec::new(),
    };

    let result: Result<CompletionResponse<ClaudeCodeCompletionResponse>, _> = resp.try_into();
    assert!(result.is_err());
}

#[test]
fn test_completion_response_raw_response_preserved() {
    let resp = ClaudeCodeCompletionResponse {
        content: "test".to_string(),
        usage: ClaudeUsage {
            input_tokens: 5,
            output_tokens: 1,
        },
        tool_calls: Vec::new(),
    };

    let rig_resp: CompletionResponse<ClaudeCodeCompletionResponse> = resp.try_into().unwrap();
    assert_eq!(rig_resp.raw_response.content, "test");
    assert_eq!(rig_resp.raw_response.usage.input_tokens, 5);
}

// =============================================================================
// Trait Verification Tests
// =============================================================================

#[test]
fn test_completion_model_trait_implemented() {
    fn assert_completion_model<T: CompletionModel>() {}
    assert_completion_model::<ClaudeCodeCompletionModel>();
}

#[test]
fn test_send_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<ClaudeCodeClient>();
    assert_sync::<ClaudeCodeClient>();
    assert_send::<ClaudeCodeCompletionModel>();
    assert_sync::<ClaudeCodeCompletionModel>();
}

// =============================================================================
// Real Claude CLI Subprocess Tests
// =============================================================================

/// Helper: check whether the `claude` CLI is available and authenticated.
fn claude_cli_available() -> bool {
    claude_command()
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build a `std::process::Command` for the `claude` CLI, resolving the full
/// path on Windows where npm installs it as a `.cmd` script.
fn claude_command() -> std::process::Command {
    std::process::Command::new(resolve_cli("claude"))
}

/// Async version of [`claude_command`].
fn claude_command_async() -> tokio::process::Command {
    tokio::process::Command::new(resolve_cli("claude"))
}

/// Resolve a CLI program name to its full path on Windows (.cmd/.exe).
/// On non-Windows, returns the name unchanged.
fn resolve_cli(name: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        if let Some(path_var) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let cmd_path = dir.join(format!("{}.cmd", name));
                if cmd_path.is_file() {
                    return cmd_path.to_string_lossy().into_owned();
                }
                let exe_path = dir.join(format!("{}.exe", name));
                if exe_path.is_file() {
                    return exe_path.to_string_lossy().into_owned();
                }
            }
        }
        name.to_string()
    }
    #[cfg(not(target_os = "windows"))]
    {
        name.to_string()
    }
}

#[tokio::test]
async fn test_real_cli_simple_completion() {
    if !claude_cli_available() {
        eprintln!("SKIP: claude CLI not available");
        return;
    }

    let model_name = configured_model();
    let client = ClaudeCodeClient::new("claude");
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

            // Check usage is reported
            assert!(resp.usage.input_tokens > 0, "input_tokens should be > 0");
            assert!(resp.usage.output_tokens > 0, "output_tokens should be > 0");
            assert!(resp.usage.total_tokens > 0, "total_tokens should be > 0");
        }
        Err(e) => {
            panic!("Completion failed: {}", e);
        }
    }
}

#[tokio::test]
async fn test_real_cli_with_system_prompt() {
    if !claude_cli_available() {
        eprintln!("SKIP: claude CLI not available");
        return;
    }

    let model_name = configured_model();
    let client = ClaudeCodeClient::new("claude");
    let model = client.completion_model(&model_name);

    let response = model
        .completion_request("What is your name?")
        .preamble(
            "You are a helpful assistant named NevoBot. Always introduce yourself by name."
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
                text.contains("NevoBot"),
                "Expected system prompt identity 'NevoBot' in response, got: {}",
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
    if !claude_cli_available() {
        eprintln!("SKIP: claude CLI not available");
        return;
    }

    let model_name = configured_model();
    let client = ClaudeCodeClient::new("claude");
    let model = client.completion_model(&model_name);

    // Build a multi-turn conversation
    let chat_history = vec![
        Message::User {
            content: OneOrMany::one(UserContent::text(
                "Remember this number: 7392. Just say OK.",
            )),
        },
        Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text("OK")),
        },
    ];

    let response = model
        .completion_request(
            "What was the number I asked you to remember? Reply with only the number.",
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
            assert!(
                text.contains("7392"),
                "Expected '7392' in response, got: {}",
                text
            );
        }
        Err(e) => {
            panic!("Multi-turn completion failed: {}", e);
        }
    }
}

#[tokio::test]
async fn test_real_cli_json_output_is_parseable() {
    if !claude_cli_available() {
        eprintln!("SKIP: claude CLI not available");
        return;
    }

    let model_name = configured_model();

    // Run the CLI directly with stream-json (matching how completion model invokes it)
    let stdin_input = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Say exactly: test123"}]}}"#;

    let mut child = claude_command_async()
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            "--model",
            &model_name,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn claude CLI");

    // Write stdin input and close
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(stdin_input.as_bytes()).await.unwrap();
        stdin.write_all(b"\n").await.unwrap();
        drop(stdin);
    }

    let output = child
        .wait_with_output()
        .await
        .expect("Failed to wait for claude CLI");

    assert!(
        output.status.success(),
        "CLI exited with error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Raw CLI output:\n{}", stdout);

    // With stream-json, output is newline-delimited JSON
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty(), "Expected at least one output line");

    // Each line should be valid JSON
    for line in &lines {
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Failed to parse JSON line: {}\nLine: {}", e, line));
    }

    // Should contain at least one assistant or result entry
    let has_known_entry = lines.iter().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
            .map(|t| t == "assistant" || t == "result")
            .unwrap_or(false)
    });
    assert!(has_known_entry, "Expected an assistant or result entry");

    // Verify parse_claude_output handles the output correctly
    let resp = parse_claude_output(&stdout).unwrap();
    assert!(
        !resp.content.is_empty(),
        "Parsed content should not be empty"
    );
    println!("Parsed content: {}", resp.content);
}

#[tokio::test]
async fn test_real_cli_nonexistent_command_returns_error() {
    let client = ClaudeCodeClient::new("nonexistent-claude-binary-xyz");
    let model = client.completion_model("sonnet");

    let result = model.completion_request("hello").send().await;

    assert!(result.is_err(), "Should fail when CLI binary not found");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("Failed to run claude CLI"),
        "Error should mention CLI failure, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_real_cli_math_computation() {
    if !claude_cli_available() {
        eprintln!("SKIP: claude CLI not available");
        return;
    }

    let model_name = configured_model();
    let client = ClaudeCodeClient::new("claude");
    let model = client.completion_model(&model_name);

    let response = model
        .completion_request("What is 17 * 23? Reply with only the number, nothing else.")
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
                text.contains("391"),
                "Expected '391' (17*23) in response, got: {}",
                text
            );
        }
        Err(e) => {
            panic!("Math computation failed: {}", e);
        }
    }
}
