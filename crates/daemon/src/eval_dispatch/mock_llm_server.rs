//! Tiny local HTTP server returning canned OpenAI + Anthropic chat
//! completion responses.
//!
//! Activated by env var `NEVOFLUX_EVAL_LLM_MODE=mock`. The daemon's
//! `HostServices.llm_config.base_url` is rewritten to point here during
//! eval-mode boot (see `src/main.rs` Task 6 wiring).
//!
//! For Phase 3a, the server returns a single canned assistant message
//! ("Eval mock response.") with appropriate stop_reason for every request.
//! Phase 3c extends with a response queue: callers enqueue per-call replies
//! via `enqueue_response`; the canned message is the fallback when empty.

#![cfg(feature = "eval-mock-llm")]

use axum::{routing::post, Json, Router};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Mutex;
use tokio::net::TcpListener;

/// Response queue. Tests / setup code calls `enqueue_response` to stage a
/// specific reply; the mock handler pops one per request. When empty, the
/// handler falls back to the canned `"Eval mock response."` string.
static RESPONSE_QUEUE: Lazy<Mutex<VecDeque<String>>> = Lazy::new(|| Mutex::new(VecDeque::new()));

/// Enqueue a response string. The next call to the mock server will return it.
/// Test helper; not used in production code paths.
pub fn enqueue_response(reply: impl Into<String>) {
    RESPONSE_QUEUE.lock().unwrap().push_back(reply.into());
}

/// Reset the queue (drains all pending). Use between tests.
pub fn reset_queue() {
    RESPONSE_QUEUE.lock().unwrap().clear();
}

fn pop_response_or_default() -> String {
    RESPONSE_QUEUE
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| "Eval mock response.".to_string())
}

/// Spawn the mock LLM server on 127.0.0.1:0 (OS-assigned port). Returns the
/// bound address. The server runs for the lifetime of the daemon process —
/// no shutdown handle is exposed (eval mode is short-lived).
pub async fn spawn() -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "mock LLM server stopped");
        }
    });
    tracing::info!(%addr, "eval mock LLM server listening");
    Ok(addr)
}

pub fn is_enabled() -> bool {
    std::env::var("NEVOFLUX_EVAL_LLM_MODE").as_deref() == Ok("mock")
}

// --- OpenAI chat.completion wire format ---

#[derive(Debug, Deserialize)]
struct ChatRequest {
    #[allow(dead_code)]
    model: String,
    #[allow(dead_code)]
    messages: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    id: String,
    object: &'static str,
    created: i64,
    model: String,
    choices: Vec<Choice>,
    usage: Usage,
}

#[derive(Debug, Serialize)]
struct Choice {
    index: u32,
    message: AssistantMessage,
    finish_reason: &'static str,
}

#[derive(Debug, Serialize)]
struct AssistantMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Serialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

async fn chat_completions(Json(req): Json<ChatRequest>) -> Json<ChatResponse> {
    let content = pop_response_or_default();
    Json(ChatResponse {
        id: "chatcmpl-mock-eval".into(),
        object: "chat.completion",
        created: chrono::Utc::now().timestamp(),
        model: req.model,
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content,
            },
            finish_reason: "stop",
        }],
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        },
    })
}

// --- Anthropic messages wire format ---

#[derive(Debug, Serialize)]
struct AnthropicResponse {
    id: String,
    #[serde(rename = "type")]
    ty: &'static str,
    role: &'static str,
    content: Vec<AnthropicContent>,
    model: String,
    stop_reason: &'static str,
    stop_sequence: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Serialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    ty: &'static str,
    text: String,
}

#[derive(Debug, Serialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct AnthropicRequest {
    model: String,
    #[allow(dead_code)]
    messages: serde_json::Value,
    #[allow(dead_code)]
    #[serde(default)]
    system: Option<serde_json::Value>,
    #[allow(dead_code)]
    #[serde(default)]
    max_tokens: Option<u32>,
}

async fn anthropic_messages(Json(req): Json<AnthropicRequest>) -> Json<AnthropicResponse> {
    let text = pop_response_or_default();
    Json(AnthropicResponse {
        id: "msg_mock_eval".into(),
        ty: "message",
        role: "assistant",
        content: vec![AnthropicContent {
            ty: "text",
            text,
        }],
        model: req.model,
        stop_reason: "end_turn",
        stop_sequence: None,
        usage: AnthropicUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_returns_loopback_addr() {
        reset_queue();
        let addr = spawn().await.unwrap();
        assert!(addr.ip().is_loopback());
        assert!(addr.port() > 0);
    }

    #[tokio::test]
    async fn openai_endpoint_returns_canned_response() {
        reset_queue();
        let addr = spawn().await.unwrap();
        let url = format!("http://{addr}/v1/chat/completions");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({
                "model": "gpt-4o-mini",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "Eval mock response."
        );
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn anthropic_endpoint_returns_canned_response() {
        reset_queue();
        let addr = spawn().await.unwrap();
        let url = format!("http://{addr}/v1/messages");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({
                "model": "claude-3-5-sonnet-latest",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 100
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["content"][0]["text"], "Eval mock response.");
        assert_eq!(body["stop_reason"], "end_turn");
    }

    #[tokio::test]
    async fn enqueued_response_overrides_default() {
        reset_queue();
        enqueue_response("Custom test reply.");

        let addr = spawn().await.unwrap();
        let url = format!("http://{addr}/v1/chat/completions");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({
                "model": "gpt-4o-mini",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "Custom test reply.");

        // Next call falls back to canned default.
        let resp2 = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({
                "model": "gpt-4o-mini",
                "messages": [{"role": "user", "content": "again"}]
            }))
            .send()
            .await
            .unwrap();
        let body2: serde_json::Value = resp2.json().await.unwrap();
        assert_eq!(body2["choices"][0]["message"]["content"], "Eval mock response.");
    }
}
