//! ACP-backed chat upstream for the gateway.
//!
//! Serves OpenAI-compatible `/v1/chat/completions` by driving a Claude
//! Code **ACP** session instead of HTTP-proxying to Anthropic. The
//! gateway holds its own headless, tool-less `AcpProvider`: a single
//! `claude-agent-acp` subprocess (lazy-spawned on the first `Acp` chat
//! request) and a NEW ACP session per request.
//!
//! Translation/aggregation is factored into pure functions so it can be
//! unit-tested without spawning claude (feed a constructed `Vec<AcpUpdate>`).

use nevoflux_llm::providers::acp::{AcpUpdate, ContentBlock, StopReason, TextContent};
use serde_json::Value;

use crate::translate::{
    OpenAIChatCompletion, OpenAIChatRequest, OpenAIChoice, OpenAIRespMessage, OpenAIUsage,
};

/// Coerce one OpenAI message `content` (string, array of parts, or null)
/// into a single plain string. Mirrors `translate::openai_content_to_string`
/// but kept local so the ACP path is self-contained (spec section 4).
fn content_to_string(content: &Option<Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                    out.push_str(text);
                } else if let Some(s) = p.as_str() {
                    out.push_str(s);
                }
            }
            out
        }
        Some(other) => other.to_string(),
    }
}

/// Flatten an OpenAI multi-turn `messages[]` into a single prompt string.
///
/// Layout: every `system` message first (joined), then each remaining
/// turn rendered with a `Role:` label, blank-line separated. The
/// request's `tools` / `tool_choice` are intentionally ignored
/// (tool-less plain-text completion; spec section 6).
pub fn flatten_messages(req: &OpenAIChatRequest) -> String {
    let mut system_parts: Vec<String> = Vec::new();
    let mut turns: Vec<String> = Vec::new();

    for msg in &req.messages {
        let text = content_to_string(&msg.content);
        match msg.role.as_str() {
            "system" => {
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            "assistant" => turns.push(format!("Assistant: {text}")),
            "user" => turns.push(format!("User: {text}")),
            other => turns.push(format!("{other}: {text}")),
        }
    }

    let mut out = String::new();
    if !system_parts.is_empty() {
        out.push_str(&system_parts.join("\n\n"));
    }
    if !turns.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&turns.join("\n\n"));
    }
    out
}

/// Build the single-block prompt content for an ACP `prompt()` call.
pub fn prompt_content(prompt: String) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(prompt))]
}

/// Map an ACP [`StopReason`] to an OpenAI `finish_reason`.
///
/// `StopReason` is `#[non_exhaustive]`, so the wildcard arm is required
/// and future variants degrade to `"stop"`.
pub fn stop_reason_to_finish_reason(stop: StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "stop",
        StopReason::MaxTokens => "length",
        StopReason::Refusal => "content_filter",
        StopReason::Cancelled => "stop",
        StopReason::MaxTurnRequests => "stop",
        _ => "stop",
    }
}

/// Outcome of aggregating an ACP update stream.
pub enum Aggregated {
    /// Assistant text + the finish reason derived from the stop reason.
    Done {
        content: String,
        finish_reason: &'static str,
    },
    /// An `AcpUpdate::Error(_)` was seen, or the stream ended without a
    /// `Complete`. Carries the error message for a 502 body.
    Failed(String),
}

/// Drain an ACP update stream into a single assistant string.
///
/// `Text` chunks concatenate; `Thought` is ignored; `Error` short-circuits
/// to [`Aggregated::Failed`]; `Complete(stop)` finishes. A stream that ends
/// without `Complete` and without `Error` is treated as `Failed`.
pub fn aggregate_updates(updates: impl IntoIterator<Item = AcpUpdate>) -> Aggregated {
    let mut content = String::new();
    for update in updates {
        match update {
            AcpUpdate::Text(t) => content.push_str(&t),
            AcpUpdate::Thought(_) => {}
            // No OpenAI-compat analog for a native tool-call result; the
            // daemon's `/loop`/`/goal` machinery records these separately
            // (see `crates/daemon/src/wasm/llm.rs`). Gateway responses only
            // ever surface assistant text.
            AcpUpdate::ToolResult { .. } => {}
            AcpUpdate::Error(e) => return Aggregated::Failed(e),
            AcpUpdate::Complete(stop) => {
                return Aggregated::Done {
                    content,
                    finish_reason: stop_reason_to_finish_reason(stop),
                };
            }
        }
    }
    Aggregated::Failed("ACP stream ended without completion".to_string())
}

/// Build the non-stream OpenAI completion body from an aggregated result.
///
/// `model` echoes the client's request model (ACP always uses claude).
/// Usage is zeroed (ACP does not report token counts; spec section 9).
pub fn build_completion(
    model: String,
    content: String,
    finish_reason: &str,
) -> OpenAIChatCompletion {
    use std::time::{SystemTime, UNIX_EPOCH};
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    OpenAIChatCompletion {
        id: format!("chatcmpl-acp-{created}"),
        object: "chat.completion",
        created,
        model,
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIRespMessage {
                role: "assistant".to_string(),
                content: Some(content),
                tool_calls: None,
            },
            finish_reason: finish_reason.to_string(),
        }],
        usage: OpenAIUsage::default(),
    }
}

use nevoflux_llm::providers::acp::{AcpProvider, AcpProviderConfig};

/// Gateway-owned ACP holder: a single lazily-connected `AcpProvider`.
///
/// Guarded by an outer `tokio::sync::Mutex` in [`crate::handlers::AppState`]
/// so concurrent first-requests connect exactly once. A failed connect
/// leaves `provider == None` so the next request retries the spawn
/// (the Mutex is not poisoned).
pub struct AcpUpstream {
    config: AcpProviderConfig,
    provider: Option<AcpProvider>,
}

impl AcpUpstream {
    pub fn new(config: AcpProviderConfig) -> Self {
        Self {
            config,
            provider: None,
        }
    }

    /// Ensure the subprocess is connected, spawning it on first use.
    /// Returns a shared reference to the live provider.
    pub async fn ensure_connected(&mut self) -> Result<&AcpProvider, String> {
        let needs_connect = match &self.provider {
            None => true,
            Some(p) => !p.is_alive(),
        };
        if needs_connect {
            let mut provider = AcpProvider::new(self.config.clone());
            provider
                .connect()
                .await
                .map_err(|e| format!("ACP connect failed: {e}"))?;
            self.provider = Some(provider);
        }
        Ok(self.provider.as_ref().expect("provider set above"))
    }
}

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    Json,
};

use crate::error::GatewayError;

/// Drive one chat request through the gateway's ACP session.
///
/// Lazy-connects the subprocess (guarded by the caller's `Mutex`), opens
/// a NEW session, sends the flattened prompt, drains the update stream,
/// and returns an OpenAI completion (non-stream) -- see the streaming
/// variant for `stream: true`.
pub async fn do_chat_completions_acp(
    acp: Arc<tokio::sync::Mutex<AcpUpstream>>,
    req: OpenAIChatRequest,
    req_idx: u64,
    stream: bool,
) -> Result<Response, GatewayError> {
    let model = req.model.clone();
    let prompt = flatten_messages(&req);

    // Lazy connect + new session + prompt, all under the holder lock so
    // the subprocess spawns exactly once. We collect the receiver while
    // holding the lock (cheap: prompt() just enqueues a request and
    // returns the mpsc Receiver), then release before draining.
    let mut receiver = {
        let mut guard = acp.lock().await;
        let provider = guard
            .ensure_connected()
            .await
            .map_err(|detail| GatewayError::UpstreamUnreachable { detail })?;
        let session =
            provider
                .new_session()
                .await
                .map_err(|e| GatewayError::UpstreamUnreachable {
                    detail: format!("ACP new_session failed: {e}"),
                })?;
        provider
            .prompt(session, prompt_content(prompt))
            .await
            .map_err(|e| GatewayError::UpstreamUnreachable {
                detail: format!("ACP prompt failed: {e}"),
            })?
    };

    tracing::info!(req_idx, stream, upstream = "acp", "chat_completions -> ACP");

    if stream {
        return Ok(stream_response(receiver, model, req_idx));
    }

    // Non-stream: drain all updates, aggregate, build the completion.
    let mut updates = Vec::new();
    while let Some(u) = receiver.recv().await {
        updates.push(u);
    }
    match aggregate_updates(updates) {
        Aggregated::Done {
            content,
            finish_reason,
        } => {
            tracing::info!(req_idx, "chat_completions ok (non-stream, acp)");
            Ok(Json(build_completion(model, content, finish_reason)).into_response())
        }
        Aggregated::Failed(detail) => Err(GatewayError::UpstreamServerError {
            upstream_status: 502,
            upstream_body: detail,
        }),
    }
}

/// Build the streaming SSE response from an ACP update receiver.
///
/// Emits an `OpenAIChatChunk` per `AcpUpdate::Text` (first chunk also
/// sets `delta.role = "assistant"`); ignores `Thought`; on `Complete`
/// emits a final chunk carrying `finish_reason` then `[DONE]`; on `Error`
/// (or an early-closed receiver) emits an OpenAI-shaped error chunk +
/// `[DONE]`. Reuses the same `Sse` + keep-alive contract as the
/// Anthropic/OpenAI paths in `handlers.rs`.
fn stream_response(
    mut receiver: tokio::sync::mpsc::Receiver<AcpUpdate>,
    model: String,
    req_idx: u64,
) -> Response {
    use crate::translate::{OpenAIChatChunk, OpenAIChunkChoice, OpenAIDelta};
    use std::time::{SystemTime, UNIX_EPOCH};

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let id = format!("chatcmpl-acp-{created}");

    let sse = async_stream::stream! {
        let mut role_emitted = false;

        let make = |delta: OpenAIDelta, finish: Option<String>| -> OpenAIChatChunk {
            OpenAIChatChunk {
                id: id.clone(),
                object: "chat.completion.chunk",
                created,
                model: model.clone(),
                choices: vec![OpenAIChunkChoice { index: 0, delta, finish_reason: finish }],
            }
        };
        let emit = |chunk: OpenAIChatChunk| -> Result<Event, Infallible> {
            match serde_json::to_string(&chunk) {
                Ok(s) => Ok(Event::default().data(s)),
                Err(e) => {
                    tracing::error!(req_idx, error = %e, "failed to encode acp chunk");
                    Ok(Event::default().data("{}"))
                }
            }
        };

        loop {
            match receiver.recv().await {
                Some(AcpUpdate::Text(text)) => {
                    if text.is_empty() {
                        continue;
                    }
                    let delta = OpenAIDelta {
                        role: if !role_emitted { Some("assistant".to_string()) } else { None },
                        content: Some(text),
                        tool_calls: None,
                    };
                    role_emitted = true;
                    yield emit(make(delta, None));
                }
                Some(AcpUpdate::Thought(_)) => {}
                // See the comment in `aggregate_updates` above — nothing to
                // surface over the OpenAI-compat SSE stream.
                Some(AcpUpdate::ToolResult { .. }) => {}
                Some(AcpUpdate::Complete(stop)) => {
                    let fr = stop_reason_to_finish_reason(stop).to_string();
                    yield emit(make(OpenAIDelta::default(), Some(fr)));
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
                Some(AcpUpdate::Error(e)) => {
                    tracing::error!(req_idx, error = %e, "acp stream error mid-flight");
                    let err = serde_json::json!({
                        "error": { "type": "acp_stream_error", "message": e }
                    });
                    yield Ok(Event::default().data(err.to_string()));
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
                None => {
                    // Receiver closed without an explicit Complete/Error.
                    tracing::warn!(req_idx, "acp stream ended without completion");
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
            }
        }
    };

    Sse::new(sse).keep_alive(KeepAlive::new()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::{OpenAIChatRequest, OpenAIMessage};
    use std::collections::BTreeMap;

    fn msg(role: &str, content: &str) -> OpenAIMessage {
        OpenAIMessage {
            role: role.to_string(),
            content: Some(Value::String(content.to_string())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn req_with(messages: Vec<OpenAIMessage>) -> OpenAIChatRequest {
        OpenAIChatRequest {
            model: "claude-anything".to_string(),
            messages,
            max_tokens: None,
            temperature: None,
            top_p: None,
            stream: None,
            tools: None,
            tool_choice: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn flatten_system_then_turns() {
        let req = req_with(vec![
            msg("system", "You are terse."),
            msg("user", "Hello"),
            msg("assistant", "Hi"),
            msg("user", "Bye"),
        ]);
        let out = flatten_messages(&req);
        assert_eq!(
            out,
            "You are terse.\n\nUser: Hello\n\nAssistant: Hi\n\nUser: Bye"
        );
    }

    #[test]
    fn flatten_no_system_just_user() {
        let req = req_with(vec![msg("user", "Just this")]);
        assert_eq!(flatten_messages(&req), "User: Just this");
    }

    #[test]
    fn flatten_array_content_parts() {
        let mut m = msg("user", "");
        m.content = Some(serde_json::json!([
            { "type": "text", "text": "part-a " },
            { "type": "text", "text": "part-b" }
        ]));
        let req = req_with(vec![m]);
        assert_eq!(flatten_messages(&req), "User: part-a part-b");
    }

    #[test]
    fn flatten_empty_messages_is_empty() {
        let req = req_with(vec![]);
        assert_eq!(flatten_messages(&req), "");
    }

    #[test]
    fn tools_are_ignored_no_panic() {
        // A request carrying tools must flatten exactly as if tools were
        // absent (tool-less completion; spec section 6).
        use crate::translate::{OpenAIFunctionDef, OpenAITool};
        let mut req = req_with(vec![msg("user", "Q")]);
        req.tools = Some(vec![OpenAITool {
            kind: "function".to_string(),
            function: OpenAIFunctionDef {
                name: "do_thing".to_string(),
                description: Some("desc".to_string()),
                parameters: Some(serde_json::json!({"type": "object"})),
            },
        }]);
        req.tool_choice = Some(serde_json::json!("auto"));
        assert_eq!(flatten_messages(&req), "User: Q");
    }

    #[test]
    fn stop_reason_mapping_table() {
        assert_eq!(stop_reason_to_finish_reason(StopReason::EndTurn), "stop");
        assert_eq!(
            stop_reason_to_finish_reason(StopReason::MaxTokens),
            "length"
        );
        assert_eq!(
            stop_reason_to_finish_reason(StopReason::Refusal),
            "content_filter"
        );
        assert_eq!(stop_reason_to_finish_reason(StopReason::Cancelled), "stop");
        assert_eq!(
            stop_reason_to_finish_reason(StopReason::MaxTurnRequests),
            "stop"
        );
    }

    #[test]
    fn aggregate_text_chunks_then_complete() {
        let updates = vec![
            AcpUpdate::Text("Hello, ".to_string()),
            AcpUpdate::Thought("(thinking)".to_string()),
            AcpUpdate::Text("world".to_string()),
            AcpUpdate::Complete(StopReason::EndTurn),
        ];
        match aggregate_updates(updates) {
            Aggregated::Done {
                content,
                finish_reason,
            } => {
                assert_eq!(content, "Hello, world");
                assert_eq!(finish_reason, "stop");
            }
            Aggregated::Failed(e) => panic!("expected Done, got Failed({e})"),
        }
    }

    #[test]
    fn aggregate_error_short_circuits() {
        let updates = vec![
            AcpUpdate::Text("partial".to_string()),
            AcpUpdate::Error("boom".to_string()),
        ];
        match aggregate_updates(updates) {
            Aggregated::Failed(e) => assert_eq!(e, "boom"),
            Aggregated::Done { .. } => panic!("expected Failed"),
        }
    }

    #[test]
    fn aggregate_no_complete_is_failed() {
        let updates = vec![AcpUpdate::Text("dangling".to_string())];
        assert!(matches!(aggregate_updates(updates), Aggregated::Failed(_)));
    }

    #[test]
    fn build_completion_shape() {
        let c = build_completion("echo-model".to_string(), "answer".to_string(), "stop");
        assert_eq!(c.object, "chat.completion");
        assert_eq!(c.model, "echo-model");
        assert_eq!(c.choices.len(), 1);
        assert_eq!(c.choices[0].message.role, "assistant");
        assert_eq!(c.choices[0].message.content.as_deref(), Some("answer"));
        assert_eq!(c.choices[0].finish_reason, "stop");
        assert_eq!(c.usage.total_tokens, 0);
    }

    /// Live round-trip through a real `claude-agent-acp` binary. Ignored
    /// by default; run on a machine where Claude Code is installed and
    /// authenticated:
    ///   cargo test -p nevoflux-llm-gateway acp_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a live, authenticated claude-agent-acp binary"]
    async fn acp_live_one_shot() {
        use nevoflux_llm::providers::acp::claude;
        use std::sync::Arc;

        let mut cfg = claude::build_config(std::env::current_dir().unwrap());
        cfg.use_mcp_bridge = false;
        cfg.inject_mcp_url = false;
        let acp = Arc::new(tokio::sync::Mutex::new(AcpUpstream::new(cfg)));

        let req = req_with(vec![msg("user", "Reply with exactly the word: pong")]);
        let resp = super::do_chat_completions_acp(acp, req, 1, false)
            .await
            .expect("acp completion");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn streaming_emits_role_then_content_then_done() {
        use axum::body::to_bytes;
        use nevoflux_llm::providers::acp::AcpUpdate;

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(AcpUpdate::Text("Hel".to_string())).await.unwrap();
        tx.send(AcpUpdate::Thought("ignored".to_string()))
            .await
            .unwrap();
        tx.send(AcpUpdate::Text("lo".to_string())).await.unwrap();
        tx.send(AcpUpdate::Complete(StopReason::EndTurn))
            .await
            .unwrap();
        drop(tx);

        let resp = super::stream_response(rx, "echo".to_string(), 7);
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        // First content chunk carries the assistant role; later chunks don't.
        assert!(text.contains("\"role\":\"assistant\""));
        assert!(text.contains("\"content\":\"Hel\""));
        assert!(text.contains("\"content\":\"lo\""));
        // Thought is dropped.
        assert!(!text.contains("ignored"));
        // Final chunk has finish_reason; stream terminates with [DONE].
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(text.contains("[DONE]"));
    }

    #[tokio::test]
    async fn streaming_error_emits_error_chunk_then_done() {
        use axum::body::to_bytes;
        use nevoflux_llm::providers::acp::AcpUpdate;

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(AcpUpdate::Text("partial".to_string()))
            .await
            .unwrap();
        tx.send(AcpUpdate::Error("kaboom".to_string()))
            .await
            .unwrap();
        drop(tx);

        let resp = super::stream_response(rx, "echo".to_string(), 8);
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("acp_stream_error"));
        assert!(text.contains("kaboom"));
        assert!(text.contains("[DONE]"));
    }
}
