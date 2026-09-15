//! Shared OpenAI-compatible SSE (`data: {...}`) parsing helpers.
//!
//! `stream_qwen` and `stream_deepseek_raw` (in `wasm::llm`) both bypass rig
//! and speak the OpenAI Chat Completions streaming wire format directly.
//! They independently reimplemented three things that need to behave
//! identically for every raw-HTTP OpenAI-compatible provider (including the
//! local raw-HTTP provider added on top of this module):
//!
//! - buffering raw byte chunks into complete SSE lines — a `data: ...` line
//!   can be split across two `bytes_stream` chunks at an arbitrary byte
//!   boundary, and parsing line-by-line per chunk (as the pre-refactor
//!   `stream_qwen` did) silently drops the half that lands in the next chunk
//! - separating a delta's `content` (text) from its `reasoning_content`
//!   (thinking) fragments
//! - accumulating fragmented `tool_calls` deltas by index and turning the
//!   finished argument string into JSON — falling back to an
//!   `INVALID_ARGUMENTS_KEY` marker (not a silently empty `{}`) when the
//!   accumulated text isn't valid JSON, so a caller can refuse to run the
//!   tool instead of running it with the wrong arguments

use crate::wasm::llm::{LlmToolCall, LlmUsage};
use nevoflux_protocol::json_repair::tool_arguments_or_marker;
use std::collections::BTreeMap;

/// Buffers raw SSE byte chunks into complete lines and yields `data: ...`
/// payloads (without the prefix), skipping the terminal `[DONE]` sentinel
/// and any non-`data:` line (blank separator lines, `event:` lines, etc.).
#[derive(Default)]
pub struct SseLineBuffer {
    buf: String,
}

impl SseLineBuffer {
    /// Feed raw bytes; returns complete `data:` payloads found so far. Bytes
    /// that don't yet complete a line stay buffered for the next call.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));

        let mut out = Vec::new();
        while let Some(nl_pos) = self.buf.find('\n') {
            let line = self.buf[..nl_pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=nl_pos);

            if let Some(data) = line.strip_prefix("data: ") {
                if data != "[DONE]" {
                    out.push(data.to_string());
                }
            }
        }
        out
    }
}

/// One piece of streamed content from a delta object.
#[derive(Debug, PartialEq)]
pub enum DeltaEvent {
    /// A `content` (assistant-visible text) fragment.
    Text(String),
    /// A `reasoning_content` (thinking) fragment.
    Reasoning(String),
}

/// Extract text/reasoning events from an OpenAI-compatible `delta` object.
/// Returns `Text` before `Reasoning` when a single delta carries both,
/// matching the field order every provider observed so far sends them in.
pub fn delta_events(delta: &serde_json::Value) -> Vec<DeltaEvent> {
    let mut events = Vec::new();
    if let Some(content) = delta["content"].as_str() {
        if !content.is_empty() {
            events.push(DeltaEvent::Text(content.to_string()));
        }
    }
    if let Some(reasoning) = delta["reasoning_content"].as_str() {
        if !reasoning.is_empty() {
            events.push(DeltaEvent::Reasoning(reasoning.to_string()));
        }
    }
    events
}

/// Accumulates fragmented `tool_calls` deltas (arguments arrive in pieces
/// across multiple SSE events) keyed by the wire `index`, in the order that
/// index implies rather than arrival order.
#[derive(Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<i64, (String, String, String)>,
}

impl ToolCallAccumulator {
    /// Fold one delta's `tool_calls` array (if any) into the accumulator.
    /// Safe to call on every delta; deltas without `tool_calls` are a no-op.
    pub fn apply(&mut self, delta: &serde_json::Value) {
        let Some(tool_calls) = delta["tool_calls"].as_array() else {
            return;
        };
        for tc in tool_calls {
            let index = tc["index"].as_i64().unwrap_or(0);
            let entry = self
                .calls
                .entry(index)
                .or_insert_with(|| (String::new(), String::new(), String::new()));

            if let Some(id) = tc["id"].as_str() {
                entry.0 = id.to_string();
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func["name"].as_str() {
                    entry.1 = name.to_string();
                }
                if let Some(args) = func["arguments"].as_str() {
                    entry.2.push_str(args);
                }
            }
        }
    }

    /// True if no `tool_calls` deltas have been accumulated.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Finalize into tool calls, in index order. Each call's `arguments` is
    /// parsed via `tool_arguments_or_marker`: unparseable argument text
    /// becomes an `INVALID_ARGUMENTS_KEY` marker object instead of a
    /// silently empty `{}`.
    pub fn finish(self) -> Vec<LlmToolCall> {
        self.calls
            .into_values()
            .map(|(id, name, arguments)| LlmToolCall {
                id: id.clone(),
                call_id: Some(id),
                name,
                arguments: tool_arguments_or_marker(&arguments),
                signature: None,
            })
            .collect()
    }
}

/// Extract usage totals from a streamed chunk's `usage` field, if present.
/// OpenAI-compatible streams only populate this on the terminal chunk (and
/// only when the request opted in via `stream_options.include_usage`).
pub fn usage_from(chunk: &serde_json::Value) -> Option<LlmUsage> {
    let usage = chunk.get("usage")?;
    if usage.is_null() {
        return None;
    }
    Some(LlmUsage {
        prompt_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
        total_tokens: usage["total_tokens"].as_u64().unwrap_or(0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_across_chunks_are_reassembled() {
        let mut b = SseLineBuffer::default();
        assert!(b.push(b"data: {\"a\":").is_empty());
        assert_eq!(
            b.push(b"1}\r\n\ndata: [DONE]\n"),
            vec!["{\"a\":1}".to_string()]
        );
    }

    #[test]
    fn tool_call_deltas_accumulate_by_index() {
        let mut acc = ToolCallAccumulator::default();
        acc.apply(&serde_json::json!({"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}));
        acc.apply(
            &serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]}),
        );
        let calls = acc.finish();
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, serde_json::json!({"path":"a.txt"}));
    }

    #[test]
    fn unparseable_arguments_become_a_marker_not_an_empty_object() {
        let mut acc = ToolCallAccumulator::default();
        acc.apply(&serde_json::json!({"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"not json at all"}}]}));
        let v = &acc.finish()[0].arguments;
        assert!(v
            .get(nevoflux_protocol::json_repair::INVALID_ARGUMENTS_KEY)
            .is_some());
    }

    #[test]
    fn reasoning_and_text_are_separate_events() {
        let ev = delta_events(&serde_json::json!({"content":"hi","reasoning_content":"think"}));
        assert_eq!(
            ev,
            vec![
                DeltaEvent::Text("hi".into()),
                DeltaEvent::Reasoning("think".into())
            ]
        );
    }

    #[test]
    fn tool_call_accumulator_starts_empty() {
        let acc = ToolCallAccumulator::default();
        assert!(acc.is_empty());
    }

    #[test]
    fn tool_call_accumulator_is_not_empty_after_apply() {
        let mut acc = ToolCallAccumulator::default();
        acc.apply(&serde_json::json!({"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{}"}}]}));
        assert!(!acc.is_empty());
    }

    #[test]
    fn usage_from_reads_totals_when_present() {
        let chunk = serde_json::json!({
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let usage = usage_from(&chunk).unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn usage_from_is_none_when_absent() {
        let chunk = serde_json::json!({"choices": []});
        assert!(usage_from(&chunk).is_none());
    }

    #[test]
    fn lines_that_are_not_data_are_dropped() {
        let mut b = SseLineBuffer::default();
        let out = b.push(b"event: ping\n\ndata: {\"a\":1}\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string()]);
    }
}
