//! Raw-HTTP on-device (local) LLM provider.
//!
//! Speaks the same OpenAI-compatible + `reasoning_content` dialect as
//! `stream_deepseek_raw` / `execute_deepseek_chat_raw` in `wasm::llm` — the
//! request body is built by `build_deepseek_request_body` so
//! `reasoning_content` rides the same assistant message as `tool_calls`,
//! which is what a thinking-mode local model expects on the next turn.
//! SSE parsing reuses `wasm::openai_sse`'s shared helpers.
//!
//! The endpoint (base URL, bearer token, model id, ...) comes from
//! `crate::local::endpoint::ensure`, which the engine supervisor (a later
//! task) publishes to once the local process is up. Every call here is also
//! gated by `crate::local::latch::egress_guard` — but one layer up, in
//! `wasm::llm::execute_llm_chat` / `execute_llm_stream_inner`, since that
//! guard applies to every provider, not just this one.

use crate::error::{DaemonError, Result};
use crate::local::endpoint::{self, LocalEndpoint};
use crate::wasm::llm::{
    build_deepseek_request_body, LlmChatRequest, LlmChatResponse, LlmStreamChunk, LlmToolCall,
};
use crate::wasm::openai_sse::{
    delta_events, usage_from, DeltaEvent, SseLineBuffer, ToolCallAccumulator,
};
use futures::StreamExt;
use nevoflux_protocol::json_repair::tool_arguments_or_marker;
use tokio::sync::mpsc;

/// Admission-control placeholder.
///
/// Task 2.10 replaces this body with
/// `crate::local::admission::acquire(Priority::from_context())`, queuing the
/// request behind the engine's parallel-slot budget. Until then every call
/// is admitted immediately — this is the single call site that task swaps.
async fn admission_hook() -> Result<()> {
    Ok(())
}

/// Build the request body sent to the on-device engine.
///
/// `build_deepseek_request_body` supplies the base shape (and the
/// reasoning_content/tool_calls co-location DeepSeek-style thinking models
/// need); this layers on two engine-specific fields:
/// - `stream_options.include_usage`, so the terminal SSE event carries
///   token usage (see `openai_sse::usage_from`) — only meaningful when
///   streaming, so only sent when `stream` is true.
/// - `chat_template_kwargs.enable_thinking: false`, for models whose chat
///   template supports both a thinking and a plain mode — without this the
///   engine defaults such models into thinking mode on every turn.
pub fn local_request_body(
    ep: &LocalEndpoint,
    request: &LlmChatRequest,
    stream: bool,
) -> serde_json::Value {
    let mut body = build_deepseek_request_body(&ep.model_id, request, stream);
    if stream {
        body["stream_options"] = serde_json::json!({"include_usage": true});
    }
    if ep.thinking_hybrid {
        body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
    }
    body
}

/// Non-streaming on-device chat completion.
pub async fn execute_local_chat(request: LlmChatRequest) -> Result<LlmChatResponse> {
    admission_hook().await?;
    let ep = endpoint::ensure()
        .await
        .map_err(DaemonError::InternalError)?;

    let body = local_request_body(&ep, &request, false);
    let url = format!("{}/chat/completions", ep.base_url.trim_end_matches('/'));

    tracing::debug!(base_url = %ep.base_url, model = %ep.model_id, "local engine chat POST");

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| DaemonError::InternalError(format!("Failed to build HTTP client: {e}")))?;

    let response = client
        .post(&url)
        .bearer_auth(&ep.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| DaemonError::InternalError(format!("Local engine request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(DaemonError::InternalError(format!(
            "Local engine HTTP {}: {}",
            status,
            &text[..text.len().min(500)]
        )));
    }

    let raw: serde_json::Value = response
        .json()
        .await
        .map_err(|e| DaemonError::InternalError(format!("Failed to parse response: {e}")))?;

    let choice = raw["choices"].get(0).ok_or_else(|| {
        DaemonError::InternalError("No choices in local engine response".to_string())
    })?;
    let message = &choice["message"];
    let content = message["content"].as_str().unwrap_or("").to_string();
    let finish_reason = choice["finish_reason"]
        .as_str()
        .unwrap_or("stop")
        .to_string();

    let tool_calls = message["tool_calls"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|tc| {
                let id = tc["id"].as_str()?.to_string();
                let function = tc.get("function")?;
                let name = function["name"].as_str()?.to_string();
                let args_raw = function["arguments"].as_str().unwrap_or("");
                let arguments = tool_arguments_or_marker(args_raw);
                Some(LlmToolCall {
                    id: id.clone(),
                    call_id: Some(id),
                    name,
                    arguments,
                    signature: None,
                })
            })
            .collect::<Vec<_>>()
    });

    Ok(LlmChatResponse {
        content,
        finish_reason,
        tool_calls,
        usage: usage_from(&raw),
        images: vec![],
    })
}

/// Streaming on-device chat completion.
///
/// Mirrors `wasm::llm::stream_deepseek_raw`'s SSE loop: buffer raw bytes
/// into complete `data:` lines, split each parsed delta into text/reasoning
/// events, accumulate fragmented tool-call deltas by index, and capture
/// usage off whichever event carries it (the local engine, like DeepSeek,
/// only populates `usage` on the terminal event, and only because
/// `local_request_body` sets `stream_options.include_usage`) — attached to
/// the final `done: true` chunk, per `LlmStreamChunk::usage`'s contract.
pub async fn stream_local(request: LlmChatRequest, tx: mpsc::Sender<LlmStreamChunk>) -> Result<()> {
    admission_hook().await?;
    let ep = endpoint::ensure()
        .await
        .map_err(DaemonError::InternalError)?;

    let body = local_request_body(&ep, &request, true);
    let url = format!("{}/chat/completions", ep.base_url.trim_end_matches('/'));

    tracing::debug!(base_url = %ep.base_url, model = %ep.model_id, "local engine stream POST");

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| DaemonError::InternalError(format!("Failed to build HTTP client: {e}")))?;

    let response = client
        .post(&url)
        .bearer_auth(&ep.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            DaemonError::InternalError(format!("Local engine stream request failed: {e}"))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(DaemonError::InternalError(format!(
            "Local engine stream HTTP {}: {}",
            status,
            &text[..text.len().min(500)]
        )));
    }

    let mut byte_stream = response.bytes_stream();
    let mut line_buf = SseLineBuffer::default();
    let mut accumulated_tool_calls = ToolCallAccumulator::default();
    let mut usage = None;

    while let Some(result) = byte_stream.next().await {
        match result {
            Ok(bytes) => {
                for data in line_buf.push(&bytes) {
                    let chunk: serde_json::Value = match serde_json::from_str(&data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // Usage may ride a chunk with an empty `choices` array
                    // (the terminal event), so this must run before the
                    // `choices[0]` lookup below rather than after it.
                    if let Some(u) = usage_from(&chunk) {
                        usage = Some(u);
                    }

                    let Some(choice) = chunk["choices"].get(0) else {
                        continue;
                    };
                    let delta = &choice["delta"];

                    for event in delta_events(delta) {
                        match event {
                            DeltaEvent::Text(text) => {
                                let _ = tx
                                    .send(LlmStreamChunk {
                                        usage: None,
                                        text: Some(text),
                                        tool_calls: vec![],
                                        done: false,
                                        reasoning: None,
                                        images: vec![],
                                    })
                                    .await;
                            }
                            DeltaEvent::Reasoning(reasoning) => {
                                let _ = tx
                                    .send(LlmStreamChunk {
                                        usage: None,
                                        text: None,
                                        tool_calls: vec![],
                                        done: false,
                                        reasoning: Some(reasoning),
                                        images: vec![],
                                    })
                                    .await;
                            }
                        }
                    }

                    accumulated_tool_calls.apply(delta);
                }
            }
            Err(e) => {
                tracing::warn!("Local engine stream chunk error: {}", e);
                break;
            }
        }
    }

    if !accumulated_tool_calls.is_empty() {
        let _ = tx
            .send(LlmStreamChunk {
                usage: None,
                text: None,
                tool_calls: accumulated_tool_calls.finish(),
                done: false,
                reasoning: None,
                images: vec![],
            })
            .await;
    }

    let _ = tx
        .send(LlmStreamChunk {
            usage,
            text: None,
            tool_calls: vec![],
            done: true,
            reasoning: None,
            images: vec![],
        })
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::llm::LlmMessage;

    /// Resets the global endpoint registry on drop, so a test's `publish`
    /// can't leak into a later test even if an assertion panics first.
    struct EndpointGuard;
    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            endpoint::publish(None);
        }
    }

    /// Resets the global LocalOnly latch on drop, for the same reason.
    struct LatchGuard;
    impl Drop for LatchGuard {
        fn drop(&mut self) {
            crate::local::latch::set(false);
        }
    }

    /// Find the first occurrence of `needle` in `haystack`.
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Minimal hand-rolled HTTP/1.1 SSE server for testing the raw-HTTP
    /// local provider without a mocking dependency.
    ///
    /// Accepts exactly one connection, reads the full request (headers +
    /// body, using `Content-Length` to know when the body is complete),
    /// writes `lines` back as `data: ...` SSE events (connection-close
    /// delimited — no `Content-Length`/`Transfer-Encoding` on the
    /// response, which is a legal HTTP/1.1 framing per RFC 7230 §3.3.3
    /// rule 7 and exactly how a real streaming server behaves), then closes
    /// the socket.
    ///
    /// Returns the endpoint URL and a receiver for the captured request
    /// text, lower-cased — so a caller doesn't have to guess whether the
    /// HTTP stack sent `Authorization: Bearer …` or `authorization: Bearer
    /// …` on the wire.
    async fn fake_sse_server(lines: Vec<&str>) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (req_tx, req_rx) = tokio::sync::oneshot::channel();
        let body: String = lines.into_iter().map(|l| format!("{l}\n\n")).collect();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end = None;
            let mut content_length = 0usize;
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        let header_text = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                        content_length = header_text
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                    }
                }
                if let Some(he) = header_end {
                    if buf.len() >= he + content_length {
                        break;
                    }
                }
            }

            let _ = req_tx.send(String::from_utf8_lossy(&buf).to_ascii_lowercase());

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        (format!("http://{addr}"), req_rx)
    }

    #[tokio::test]
    async fn stream_local_sends_bearer_and_parses_text_reasoning_and_tools() {
        let _g = crate::local::latch::test_serial();
        let _reset = EndpointGuard;

        let (url, captured) = fake_sse_server(vec![
            r#"data: {"choices":[{"delta":{"reasoning_content":"r"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"he"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"llo"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"read","arguments":"{\"path\":\"x\"}"}}]}}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
            "data: [DONE]",
        ])
        .await;
        endpoint::publish(Some(LocalEndpoint {
            base_url: url,
            api_key: "k".into(),
            n_ctx: 16384,
            model_id: "m".into(),
            thinking_hybrid: true,
        }));

        let (tx, mut rx) = mpsc::channel(32);
        stream_local(
            LlmChatRequest {
                messages: vec![LlmMessage::user("hi")],
                ..Default::default()
            },
            tx,
        )
        .await
        .unwrap();

        let mut text = String::new();
        let mut tools = vec![];
        let mut reasoning = String::new();
        let mut usage = None;
        while let Some(c) = rx.recv().await {
            if let Some(t) = c.text {
                text += &t
            }
            if let Some(r) = c.reasoning {
                reasoning += &r
            }
            tools.extend(c.tool_calls);
            if c.usage.is_some() {
                usage = c.usage
            }
        }
        assert_eq!(
            (text.as_str(), reasoning.as_str(), tools[0].name.as_str()),
            ("hello", "r", "read")
        );
        assert_eq!(usage.unwrap().total_tokens, 7);

        let req = captured.await.unwrap();
        assert!(req.contains("authorization: bearer k"));
        assert!(req.contains("\"enable_thinking\":false"));
    }

    #[tokio::test]
    async fn cloud_call_is_refused_while_latched() {
        let _g = crate::local::latch::test_serial();
        let _reset = LatchGuard;
        crate::local::latch::set(true);
        let r = crate::wasm::llm::execute_llm_chat(
            nevoflux_llm::ProviderType::Anthropic,
            "k",
            "m",
            LlmChatRequest::default(),
            None,
        )
        .await;
        assert!(matches!(
            r,
            Err(crate::error::DaemonError::PermissionDenied(_))
        ));
    }
}
