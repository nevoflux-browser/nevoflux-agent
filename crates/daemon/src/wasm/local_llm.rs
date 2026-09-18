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
use crate::local::admission;
use crate::local::endpoint::{self, LocalEndpoint};
use crate::wasm::llm::{
    build_deepseek_request_body, LlmChatRequest, LlmChatResponse, LlmStreamChunk, LlmToolCall,
};
use crate::wasm::openai_sse::{
    delta_events, usage_from, DeltaEvent, SseLineBuffer, ToolCallAccumulator,
};
use futures::StreamExt;
use nevoflux_protocol::json_repair::tool_arguments_or_marker;
use std::future::Future;
use tokio::sync::mpsc;

/// Resolve the endpoint to actually talk to, once an [`admission::Permit`]
/// is already held.
///
/// Interactive (P0) requests may cold-start the engine via
/// `endpoint::ensure()`. Background (P1) requests must not (§17.2.1's "P1
/// 不冷启动引擎"): for them this only ever reads what's already published
/// (`endpoint::current()` -- a plain sync `RwLock` read that structurally
/// cannot itself start anything), returning [`admission::AdmissionError::Deferred`]
/// if nothing is published yet. That also keeps a preempted background
/// caller's cancellation meaningful: with nothing slow to wait through
/// here, `run_admitted`'s `select!` against `permit.cancel` is never blind
/// to an in-progress cold start the way it would be if this called
/// `ensure()` for P1 too (a cold start can take up to 120s and does not
/// itself observe a `Permit`'s cancellation).
async fn endpoint_for_priority() -> Result<LocalEndpoint> {
    if admission::current_priority() == admission::Priority::Background {
        endpoint::current().ok_or(DaemonError::Admission(admission::AdmissionError::Deferred))
    } else {
        endpoint::ensure().await.map_err(DaemonError::InternalError)
    }
}

/// Run `do_work` (the admitted HTTP round-trip / stream -- it resolves its
/// own endpoint via [`endpoint_for_priority`] and races itself against
/// `permit.cancel` internally) under the local engine's token-budget
/// admission controller (Task 2.7, `crate::local::admission`).
///
/// Interactive (P0, chat) requests acquire once: `Admission::acquire`
/// already waits FIFO with no timeout, so there is nothing here worth
/// retrying, and an interactive permit is never preempted.
///
/// Background (P1, memory extraction / knowledge consolidation -- see
/// `admission::background`) requests retry across BOTH failure modes under
/// ONE shared attempt budget -- 3 attempts, 30s backoff between them, then
/// `warn!` and give up. That is §17.2.4's "N 次后放弃并记日志" and the
/// brief's "Background callers retry up to 3 times with 30s backoff",
/// which is the SAME sentence covering both: a rejection at `acquire`
/// (`Deferred`/`QueueFull`) counts as an attempt, and so does an
/// `AdmissionError` (typically `Preempted`) coming back from `do_work`
/// AFTER admission -- a P0 arrival that cuts a P1 off mid-flight must
/// re-queue the P1's work (§17.2.4's "重新排队"), not drop it, and
/// re-queueing shares the same N-attempt ceiling as an outright rejection
/// rather than resetting it on every preemption. `AdmissionError::TooLarge`
/// is never retried, from either source -- no amount of waiting changes
/// whether a request fits the whole context pool.
///
/// **Contract for `do_work`:** re-queueing re-invokes the SAME closure
/// against whatever state it closed over (e.g. `stream_local`'s `tx`), so
/// `do_work` must only ever report `DaemonError::Admission(_)` (the
/// retryable signal) up to the point it has produced no side effect a
/// retry couldn't cleanly redo from scratch. `execute_local_chat` can
/// always retry cleanly (no response has gone anywhere until the whole
/// call succeeds). `stream_local` cannot once it has sent a chunk on
/// `tx` -- it tracks that and reports a terminal `DaemonError::InternalError`
/// instead once anything has been streamed, precisely so a re-queue can
/// never replay a prefix to an already-partially-served client.
async fn run_admitted<T, F, Fut>(req: &LlmChatRequest, do_work: F) -> Result<T>
where
    F: Fn(admission::Permit) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let prio = admission::current_priority();
    let tokens = admission::estimate_request_tokens(req);

    if prio == admission::Priority::Interactive {
        let engine_ready = endpoint::current().is_some();
        let permit = admission::admission()
            .acquire(prio, tokens, engine_ready)
            .await?;
        return do_work(permit).await;
    }

    const MAX_ATTEMPTS: u32 = 3;
    // Real 30s in production (§17.2.4 / the brief); cut to a few
    // milliseconds under `cfg(test)` so a test exercising this retry loop
    // -- e.g. re-queueing after a real preemption -- doesn't cost up to a
    // minute of actual wall-clock time. `cfg!(test)` is a compile-time
    // constant, and `Duration::from_millis`/`from_secs` are `const fn`, so
    // this has zero effect on the shipped binary.
    const BACKOFF: std::time::Duration = if cfg!(test) {
        std::time::Duration::from_millis(20)
    } else {
        std::time::Duration::from_secs(30)
    };
    let mut last_err: Option<admission::AdmissionError> = None;

    for attempt in 1..=MAX_ATTEMPTS {
        // Re-sampled on every attempt, not captured once before the loop:
        // a stale `engine_ready` would make this retry provably useless
        // whenever the engine wasn't ready at entry (every attempt would
        // see the same stale `false` and never observe the supervisor
        // publishing) -- exactly the condition this retry exists to wait
        // out (§17.2.1's "延后（不失败）").
        let engine_ready = endpoint::current().is_some();
        let permit = match admission::admission()
            .acquire(prio, tokens, engine_ready)
            .await
        {
            Ok(permit) => permit,
            Err(admission::AdmissionError::TooLarge) => {
                return Err(DaemonError::Admission(admission::AdmissionError::TooLarge));
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(BACKOFF).await;
                }
                continue;
            }
        };

        match do_work(permit).await {
            Ok(v) => return Ok(v),
            Err(DaemonError::Admission(admission::AdmissionError::TooLarge)) => {
                return Err(DaemonError::Admission(admission::AdmissionError::TooLarge));
            }
            Err(DaemonError::Admission(e)) => {
                last_err = Some(e);
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(BACKOFF).await;
                }
            }
            Err(e) => return Err(e),
        }
    }

    let last_err = last_err.expect("loop runs at least once and always records an error");
    tracing::warn!(
        error = %last_err,
        attempts = MAX_ATTEMPTS,
        "background local-engine admission gave up"
    );
    Err(DaemonError::Admission(last_err))
}

/// Apply the local-engine HTTP policy to a builder: unconditionally
/// strips any proxy configuration — whether inherited from
/// `HTTP_PROXY`/`ALL_PROXY` or set explicitly via `.proxy(...)` earlier
/// on the same builder — on top of whatever timeouts the caller already
/// configured (R30, fix round 1 follow-up item B / R33).
///
/// The engine is loopback-only, so routing its traffic through a
/// misconfigured (or malicious) proxy would leak LocalOnly-latched prompt
/// content off the machine — exactly what the latch exists to prevent.
/// `reqwest` otherwise honors `HTTP_PROXY`/`ALL_PROXY` for every request
/// regardless of host; `.no_proxy()` opts a client out unconditionally
/// rather than relying on `NO_PROXY` covering `127.0.0.1`/`localhost`.
///
/// Factored out (rather than inlined at each `reqwest::Client::builder()`
/// call site) specifically so it can be tested deterministically: start
/// from a builder with an *explicit* proxy set, run it through here, and
/// confirm a real request still reaches its target directly — see
/// `apply_local_http_policy_clears_an_explicit_proxy` below. That avoids
/// mutating the process's real `HTTP_PROXY` env var, which would race any
/// other test in the crate that builds a plain (non-`no_proxy`) client
/// concurrently (R33 — replaces an earlier version of this test that did
/// mutate the env var).
fn apply_local_http_policy(b: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    b.no_proxy()
}

/// Truncate an error-response body to at most 500 *characters* for use in an
/// error message.
///
/// `&text[..500]` (byte-slicing) panics whenever byte offset 500 doesn't
/// land on a UTF-8 character boundary — entirely possible for a body that
/// contains any multibyte character (e.g. a non-ASCII error message from
/// the engine). Truncating by `chars()` instead is always a valid slice,
/// whatever the byte layout.
fn truncate_for_error(text: &str) -> String {
    text.chars().take(500).collect()
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
    let request = &request;
    run_admitted(request, |permit| async move {
        let ep = endpoint_for_priority().await?;
        let body = local_request_body(&ep, request, false);
        let url = format!("{}/chat/completions", ep.base_url.trim_end_matches('/'));

        tracing::debug!(base_url = %ep.base_url, model = %ep.model_id, "local engine chat POST");

        let client = apply_local_http_policy(
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(600)),
        )
        .build()
        .map_err(|e| DaemonError::InternalError(format!("Failed to build HTTP client: {e}")))?;

        // The actual round-trip, raced against `permit.cancel`: a P0
        // arrival short on token budget can preempt this in-flight P1
        // request (see `admission::Admission::acquire`). `select!` dropping
        // the losing branch drops this whole in-progress request/response
        // with it. `biased`, with the request branch listed first: an
        // unbiased `select!` can still report `Preempted` for a request
        // that finished in the very same poll as the cancellation,
        // discarding an already-arrived response -- `biased` only lets
        // cancellation win when the request genuinely has not finished.
        let do_request = async {
            let response = client
                .post(&url)
                .bearer_auth(&ep.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    DaemonError::InternalError(format!("Local engine request failed: {e}"))
                })?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(DaemonError::InternalError(format!(
                    "Local engine HTTP {}: {}",
                    status,
                    truncate_for_error(&text)
                )));
            }

            let raw: serde_json::Value = response.json().await.map_err(|e| {
                DaemonError::InternalError(format!("Failed to parse response: {e}"))
            })?;

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
        };

        tokio::select! {
            biased;
            result = do_request => result,
            _ = permit.cancel.cancelled() => Err(DaemonError::Admission(admission::AdmissionError::Preempted)),
        }
    })
    .await
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
    let request = &request;
    let tx = &tx;
    run_admitted(request, |permit| async move {
        let ep = endpoint_for_priority().await?;
        let body = local_request_body(&ep, request, true);
        let url = format!("{}/chat/completions", ep.base_url.trim_end_matches('/'));

        tracing::debug!(base_url = %ep.base_url, model = %ep.model_id, "local engine stream POST");

        let client = apply_local_http_policy(
            reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(10)),
        )
        .build()
        .map_err(|e| DaemonError::InternalError(format!("Failed to build HTTP client: {e}")))?;

        // Tracks whether ANY chunk has actually reached `tx` yet. A
        // preempted stream must not re-queue once this is true: `run_admitted`'s
        // retry re-invokes this whole closure against the SAME `tx`, and a
        // client that already received a prefix has no way to tell a
        // replay from more of the same stream -- prefix-then-full-output
        // plus a duplicate `done: true`. A client that gets nothing at all
        // can at least tell the call failed. An `AtomicBool`, not a
        // `Cell`: both `select!` arms below run on this one task and never
        // race each other, but this whole future still needs to stay
        // `Send` (it is `tokio::spawn`ed elsewhere via `wasm::llm`'s
        // dispatch), and a `&Cell<bool>` held across an `.await` isn't
        // `Send` (`Cell` is deliberately `!Sync`) the way `&AtomicBool` is.
        let sent_any = std::sync::atomic::AtomicBool::new(false);

        // See the matching comment in `execute_local_chat`: raced against
        // `permit.cancel`, `biased` with the stream branch listed first.
        let do_stream = async {
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
                    truncate_for_error(&text)
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

                            // Usage may ride a chunk with an empty `choices`
                            // array (the terminal event), so this must run
                            // before the `choices[0]` lookup below rather
                            // than after it.
                            if let Some(u) = usage_from(&chunk) {
                                usage = Some(u);
                            }

                            let Some(choice) = chunk["choices"].get(0) else {
                                continue;
                            };
                            let delta = &choice["delta"];

                            for event in delta_events(delta) {
                                sent_any.store(true, std::sync::atomic::Ordering::Relaxed);
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
                sent_any.store(true, std::sync::atomic::Ordering::Relaxed);
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
        };

        tokio::select! {
            biased;
            result = do_stream => result,
            _ = permit.cancel.cancelled() => {
                if sent_any.load(std::sync::atomic::Ordering::Relaxed) {
                    // A re-queue would re-invoke this closure against the
                    // SAME `tx` and replay from the top -- the client
                    // already has a prefix, so surface a terminal failure
                    // instead of `run_admitted`'s retryable
                    // `DaemonError::Admission(Preempted)`. A stream that
                    // ends abruptly is recoverable by the caller; one that
                    // silently duplicates its prefix is not.
                    Err(DaemonError::InternalError(
                        admission::AdmissionError::Preempted.to_string(),
                    ))
                } else {
                    Err(DaemonError::Admission(admission::AdmissionError::Preempted))
                }
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::llm::LlmMessage;

    /// Resets the global endpoint registry on drop, so a test's `publish`
    /// can't leak into a later test even if an assertion panics first.
    ///
    /// Every test that constructs one holds
    /// [`crate::local::endpoint::test_serial_async`] for its whole body
    /// (R35 — a lock scoped to the `endpoint` registry specifically, not
    /// shared with `crate::local::latch`'s real-global tests, so the two
    /// domains don't serialize behind each other for no reason), so by
    /// the time this guard drops (still inside that same critical section
    /// — see declaration order in each test), the cleanup write is
    /// already exclusive; no separate lock needed here.
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

    /// Read a full HTTP/1.1 request (headers + body) off `socket`, using
    /// `Content-Length` to know when the body is complete. Returns the raw
    /// bytes read (headers included, exactly as sent).
    async fn read_full_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;

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
        buf
    }

    /// Minimal hand-rolled HTTP/1.1 server for testing the raw-HTTP local
    /// provider without a mocking dependency.
    ///
    /// Accepts exactly one connection, reads the full request via
    /// [`read_full_request`], writes back `head` (the status line plus any
    /// extra header lines, `\r\n`-joined, no trailing blank line) followed
    /// by `body` — connection-close delimited (no `Content-Length` /
    /// `Transfer-Encoding` on the response, which is a legal HTTP/1.1
    /// framing per RFC 7230 §3.3.3 rule 7 and exactly how a real streaming
    /// server behaves) — then closes the socket.
    ///
    /// Returns the endpoint URL and a receiver for the captured request
    /// text, lower-cased — so a caller doesn't have to guess whether the
    /// HTTP stack sent `Authorization: Bearer …` or `authorization: Bearer
    /// …` on the wire.
    async fn fake_http_server(
        head: &str,
        body: Vec<u8>,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (req_tx, req_rx) = tokio::sync::oneshot::channel();
        let head = head.to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let buf = read_full_request(&mut socket).await;
            let _ = req_tx.send(String::from_utf8_lossy(&buf).to_ascii_lowercase());

            let mut response = format!("{head}\r\nConnection: close\r\n\r\n").into_bytes();
            response.extend_from_slice(&body);
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        });

        (format!("http://{addr}"), req_rx)
    }

    /// SSE-flavored sibling of [`fake_http_server`]: joins `lines` with
    /// blank-line separators (`data: ...\n\n`) and serves them as a
    /// `200 OK text/event-stream` body.
    async fn fake_sse_server(lines: Vec<&str>) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let body: String = lines.into_iter().map(|l| format!("{l}\n\n")).collect();
        fake_http_server(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream",
            body.into_bytes(),
        )
        .await
    }

    /// Serves TWO connections in sequence, with DIFFERENT bodies: the first
    /// (`first_body`) only after sleeping `slow_delay` (real wall-clock
    /// time, same as the rest of this fixture harness) after reading its
    /// request -- long enough to keep a caller's `.send().await` genuinely
    /// pending for a controlled window -- the second (`second_body`)
    /// immediately. Used to force a real mid-flight preemption on the
    /// first attempt (its connection is dropped when the caller gives up,
    /// so the slow response, once it finally arrives, lands on nobody) and
    /// then let a caller assert the eventual result came specifically from
    /// the SECOND response -- not merely that the call returned `Ok` at
    /// all, which the first response alone could also produce if no
    /// preemption happened (a silently vacuous pass).
    async fn fake_http_server_slow_then_fast(
        slow_delay: std::time::Duration,
        head: &str,
        first_body: Vec<u8>,
        second_body: Vec<u8>,
    ) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let head = head.to_string();

        tokio::spawn(async move {
            for (delay, body) in [(Some(slow_delay), first_body), (None, second_body)] {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let _ = read_full_request(&mut socket).await;
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                let mut response = format!("{head}\r\nConnection: close\r\n\r\n").into_bytes();
                response.extend_from_slice(&body);
                // The first connection's write commonly fails (the caller
                // already dropped it on preemption) -- expected, not a
                // fixture bug, so ignored just like the second.
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });

        format!("http://{addr}")
    }

    /// Exercises the `select!` + re-queue wiring in `execute_local_chat`
    /// itself (not `Admission` in isolation, which all the other
    /// admission-related tests in `local::admission` do): a background
    /// request's first attempt is kept genuinely in flight by a slow
    /// fixture response, a real interactive `acquire` against the SAME
    /// production singleton forces it to preempt out, and -- once the
    /// aggressor releases -- `run_admitted`'s re-queue must retry and
    /// succeed against the fixture's second (fast) response. This closes
    /// the coverage gap the review found -- the wiring landed with zero
    /// tests through the code that actually uses it.
    ///
    /// The aggressor permit is dropped BEFORE awaiting the victim, not
    /// after: `run_admitted`'s retry re-`acquire`s, which for a background
    /// request waits for no interactive permit to be in flight -- awaiting
    /// the victim first while still holding the aggressor would deadlock
    /// the retry against this very test.
    ///
    /// Deliberately drives the real global [`crate::local::admission::admission`]
    /// singleton rather than a local `Admission` (which `execute_local_chat`
    /// has no way to accept instead) -- unlike the `local::admission` unit
    /// tests, this is NOT hermetic against unrelated concurrently-running
    /// tests in the same process, by necessity. That's judged acceptable
    /// here specifically because: the aggressor only preempts BACKGROUND
    /// permits (this crate's only other background-priority caller in a
    /// test is this same test), it can never evict an unrelated
    /// INTERACTIVE permit belonging to another test, and it has no
    /// timeout -- so at worst a concurrently-running unrelated test's own
    /// interactive request waits briefly longer, never fails or hangs.
    #[tokio::test]
    async fn execute_local_chat_re_queues_after_a_p0_arrival_forces_it_out() {
        // R35: see `stream_local_sends_bearer_...`'s comment above.
        let _g = crate::local::endpoint::test_serial_async().await;
        let _reset = EndpointGuard;

        // The two responses carry DIFFERENT content markers specifically so
        // the assertion below can tell which one actually answered the
        // call -- not just that SOME response did, which the slow one
        // alone could also produce if no preemption happened at all (a
        // silently vacuous pass: skewed timing, a scheduler quirk, or a
        // future regression that stops the aggressor from preempting
        // anything would leave this test green for the wrong reason).
        const STALE_MARKER: &str = "stale-should-never-be-observed";
        const REQUEUED_MARKER: &str = "requeued-after-preemption";
        let url = fake_http_server_slow_then_fast(
            std::time::Duration::from_millis(500),
            "HTTP/1.1 200 OK\r\nContent-Type: application/json",
            serde_json::json!({"choices": [{"message": {"content": STALE_MARKER}}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"choices": [{"message": {"content": REQUEUED_MARKER}}]})
                .to_string()
                .into_bytes(),
        )
        .await;
        endpoint::publish(Some(LocalEndpoint {
            base_url: url,
            api_key: "k".into(),
            n_ctx: 16384,
            model_id: "m".into(),
            thinking_hybrid: false,
        }));

        // The victim: a small background (P1) request whose first attempt's
        // HTTP round-trip is still pending (blocked on the fixture's
        // artificial delay) when the aggressor below arrives.
        let victim = tokio::spawn(admission::background(async move {
            execute_local_chat(LlmChatRequest {
                max_tokens: Some(50),
                ..Default::default()
            })
            .await
        }));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // The aggressor: a real interactive `acquire` against the same
        // production singleton the victim went through, sized so that
        // admitting it must preempt the victim (the only P1 in flight).
        let hog_tokens = crate::local::config::CTX_FLOOR - 20;
        let p0 = admission::admission()
            .acquire(admission::Priority::Interactive, hog_tokens, true)
            .await
            .expect("the interactive aggressor admits by preempting the victim's permit");

        // Give the victim's `select!` a moment to actually observe the
        // cancellation before releasing the aggressor -- then release it
        // BEFORE awaiting the victim (see the test's doc comment for why
        // the other order deadlocks the re-queued retry against itself).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(p0);

        let result = victim.await.expect("victim task did not panic");
        let response = result.expect(
            "the re-queued retry must succeed against the fixture's second (fast) response \
             once the aggressor releases -- Preempted must not be terminal for a background \
             caller (§17.2.4)",
        );
        assert_eq!(
            response.content, REQUEUED_MARKER,
            "the response must come from the re-queued SECOND attempt, not the first -- an \
             `Ok` alone doesn't prove the preemption-and-requeue path actually ran (it would \
             pass identically if the aggressor never preempted anything and the first, slow \
             response just answered the only attempt)"
        );
    }

    #[tokio::test]
    async fn stream_local_sends_bearer_and_parses_text_reasoning_and_tools() {
        // R35: this test needs exclusive use of the shared `endpoint`
        // registry for its WHOLE body (publish, then read it back via
        // `stream_local`'s internal `endpoint::ensure`) — a lock scoped to
        // just the `endpoint::publish` write is not enough, since another
        // concurrently-running endpoint test could overwrite the registry
        // in the gap before the read (observed empirically, not just
        // theoretically). `test_serial_async()` is `.await`-safe (a tokio
        // mutex, not `test_serial()`'s std one), so holding it for the
        // whole body can't starve unrelated timing-sensitive tests. Uses
        // `endpoint::test_serial_async` — scoped to this registry only,
        // not shared with `latch`'s real-global tests (fix round 1
        // follow-up tuning: sharing one lock across both domains
        // serialized them into one needlessly long chain).
        let _g = crate::local::endpoint::test_serial_async().await;
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
        assert!(req.contains("\"include_usage\":true"));
    }

    #[tokio::test]
    async fn cloud_call_is_refused_while_latched() {
        // R35 / R26: `set`/`is_on` are thread-local overrides in test
        // builds (never touch the real global), so this test needs no
        // `test_serial()` at all — the thread-local is invisible to every
        // other concurrently-running test.
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

    #[test]
    fn local_request_body_includes_stream_options_only_when_streaming() {
        let ep = LocalEndpoint {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "k".into(),
            n_ctx: 16384,
            model_id: "m".into(),
            thinking_hybrid: false,
        };
        let req = LlmChatRequest::default();

        let streaming = local_request_body(&ep, &req, true);
        assert_eq!(
            streaming["stream_options"],
            serde_json::json!({"include_usage": true})
        );

        let non_streaming = local_request_body(&ep, &req, false);
        assert!(
            non_streaming.get("stream_options").is_none(),
            "non-stream body must not carry stream_options: {non_streaming}"
        );
    }

    #[tokio::test]
    async fn non_2xx_response_over_500_bytes_with_split_multibyte_char_does_not_panic() {
        // R35: see `stream_local_sends_bearer_...`'s comment above.
        let _g = crate::local::endpoint::test_serial_async().await;
        let _reset = EndpointGuard;

        // 498 ASCII bytes, then a 3-byte CJK character (bytes 498-500), then
        // more filler. Byte offset 500 lands on that character's last
        // continuation byte — not a char boundary — which is exactly what
        // made the old `&text[..500]` byte-slice panic.
        let mut body = "x".repeat(498);
        body.push('\u{597d}'); // 好, 3 bytes: E5 A5 BD
        body.push_str("yyyyyyyyyy");
        assert!(body.len() > 500, "test body must exceed 500 bytes");
        assert!(
            !body.is_char_boundary(500),
            "test body must straddle byte 500"
        );

        let (url, _captured) =
            fake_http_server("HTTP/1.1 500 Internal Server Error", body.into_bytes()).await;
        endpoint::publish(Some(LocalEndpoint {
            base_url: url,
            api_key: "k".into(),
            n_ctx: 16384,
            model_id: "m".into(),
            thinking_hybrid: false,
        }));

        let err = execute_local_chat(LlmChatRequest::default())
            .await
            .expect_err("a non-2xx response must be an Err, not a panic");
        assert!(
            err.to_string().contains("500"),
            "error should mention the HTTP status: {err}"
        );
    }

    #[tokio::test]
    async fn execute_local_chat_sends_bearer_and_parses_content_tools_and_usage() {
        // R35: see `stream_local_sends_bearer_...`'s comment above.
        let _g = crate::local::endpoint::test_serial_async().await;
        let _reset = EndpointGuard;

        let body = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "done",
                    "tool_calls": [{
                        "id": "t1",
                        "function": {
                            "name": "read",
                            "arguments": "{\"path\":\"x\"}"
                        }
                    }]
                }
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        })
        .to_string();

        let (url, captured) = fake_http_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json",
            body.into_bytes(),
        )
        .await;
        endpoint::publish(Some(LocalEndpoint {
            base_url: url,
            api_key: "k".into(),
            n_ctx: 16384,
            model_id: "m".into(),
            thinking_hybrid: false,
        }));

        let resp = execute_local_chat(LlmChatRequest {
            messages: vec![LlmMessage::user("hi")],
            ..Default::default()
        })
        .await
        .unwrap();

        assert_eq!(resp.content, "done");
        let tools = resp.tool_calls.expect("tool_calls must be parsed");
        assert_eq!(tools[0].name, "read");
        assert_eq!(tools[0].arguments, serde_json::json!({"path": "x"}));
        let usage = resp.usage.expect("usage must be parsed");
        assert_eq!(usage.total_tokens, 7);

        let req = captured.await.unwrap();
        assert!(req.contains("authorization: bearer k"));
    }

    /// R30 / R33 (fix round 1 follow-up item B): `apply_local_http_policy`
    /// must clear even an *explicit* proxy set on the builder, not just
    /// avoid reading `HTTP_PROXY` from the environment. Deterministic and
    /// touches no process-global env var (replacing an earlier version of
    /// this test that mutated `HTTP_PROXY`, which could race any other
    /// test in the crate building a plain `reqwest::Client` concurrently):
    /// start from a builder explicitly proxying everything through
    /// `127.0.0.1:9` (nothing listens there — the "discard" port), run it
    /// through `apply_local_http_policy`, and confirm the built client
    /// still reaches a real local server directly. If the policy failed
    /// to clear the proxy, the request would instead try to speak
    /// HTTP-proxy protocol to `:9` and fail.
    #[tokio::test]
    async fn apply_local_http_policy_clears_an_explicit_proxy() {
        let body = serde_json::json!({"ok": true}).to_string();
        let (url, _captured) = fake_http_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json",
            body.into_bytes(),
        )
        .await;

        let builder = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").expect("valid proxy url"));
        let client = apply_local_http_policy(builder)
            .build()
            .expect("client should build");

        let resp = client.get(&url).send().await;
        assert!(
            resp.is_ok(),
            "apply_local_http_policy must clear the explicit proxy so the request reaches \
             the target directly; got {:?}",
            resp.err()
        );
    }
}
