//! Termination strategy + answer extraction.
//!
//! See spec §6.2.3 — task "completion" is two independent axes:
//! - When to stop watching the SSE stream (`TerminationStrategy`)
//! - How to pull `final_answer` from the collected events (`AnswerExtractor`)
//!
//! Default `(NaturalStop, LastAssistantMessage)` covers ~80% of benchmarks;
//! BrowseComp / Canvas SDK / MCP-bidir override per-benchmark.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A daemon event observed by the SSE consumer. Wire-format-matched to
/// `daemon::eval_bridge::sse::EvalEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
    Token { text: String },
    ToolCall { name: String, args: serde_json::Value, trace_id: String },
    ToolResult { trace_id: String, ok: bool, result: serde_json::Value },
    DaemonEvent { name: String, payload: serde_json::Value },
    Stop { reason: String },
    Error { message: String },
}

/// What the runner should do after observing the events so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminationDecision {
    /// Keep watching the SSE stream.
    Continue,
    /// Stop normally; judge will run.
    Stop,
    /// Stop with a hard failure (judge still runs but typically marks fail).
    StopWithError(String),
}

#[derive(Clone)]
pub enum TerminationStrategy {
    /// Wait for `Stop` event OR hard task timeout. Most benchmarks.
    NaturalStop,

    /// Wait for `<ANSWER>...</ANSWER>` in token stream, OR `Stop`, OR timeout.
    /// Configure with `Benchmark::prompt_suffix` to inject answer-tag instruction.
    AnswerTag,

    /// Wait for `Stop`. When Stop fires, check that ALL named daemon_event
    /// names have been observed. If any are missing, return `StopWithError`.
    /// Canvas SDK / MCP-bidir style.
    StopWithRequiredEvents(Vec<String>),

    /// Escape hatch: user-supplied predicate.
    Custom(Arc<dyn Fn(&[DaemonEvent]) -> TerminationDecision + Send + Sync>),
}

impl std::fmt::Debug for TerminationStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NaturalStop => f.write_str("NaturalStop"),
            Self::AnswerTag => f.write_str("AnswerTag"),
            Self::StopWithRequiredEvents(names) => {
                write!(f, "StopWithRequiredEvents({names:?})")
            }
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

impl TerminationStrategy {
    /// Evaluate a stream of events against the strategy.
    /// Called by runner after each new event arrives.
    pub fn evaluate(&self, events: &[DaemonEvent]) -> TerminationDecision {
        match self {
            Self::NaturalStop => evaluate_natural_stop(events),
            Self::AnswerTag => evaluate_answer_tag(events),
            Self::StopWithRequiredEvents(required) => {
                evaluate_required_events(events, required)
            }
            Self::Custom(f) => f(events),
        }
    }
}

fn evaluate_natural_stop(events: &[DaemonEvent]) -> TerminationDecision {
    if events.iter().any(|e| matches!(e, DaemonEvent::Stop { .. })) {
        TerminationDecision::Stop
    } else {
        TerminationDecision::Continue
    }
}

fn evaluate_answer_tag(events: &[DaemonEvent]) -> TerminationDecision {
    let mut buf = String::new();
    for e in events {
        if let DaemonEvent::Token { text } = e {
            buf.push_str(text);
        }
        if let DaemonEvent::Stop { .. } = e {
            return TerminationDecision::Stop;
        }
    }
    if buf.contains("<ANSWER>") && buf.contains("</ANSWER>") {
        TerminationDecision::Stop
    } else {
        TerminationDecision::Continue
    }
}

fn evaluate_required_events(
    events: &[DaemonEvent],
    required: &[String],
) -> TerminationDecision {
    let stop_seen = events.iter().any(|e| matches!(e, DaemonEvent::Stop { .. }));
    if !stop_seen {
        return TerminationDecision::Continue;
    }
    let observed: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|e| match e {
            DaemonEvent::DaemonEvent { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let missing: Vec<&str> = required
        .iter()
        .map(|s| s.as_str())
        .filter(|name| !observed.contains(name))
        .collect();
    if missing.is_empty() {
        TerminationDecision::Stop
    } else {
        TerminationDecision::StopWithError(format!(
            "Stop fired but required daemon_events missing: {missing:?}"
        ))
    }
}

#[derive(Clone)]
pub enum AnswerExtractor {
    LastAssistantMessage,
    AnswerTagOrLast,
    EventsOnly,
    Custom(Arc<dyn Fn(&[DaemonEvent]) -> Option<String> + Send + Sync>),
}

impl std::fmt::Debug for AnswerExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LastAssistantMessage => f.write_str("LastAssistantMessage"),
            Self::AnswerTagOrLast => f.write_str("AnswerTagOrLast"),
            Self::EventsOnly => f.write_str("EventsOnly"),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

impl AnswerExtractor {
    pub fn extract(&self, events: &[DaemonEvent]) -> Option<String> {
        match self {
            Self::LastAssistantMessage => extract_last_assistant(events),
            Self::AnswerTagOrLast => extract_answer_tag_or_last(events),
            Self::EventsOnly => None,
            Self::Custom(f) => f(events),
        }
    }
}

fn extract_last_assistant(events: &[DaemonEvent]) -> Option<String> {
    let mut buf = String::new();
    for e in events {
        if let DaemonEvent::Token { text } = e {
            buf.push_str(text);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

fn extract_answer_tag_or_last(events: &[DaemonEvent]) -> Option<String> {
    let buf = extract_last_assistant(events)?;
    if let Some(start) = buf.find("<ANSWER>") {
        let after_open = start + "<ANSWER>".len();
        if let Some(end) = buf[after_open..].find("</ANSWER>") {
            return Some(buf[after_open..after_open + end].trim().to_string());
        }
    }
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(s: &str) -> DaemonEvent { DaemonEvent::Token { text: s.into() } }
    fn stop() -> DaemonEvent { DaemonEvent::Stop { reason: "natural".into() } }
    fn dev(name: &str) -> DaemonEvent {
        DaemonEvent::DaemonEvent { name: name.into(), payload: serde_json::Value::Null }
    }

    #[test]
    fn natural_stop_continues_until_stop() {
        let s = TerminationStrategy::NaturalStop;
        assert_eq!(s.evaluate(&[token("hi")]), TerminationDecision::Continue);
        assert_eq!(s.evaluate(&[token("hi"), stop()]), TerminationDecision::Stop);
    }

    #[test]
    fn answer_tag_finds_closed_pair() {
        let s = TerminationStrategy::AnswerTag;
        assert_eq!(s.evaluate(&[token("<ANSWER>42</ANSWER>")]), TerminationDecision::Stop);
        assert_eq!(s.evaluate(&[token("<ANSWER>partial")]), TerminationDecision::Continue);
        assert_eq!(s.evaluate(&[token("any"), stop()]), TerminationDecision::Stop);
    }

    #[test]
    fn required_events_fail_fast_on_missing() {
        let s = TerminationStrategy::StopWithRequiredEvents(vec![
            "canvas_app_created".into(),
            "canvas_sdk_chat_invoked".into(),
        ]);
        assert_eq!(s.evaluate(&[dev("canvas_app_created")]), TerminationDecision::Continue);
        let all = vec![dev("canvas_app_created"), dev("canvas_sdk_chat_invoked"), stop()];
        assert_eq!(s.evaluate(&all), TerminationDecision::Stop);
        let missing = vec![dev("canvas_app_created"), stop()];
        match s.evaluate(&missing) {
            TerminationDecision::StopWithError(msg) => {
                assert!(msg.contains("canvas_sdk_chat_invoked"));
            }
            other => panic!("expected StopWithError, got {other:?}"),
        }
    }

    #[test]
    fn last_assistant_concatenates_tokens() {
        let ev = vec![token("foo"), token(" "), token("bar"), stop()];
        assert_eq!(
            AnswerExtractor::LastAssistantMessage.extract(&ev),
            Some("foo bar".to_string())
        );
    }

    #[test]
    fn answer_tag_extractor_pulls_content() {
        let ev = vec![token("preamble <ANSWER>42</ANSWER> trailer")];
        assert_eq!(
            AnswerExtractor::AnswerTagOrLast.extract(&ev),
            Some("42".to_string())
        );
    }

    #[test]
    fn answer_tag_extractor_falls_back_to_last() {
        let ev = vec![token("no tag here")];
        assert_eq!(
            AnswerExtractor::AnswerTagOrLast.extract(&ev),
            Some("no tag here".to_string())
        );
    }

    #[test]
    fn events_only_returns_none() {
        let ev = vec![token("anything"), stop()];
        assert_eq!(AnswerExtractor::EventsOnly.extract(&ev), None);
    }
}
