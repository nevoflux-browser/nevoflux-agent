//! SSE event consumer for `/_eval/sessions/:id/events`.
//!
//! Parses `data:` lines from the bridge's response body into `DaemonEvent`
//! values. Ignores comment lines (heartbeats start with `:`).

use crate::termination::DaemonEvent;
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SseError {
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("malformed event: {0}")]
    Malformed(String),
}

/// Wrap a reqwest streaming Response into a Stream of DaemonEvent.
pub fn stream_events(
    resp: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<DaemonEvent, SseError>> + Send>> {
    let byte_stream = resp.bytes_stream();
    let parser = SseParser::new();
    Box::pin(parser.parse(byte_stream))
}

struct SseParser {
    buf: String,
}

impl SseParser {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    fn parse(
        mut self,
        mut byte_stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
    ) -> impl Stream<Item = Result<DaemonEvent, SseError>> + Send {
        async_stream::stream! {
            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(SseError::Transport(e));
                        return;
                    }
                };
                let s = match std::str::from_utf8(&chunk) {
                    Ok(s) => s,
                    Err(_) => {
                        yield Err(SseError::Malformed("non-utf8 chunk".into()));
                        continue;
                    }
                };
                self.buf.push_str(s);

                while let Some(nl) = self.buf.find('\n') {
                    let line = self.buf[..nl].to_string();
                    self.buf.drain(..=nl);

                    let line = line.trim_end_matches('\r');
                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }
                    if let Some(payload) = line.strip_prefix("data:") {
                        let json = payload.trim();
                        if json.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<DaemonEvent>(json) {
                            Ok(e) => yield Ok(e),
                            Err(e) => yield Err(SseError::Malformed(format!(
                                "json parse: {e} (line: {json:?})"
                            ))),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn fake_chunks(s: &str) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin {
        let bytes = Bytes::from(s.to_string());
        stream::iter(vec![Ok(bytes)])
    }

    #[tokio::test]
    async fn parses_single_data_line() {
        let raw = "data: {\"type\":\"token\",\"text\":\"hi\"}\n";
        let parser = SseParser::new();
        let mut events = Box::pin(parser.parse(fake_chunks(raw)));
        let evt = events.next().await.unwrap().unwrap();
        assert!(matches!(evt, DaemonEvent::Token { ref text } if text == "hi"));
    }

    #[tokio::test]
    async fn skips_keepalive_comments() {
        let raw = ":keepalive\ndata: {\"type\":\"stop\",\"reason\":\"natural\"}\n";
        let parser = SseParser::new();
        let mut events = Box::pin(parser.parse(fake_chunks(raw)));
        let evt = events.next().await.unwrap().unwrap();
        assert!(matches!(evt, DaemonEvent::Stop { .. }));
    }

    #[tokio::test]
    async fn handles_split_chunk_across_lines() {
        let parser = SseParser::new();
        let stream = stream::iter(vec![
            Ok(Bytes::from("data: {\"type\":\"tok")),
            Ok(Bytes::from("en\",\"text\":\"x\"}\n")),
        ]);
        let mut events = Box::pin(parser.parse(stream));
        let evt = events.next().await.unwrap().unwrap();
        assert!(matches!(evt, DaemonEvent::Token { ref text } if text == "x"));
    }
}
