//! E2E tests for LLM providers.
//!
//! These tests require real API keys and are ignored by default.
//! Run with: `cargo test -p nevoflux-daemon --test llm_e2e -- --ignored`
//!
//! Required environment variables:
//! - ANTHROPIC_API_KEY: For Anthropic tests
//! - OPENAI_API_KEY: For OpenAI tests
//! - OPENROUTER_API_KEY: For OpenRouter tests
//! - DEEPSEEK_API_KEY: For DeepSeek tests
//! - DASHSCOPE_API_KEY: For Qwen tests

use nevoflux_daemon::wasm::llm::{
    execute_llm_chat, LlmChatRequest, LlmMessage, LlmToolCall, LlmToolDefinition,
};
use nevoflux_llm::ProviderType;
use serde_json::json;

fn get_env_key(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

#[tokio::test]
#[ignore]
async fn test_anthropic_chat() {
    let api_key = match get_env_key("ANTHROPIC_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: ANTHROPIC_API_KEY not set");
            return;
        }
    };

    let request = LlmChatRequest {
        messages: vec![LlmMessage::user(
            "Say 'Hello from Anthropic!' and nothing else.",
        )],
        system: Some("You are a helpful assistant. Be very concise.".into()),
        temperature: Some(0.0),
        max_tokens: Some(50),
        tools: None,
    };

    let response = execute_llm_chat(
        ProviderType::Anthropic,
        &api_key,
        "claude-3-haiku-20240307",
        request,
    )
    .await;

    match response {
        Ok(resp) => {
            println!("Anthropic response: {:?}", resp);
            assert!(!resp.content.is_empty());
            assert!(resp.content.to_lowercase().contains("hello"));
        }
        Err(e) => panic!("Anthropic chat failed: {:?}", e),
    }
}

#[tokio::test]
#[ignore]
async fn test_openai_chat() {
    let api_key = match get_env_key("OPENAI_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
    };

    let request = LlmChatRequest {
        messages: vec![LlmMessage::user(
            "Say 'Hello from OpenAI!' and nothing else.",
        )],
        system: Some("You are a helpful assistant. Be very concise.".into()),
        temperature: Some(0.0),
        max_tokens: Some(50),
        tools: None,
    };

    let response = execute_llm_chat(ProviderType::OpenAi, &api_key, "gpt-4o-mini", request).await;

    match response {
        Ok(resp) => {
            println!("OpenAI response: {:?}", resp);
            assert!(!resp.content.is_empty());
            assert!(resp.content.to_lowercase().contains("hello"));
        }
        Err(e) => panic!("OpenAI chat failed: {:?}", e),
    }
}

#[tokio::test]
#[ignore]
async fn test_openrouter_chat() {
    let api_key = match get_env_key("OPENROUTER_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: OPENROUTER_API_KEY not set");
            return;
        }
    };

    let request = LlmChatRequest {
        messages: vec![LlmMessage::user(
            "Say 'Hello from OpenRouter!' and nothing else.",
        )],
        system: Some("You are a helpful assistant. Be very concise.".into()),
        temperature: Some(0.0),
        max_tokens: Some(50),
        tools: None,
    };

    let response = execute_llm_chat(
        ProviderType::OpenRouter,
        &api_key,
        "openai/gpt-4o-mini",
        request,
    )
    .await;

    match response {
        Ok(resp) => {
            println!("OpenRouter response: {:?}", resp);
            assert!(!resp.content.is_empty());
            assert!(resp.content.to_lowercase().contains("hello"));
        }
        Err(e) => panic!("OpenRouter chat failed: {:?}", e),
    }
}

#[tokio::test]
#[ignore]
async fn test_deepseek_chat() {
    let api_key = match get_env_key("DEEPSEEK_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: DEEPSEEK_API_KEY not set");
            return;
        }
    };

    let request = LlmChatRequest {
        messages: vec![LlmMessage::user(
            "Say 'Hello from DeepSeek!' and nothing else.",
        )],
        system: Some("You are a helpful assistant. Be very concise.".into()),
        temperature: Some(0.0),
        max_tokens: Some(50),
        tools: None,
    };

    let response =
        execute_llm_chat(ProviderType::DeepSeek, &api_key, "deepseek-chat", request).await;

    match response {
        Ok(resp) => {
            println!("DeepSeek response: {:?}", resp);
            assert!(!resp.content.is_empty());
            assert!(resp.content.to_lowercase().contains("hello"));
        }
        Err(e) => panic!("DeepSeek chat failed: {:?}", e),
    }
}

#[tokio::test]
#[ignore]
async fn test_qwen_chat() {
    let api_key = match get_env_key("DASHSCOPE_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: DASHSCOPE_API_KEY not set");
            return;
        }
    };

    let request = LlmChatRequest {
        messages: vec![LlmMessage::user("Say 'Hello from Qwen!' and nothing else.")],
        system: Some("You are a helpful assistant. Be very concise.".into()),
        temperature: Some(0.0),
        max_tokens: Some(50),
        tools: None,
    };

    let response = execute_llm_chat(ProviderType::Qwen, &api_key, "qwen-turbo", request).await;

    match response {
        Ok(resp) => {
            println!("Qwen response: {:?}", resp);
            assert!(!resp.content.is_empty());
            assert!(resp.content.to_lowercase().contains("hello"));
        }
        Err(e) => panic!("Qwen chat failed: {:?}", e),
    }
}

#[tokio::test]
#[ignore]
async fn test_openai_tool_calling() {
    let api_key = match get_env_key("OPENAI_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
    };

    // Step 1: Send request with tool definition
    let request = LlmChatRequest {
        messages: vec![LlmMessage::user("What's the weather in Tokyo?")],
        system: Some(
            "You are a helpful assistant. Use the get_weather tool to answer weather questions."
                .into(),
        ),
        temperature: Some(0.0),
        max_tokens: Some(200),
        tools: Some(vec![LlmToolDefinition {
            name: "get_weather".into(),
            description: "Get the current weather in a given location".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The city name, e.g. Tokyo, San Francisco"
                    }
                },
                "required": ["location"]
            }),
        }]),
    };

    let response = execute_llm_chat(ProviderType::OpenAi, &api_key, "gpt-4o-mini", request).await;

    match response {
        Ok(resp) => {
            println!("OpenAI tool call response: {:?}", resp);

            // Should return a tool call
            assert_eq!(resp.finish_reason, "tool_calls");
            assert!(resp.tool_calls.is_some());

            let tool_calls = resp.tool_calls.unwrap();
            assert!(!tool_calls.is_empty());

            let tool_call = &tool_calls[0];
            assert_eq!(tool_call.name, "get_weather");
            println!("Tool call arguments: {}", tool_call.arguments);

            // Step 2: Send tool result back
            let request2 = LlmChatRequest {
                messages: vec![
                    LlmMessage::user("What's the weather in Tokyo?"),
                    LlmMessage::assistant_with_tool_calls(vec![LlmToolCall {
                        id: tool_call.id.clone(),
                        name: tool_call.name.clone(),
                        arguments: tool_call.arguments.clone(),
                    }]),
                    LlmMessage::tool_result(
                        &tool_call.id,
                        r#"{"temperature": "22°C", "condition": "Sunny", "humidity": "45%"}"#,
                    ),
                ],
                system: Some("You are a helpful assistant.".into()),
                temperature: Some(0.0),
                max_tokens: Some(200),
                tools: None,
            };

            let response2 =
                execute_llm_chat(ProviderType::OpenAi, &api_key, "gpt-4o-mini", request2).await;

            match response2 {
                Ok(resp2) => {
                    println!("OpenAI final response: {:?}", resp2);
                    assert_eq!(resp2.finish_reason, "stop");
                    assert!(!resp2.content.is_empty());
                    // Should mention the weather info
                    let content_lower = resp2.content.to_lowercase();
                    assert!(
                        content_lower.contains("22") || content_lower.contains("sunny"),
                        "Response should contain weather info"
                    );
                }
                Err(e) => panic!("OpenAI second request failed: {:?}", e),
            }
        }
        Err(e) => panic!("OpenAI tool call failed: {:?}", e),
    }
}

#[tokio::test]
#[ignore]
async fn test_multi_turn_conversation() {
    let api_key = match get_env_key("OPENAI_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
    };

    // Turn 1
    let request1 = LlmChatRequest {
        messages: vec![LlmMessage::user("My name is Alice.")],
        system: Some("You are a helpful assistant. Remember the user's name.".into()),
        temperature: Some(0.0),
        max_tokens: Some(100),
        tools: None,
    };

    let resp1 = execute_llm_chat(ProviderType::OpenAi, &api_key, "gpt-4o-mini", request1)
        .await
        .expect("Turn 1 failed");

    println!("Turn 1 response: {}", resp1.content);

    // Turn 2 - should remember the name
    let request2 = LlmChatRequest {
        messages: vec![
            LlmMessage::user("My name is Alice."),
            LlmMessage::assistant(&resp1.content),
            LlmMessage::user("What is my name?"),
        ],
        system: Some("You are a helpful assistant. Remember the user's name.".into()),
        temperature: Some(0.0),
        max_tokens: Some(100),
        tools: None,
    };

    let resp2 = execute_llm_chat(ProviderType::OpenAi, &api_key, "gpt-4o-mini", request2)
        .await
        .expect("Turn 2 failed");

    println!("Turn 2 response: {}", resp2.content);
    assert!(
        resp2.content.to_lowercase().contains("alice"),
        "Should remember the name Alice"
    );
}
