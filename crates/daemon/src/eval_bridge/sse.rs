//! SSE helpers for the eval bridge.

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use serde::Serialize;
use std::convert::Infallible;
use std::pin::Pin;

/// Wire-format for events streamed to eval clients over
/// `GET /_eval/sessions/:id/events`.
///
/// The `type` field is the SSE discriminant. Clients should expect new variants
/// in future protocol revisions; unknown variants must be silently skipped.
///
/// Phase-2 note: `Token`, `ToolCall`, and `ToolResult` are defined here for
/// spec completeness but are not yet wired to real daemon domain events.
/// For now the stream emits `DaemonEvent` (raw bus payload) or `Error`
/// (when the event bus is unavailable). Full mapping is deferred to phase-2
/// once the eval client requirements stabilise.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvalEvent {
    /// A streamed text token from the LLM.
    Token { text: String },
    /// The agent invoked a tool.
    ToolCall {
        name: String,
        args: serde_json::Value,
        trace_id: String,
    },
    /// A tool invocation returned.
    ToolResult {
        trace_id: String,
        ok: bool,
        result: serde_json::Value,
    },
    /// A raw domain event forwarded from the daemon EventBus.
    ///
    /// `name` is the EventBus topic; `payload` is the opaque bus payload.
    DaemonEvent {
        name: String,
        payload: serde_json::Value,
    },
    /// The agent turn finished normally.
    Stop { reason: String },
    /// An error prevented normal event delivery.
    Error { message: String },
}

/// Wrap a `Stream<Item = EvalEvent>` into an SSE `Response` with keep-alive.
///
/// All items are serialised as JSON and sent as `data:` lines. Serialisation
/// failures produce an inline `Error` event rather than terminating the stream.
pub fn to_sse(
    stream: Pin<Box<dyn Stream<Item = EvalEvent> + Send>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    use futures::StreamExt;
    let mapped = stream.map(|evt| {
        let json = serde_json::to_string(&evt)
            .unwrap_or_else(|e| format!(r#"{{"type":"error","message":"serialize: {e}"}}"#));
        Ok(Event::default().data(json))
    });
    Sse::new(mapped).keep_alive(KeepAlive::default())
}
