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
    execute_llm_chat, start_llm_stream, LlmChatRequest, LlmMessage, LlmStreamRegistry,
    LlmToolCall, LlmToolDefinition,
};
use nevoflux_llm::ProviderType;
use serde_json::json;
use std::sync::Arc;

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
            // Use call_id for tool results if available (required by OpenAI Responses API)
            let tool_result_id = tool_call.call_id.as_ref().unwrap_or(&tool_call.id);
            let request2 = LlmChatRequest {
                messages: vec![
                    LlmMessage::user("What's the weather in Tokyo?"),
                    LlmMessage::assistant_with_tool_calls(vec![LlmToolCall {
                        id: tool_call.id.clone(),
                        call_id: tool_call.call_id.clone(),
                        name: tool_call.name.clone(),
                        arguments: tool_call.arguments.clone(),
                    }]),
                    LlmMessage::tool_result(
                        tool_result_id,
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

#[tokio::test]
#[ignore]
async fn test_openai_streaming_tool_calling() {
    let api_key = match get_env_key("OPENAI_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
    };

    let registry = Arc::new(LlmStreamRegistry::new());

    // Step 1: Send streaming request with tool definition
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

    let stream_id = start_llm_stream(
        ProviderType::OpenAi,
        &api_key,
        "gpt-4o-mini",
        request,
        registry.clone(),
    )
    .await
    .expect("Failed to start stream");

    println!("Stream started with ID: {}", stream_id);

    // Collect all tool calls from streaming chunks
    let mut all_tool_calls = Vec::new();
    let mut chunk_count = 0;

    loop {
        // Small delay to allow chunks to arrive
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        match registry.next_chunk(stream_id) {
            Ok(Some(chunk)) => {
                chunk_count += 1;
                println!(
                    "Chunk {}: text={:?}, tool_calls={}, done={}",
                    chunk_count,
                    chunk.text,
                    chunk.tool_calls.len(),
                    chunk.done
                );

                for tc in &chunk.tool_calls {
                    println!(
                        "  Tool call: id={}, call_id={:?}, name={}",
                        tc.id, tc.call_id, tc.name
                    );
                }

                all_tool_calls.extend(chunk.tool_calls);

                if chunk.done {
                    break;
                }
            }
            Ok(None) => {
                // No chunk available yet, continue waiting
                continue;
            }
            Err(e) => {
                panic!("Stream error: {:?}", e);
            }
        }
    }

    registry.close(stream_id);

    println!("\n=== Summary ===");
    println!("Total chunks received: {}", chunk_count);
    println!("Total tool calls: {}", all_tool_calls.len());

    // Verify we got tool calls
    assert!(!all_tool_calls.is_empty(), "Should have received tool calls");

    // Find the get_weather tool call
    let weather_call = all_tool_calls
        .iter()
        .find(|tc| tc.name == "get_weather")
        .expect("Should have get_weather tool call");

    println!("\nWeather tool call:");
    println!("  id: {}", weather_call.id);
    println!("  call_id: {:?}", weather_call.call_id);
    println!("  name: {}", weather_call.name);
    println!("  arguments: {}", weather_call.arguments);

    // CRITICAL: Verify call_id is present (this is what caused the original bug)
    assert!(
        weather_call.call_id.is_some(),
        "call_id MUST be present for OpenAI Responses API! Got id={}, call_id={:?}",
        weather_call.id,
        weather_call.call_id
    );

    let call_id = weather_call.call_id.as_ref().unwrap();
    assert!(
        call_id.starts_with("call_"),
        "call_id should start with 'call_', got: {}",
        call_id
    );

    // Step 2: Send tool result back using call_id
    let request2 = LlmChatRequest {
        messages: vec![
            LlmMessage::user("What's the weather in Tokyo?"),
            LlmMessage::assistant_with_tool_calls(vec![LlmToolCall {
                id: weather_call.id.clone(),
                call_id: weather_call.call_id.clone(),
                name: weather_call.name.clone(),
                arguments: weather_call.arguments.clone(),
            }]),
            LlmMessage::tool_result(
                call_id, // Use call_id, not id!
                r#"{"temperature": "22°C", "condition": "Sunny", "humidity": "45%"}"#,
            ),
        ],
        system: Some("You are a helpful assistant.".into()),
        temperature: Some(0.0),
        max_tokens: Some(200),
        tools: None,
    };

    // Use non-streaming for the follow-up to verify it works
    let response2 = execute_llm_chat(ProviderType::OpenAi, &api_key, "gpt-4o-mini", request2)
        .await
        .expect("Second request with tool result failed");

    println!("\nFinal response: {}", response2.content);
    assert_eq!(response2.finish_reason, "stop");
    assert!(!response2.content.is_empty());

    // Should mention the weather info
    let content_lower = response2.content.to_lowercase();
    assert!(
        content_lower.contains("22") || content_lower.contains("sunny"),
        "Response should contain weather info"
    );

    println!("\n✅ Streaming tool call test passed!");
}

#[tokio::test]
#[ignore]
async fn test_openai_streaming_tool_calling_full_streaming() {
    // This test uses streaming for BOTH requests (first and follow-up),
    // which matches the real agent scenario.
    let api_key = match get_env_key("OPENAI_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
    };

    let registry = Arc::new(LlmStreamRegistry::new());

    // Step 1: Send streaming request with tool definition
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

    let stream_id = start_llm_stream(
        ProviderType::OpenAi,
        &api_key,
        "gpt-4o-mini",
        request,
        registry.clone(),
    )
    .await
    .expect("Failed to start stream");

    // Collect tool call from first stream
    let mut weather_call = None;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        match registry.next_chunk(stream_id) {
            Ok(Some(chunk)) => {
                for tc in &chunk.tool_calls {
                    if tc.name == "get_weather" {
                        weather_call = Some(tc.clone());
                    }
                }
                if chunk.done {
                    break;
                }
            }
            Ok(None) => continue,
            Err(e) => panic!("Stream error: {:?}", e),
        }
    }
    registry.close(stream_id);

    let weather_call = weather_call.expect("Should have get_weather tool call");
    let call_id = weather_call
        .call_id
        .as_ref()
        .expect("call_id must be present");

    println!("First stream got tool call: id={}, call_id={}", weather_call.id, call_id);

    // Step 2: Send tool result back using STREAMING (this is the key difference!)
    let request2 = LlmChatRequest {
        messages: vec![
            LlmMessage::user("What's the weather in Tokyo?"),
            LlmMessage::assistant_with_tool_calls(vec![LlmToolCall {
                id: weather_call.id.clone(),
                call_id: weather_call.call_id.clone(),
                name: weather_call.name.clone(),
                arguments: weather_call.arguments.clone(),
            }]),
            LlmMessage::tool_result(
                call_id,
                r#"{"temperature": "22°C", "condition": "Sunny", "humidity": "45%"}"#,
            ),
        ],
        system: Some("You are a helpful assistant.".into()),
        temperature: Some(0.0),
        max_tokens: Some(200),
        tools: None,
    };

    // Use STREAMING for the follow-up (this is what the real agent does)
    let stream_id2 = start_llm_stream(
        ProviderType::OpenAi,
        &api_key,
        "gpt-4o-mini",
        request2,
        registry.clone(),
    )
    .await
    .expect("Failed to start second stream");

    println!("Second stream started with ID: {}", stream_id2);

    // Collect response from second stream
    let mut combined_text = String::new();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        match registry.next_chunk(stream_id2) {
            Ok(Some(chunk)) => {
                if let Some(text) = &chunk.text {
                    combined_text.push_str(text);
                }
                if chunk.done {
                    break;
                }
            }
            Ok(None) => continue,
            Err(e) => panic!("Second stream error: {:?}", e),
        }
    }
    registry.close(stream_id2);

    println!("\nFinal response (via streaming): {}", combined_text);
    assert!(!combined_text.is_empty(), "Response should not be empty");

    let content_lower = combined_text.to_lowercase();
    assert!(
        content_lower.contains("22") || content_lower.contains("sunny"),
        "Response should contain weather info"
    );

    println!("\n✅ Full streaming tool call test passed!");
}

/// Helper struct to track tool calls and build messages for multi-round tests
struct ToolCallTracker {
    messages: Vec<LlmMessage>,
    tools: Vec<LlmToolDefinition>,
}

impl ToolCallTracker {
    fn new(initial_user_message: &str, system: Option<&str>, tools: Vec<LlmToolDefinition>) -> Self {
        let mut messages = Vec::new();
        if let Some(sys) = system {
            messages.push(LlmMessage {
                role: "system".into(),
                content: sys.into(),
                tool_calls: None,
                tool_call_id: None,
                attachments: vec![],
            });
        }
        messages.push(LlmMessage::user(initial_user_message));
        Self { messages, tools }
    }

    fn add_assistant_with_tool_calls(&mut self, tool_calls: Vec<LlmToolCall>) {
        self.messages
            .push(LlmMessage::assistant_with_tool_calls(tool_calls));
    }

    fn add_tool_result(&mut self, tool_call_id: &str, content: &str) {
        self.messages
            .push(LlmMessage::tool_result(tool_call_id, content));
    }

    fn build_request(&self) -> LlmChatRequest {
        LlmChatRequest {
            messages: self.messages.clone(),
            system: None, // System is included in messages
            temperature: Some(0.0),
            max_tokens: Some(300),
            tools: Some(self.tools.clone()),
        }
    }

    fn build_request_no_tools(&self) -> LlmChatRequest {
        LlmChatRequest {
            messages: self.messages.clone(),
            system: None,
            temperature: Some(0.0),
            max_tokens: Some(300),
            tools: None,
        }
    }
}

/// Collect all chunks from a stream until done, returning (text, tool_calls)
async fn collect_stream(
    registry: &LlmStreamRegistry,
    stream_id: u64,
) -> (String, Vec<LlmToolCall>) {
    let mut combined_text = String::new();
    let mut all_tool_calls = Vec::new();

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        match registry.next_chunk(stream_id) {
            Ok(Some(chunk)) => {
                if let Some(text) = &chunk.text {
                    combined_text.push_str(text);
                }
                all_tool_calls.extend(chunk.tool_calls);
                if chunk.done {
                    break;
                }
            }
            Ok(None) => continue,
            Err(e) => panic!("Stream error: {:?}", e),
        }
    }
    registry.close(stream_id);

    (combined_text, all_tool_calls)
}

#[tokio::test]
#[ignore]
async fn test_multi_round_tool_calling() {
    // This test simulates the real agent loop:
    // User question → Tool Call 1 → Result 1 → Tool Call 2 → Result 2 → Final Response
    //
    // Scenario: User asks about weather in two cities
    // Round 1: LLM calls get_weather for first city
    // Round 2: LLM calls get_weather for second city
    // Round 3: LLM synthesizes both results into final answer

    let api_key = match get_env_key("OPENAI_API_KEY") {
        Some(key) => key,
        None => {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
    };

    let registry = Arc::new(LlmStreamRegistry::new());

    let tools = vec![LlmToolDefinition {
        name: "get_weather".into(),
        description: "Get the current weather in a given location. Call this once per city."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "The city name"
                }
            },
            "required": ["location"]
        }),
    }];

    let mut tracker = ToolCallTracker::new(
        "What's the weather in Tokyo and Paris? Use the get_weather tool for each city separately.",
        Some("You are a helpful assistant. When asked about weather in multiple cities, call the get_weather tool once for each city. Do not try to get weather for multiple cities in a single call."),
        tools,
    );

    let mut round = 0;
    const MAX_ROUNDS: usize = 5;

    loop {
        round += 1;
        if round > MAX_ROUNDS {
            panic!("Too many rounds, possible infinite loop");
        }

        println!("\n=== Round {} (messages: {}) ===", round, tracker.messages.len());

        // Build request (include tools unless we've done multiple rounds)
        let request = if round <= 3 {
            tracker.build_request()
        } else {
            tracker.build_request_no_tools()
        };

        // Start streaming request
        let stream_id = start_llm_stream(
            ProviderType::OpenAi,
            &api_key,
            "gpt-4o-mini",
            request,
            registry.clone(),
        )
        .await
        .expect("Failed to start stream");

        let (text, tool_calls) = collect_stream(&registry, stream_id).await;

        println!("Response text: {}", if text.is_empty() { "(empty)" } else { &text });
        println!("Tool calls: {}", tool_calls.len());

        if tool_calls.is_empty() {
            // No more tool calls, we have the final response
            println!("\n=== Final Response ===");
            println!("{}", text);

            // Verify the response mentions both cities
            let text_lower = text.to_lowercase();
            assert!(
                text_lower.contains("tokyo") || text_lower.contains("東京"),
                "Response should mention Tokyo"
            );
            assert!(
                text_lower.contains("paris") || text_lower.contains("巴黎"),
                "Response should mention Paris"
            );

            println!("\n✅ Multi-round tool calling test passed! (completed in {} rounds)", round);
            return;
        }

        // When model returns multiple tool calls, we need to:
        // 1. Add ONE assistant message with ALL tool calls
        // 2. Add ONE tool result for EACH tool call
        tracker.add_assistant_with_tool_calls(tool_calls.clone());

        // Process each tool call and add results
        for tc in &tool_calls {
            println!(
                "  Tool call: {} (id={}, call_id={:?})",
                tc.name, tc.id, tc.call_id
            );

            let call_id = tc.call_id.as_ref().unwrap_or(&tc.id);
            let location = tc.arguments["location"].as_str().unwrap_or("unknown");

            // Simulate tool execution with fake weather data
            let weather_result = match location.to_lowercase().as_str() {
                l if l.contains("tokyo") => {
                    r#"{"temperature": "18°C", "condition": "Cloudy", "humidity": "60%"}"#
                }
                l if l.contains("paris") => {
                    r#"{"temperature": "12°C", "condition": "Rainy", "humidity": "80%"}"#
                }
                _ => r#"{"temperature": "20°C", "condition": "Unknown", "humidity": "50%"}"#,
            };

            tracker.add_tool_result(call_id, weather_result);
        }
    }
}
