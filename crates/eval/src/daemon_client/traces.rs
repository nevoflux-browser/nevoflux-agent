//! Parse the `/_eval/sessions/:id/traces` JSONL output.
//!
//! Each line is a `TraceSpanRecord`-shaped JSON object (see daemon's
//! `crates/storage/src/repositories/traces.rs::TraceSpanRecord`). We
//! only need a couple of fields here.

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
pub struct TraceEntry {
    pub id: i64,
    pub session_id: String,
    pub span_type: String,
    pub tool_name: Option<String>,
}

#[derive(Debug, Error)]
pub enum TracesParseError {
    #[error("malformed trace line: {0}")]
    Malformed(String),
}

/// Parse the JSONL body into TraceEntry rows. Lines that fail to parse are
/// skipped with a warning rather than aborting the whole result — defensive
/// behavior because the wire format may evolve.
pub fn parse_jsonl(body: &str) -> Vec<TraceEntry> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<TraceEntry>(l) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(line = %l, error = %e, "skipping malformed trace line");
                None
            }
        })
        .collect()
}

/// Extract daemon_event names from a list of trace entries. Daemon emits
/// `daemon_event` rows with `span_type = "daemon_event"` and `tool_name` set
/// to the event name.
pub fn event_names(entries: &[TraceEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.span_type == "daemon_event")
        .filter_map(|e| e.tool_name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonl_lines() {
        let body = "{\"id\":1,\"session_id\":\"s\",\"span_type\":\"daemon_event\",\"tool_name\":\"canvas_app_created\"}\n\
                    {\"id\":2,\"session_id\":\"s\",\"span_type\":\"llm_call\",\"tool_name\":null}\n";
        let entries = parse_jsonl(body);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 1);
    }

    #[test]
    fn extracts_event_names() {
        let body = "{\"id\":1,\"session_id\":\"s\",\"span_type\":\"daemon_event\",\"tool_name\":\"canvas_app_created\"}\n\
                    {\"id\":2,\"session_id\":\"s\",\"span_type\":\"llm_call\",\"tool_name\":null}\n\
                    {\"id\":3,\"session_id\":\"s\",\"span_type\":\"daemon_event\",\"tool_name\":\"tool_call_blocked\"}\n";
        let entries = parse_jsonl(body);
        let names = event_names(&entries);
        assert_eq!(names, vec!["canvas_app_created", "tool_call_blocked"]);
    }

    #[test]
    fn skips_malformed_lines() {
        let body = "not json\n\
                    {\"id\":1,\"session_id\":\"s\",\"span_type\":\"daemon_event\",\"tool_name\":\"x\"}\n";
        let entries = parse_jsonl(body);
        assert_eq!(entries.len(), 1);
    }
}
