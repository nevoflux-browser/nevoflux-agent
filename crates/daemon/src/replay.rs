//! Replay a session event log (design spec §3.5).
//!
//! Replay is deliberately *derivational*, not executional: it rebuilds what the
//! kernel would have been asked to do from the log alone. That is what makes the
//! fixtures keyless — no provider is contacted, so the regression net runs
//! anywhere, including CI without secrets.

use nevoflux_protocol::session_event::{SessionEvent, SessionEventPayload};

/// A message as reconstructed from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// Message body.
    pub content: String,
}

/// The agent input a log implies: the prompt, the history and the live message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedInput {
    /// System prompt in force at the end of the replayed range.
    pub system_prompt: Option<String>,
    /// Everything before the last user message.
    pub history: Vec<DerivedMessage>,
    /// The last user message in the range.
    pub user_message: String,
}

/// Parse JSONL into events, reporting the offending line number on failure.
pub fn parse_jsonl(text: &str) -> Result<Vec<SessionEvent>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: SessionEvent =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        out.push(ev);
    }
    Ok(out)
}

/// Encode events back to JSONL, one object per line.
///
/// Round-trips with [`parse_jsonl`]; that is the property that makes a session
/// portable between the local kernel and dsh-cloud rather than merely readable.
pub fn to_jsonl(events: &[SessionEvent]) -> Result<String, String> {
    let mut buf = String::new();
    for ev in events {
        buf.push_str(&serde_json::to_string(ev).map_err(|e| e.to_string())?);
        buf.push('\n');
    }
    Ok(buf)
}

/// The tool calls a log implies, in execution order, each paired with its result.
///
/// A call with no matching result renders as `(no result)`, which is exactly what
/// a run that died mid-tool should look like — the point of logging the call
/// before executing it.
pub fn tool_sequence(events: &[SessionEvent]) -> Vec<String> {
    let mut out = Vec::new();
    for ev in events {
        if let SessionEventPayload::ToolCall { id, name, .. } = &ev.payload {
            let outcome = events
                .iter()
                .find_map(|e| match &e.payload {
                    SessionEventPayload::ToolResult {
                        id: rid, is_error, ..
                    } if rid == id => Some(if *is_error { "error" } else { "ok" }),
                    _ => None,
                })
                .unwrap_or("(no result)");
            out.push(format!("{name}({id}) -> {outcome}"));
        }
    }
    out
}

/// Rebuild the agent input a log implies, or `None` if it holds no user message.
pub fn derive_agent_input(events: &[SessionEvent]) -> Option<DerivedInput> {
    let mut system_prompt = None;
    let mut msgs: Vec<DerivedMessage> = Vec::new();

    for ev in events {
        match &ev.payload {
            SessionEventPayload::SystemMessage { content, .. } => {
                system_prompt = Some(content.clone());
            }
            SessionEventPayload::UserMessage { content, .. } => msgs.push(DerivedMessage {
                role: "user".into(),
                content: content.clone(),
            }),
            SessionEventPayload::AssistantMessage { content, .. } => msgs.push(DerivedMessage {
                role: "assistant".into(),
                content: content.clone(),
            }),
            _ => {}
        }
    }

    let last_user_idx = msgs.iter().rposition(|m| m.role == "user")?;
    let user_message = msgs[last_user_idx].content.clone();
    msgs.truncate(last_user_idx);
    Some(DerivedInput {
        system_prompt,
        history: msgs,
        user_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_protocol::session_event::ToolOrigin;

    fn ev(seq: i64, payload: SessionEventPayload) -> SessionEvent {
        SessionEvent {
            seq,
            ts: 1_757_000_000_000 + seq,
            payload,
        }
    }

    #[test]
    fn a_call_without_a_result_reads_as_unfinished() {
        let events = vec![ev(
            1,
            SessionEventPayload::ToolCall {
                id: "t1".into(),
                name: "browser_click".into(),
                args: serde_json::json!({}),
                origin: ToolOrigin::model(),
                tab_url: None,
            },
        )];
        assert_eq!(
            tool_sequence(&events),
            vec!["browser_click(t1) -> (no result)".to_string()]
        );
    }

    #[test]
    fn deriving_an_input_without_a_user_message_yields_nothing() {
        let events = vec![ev(
            1,
            SessionEventPayload::SystemMessage {
                content: "SYS".into(),
                sections: vec![],
                origin: "kernel".into(),
            },
        )];
        assert!(derive_agent_input(&events).is_none());
    }

    #[test]
    fn the_latest_system_message_wins_when_the_prompt_was_replaced() {
        let events = vec![
            ev(
                1,
                SessionEventPayload::SystemMessage {
                    content: "OLD".into(),
                    sections: vec![],
                    origin: "kernel".into(),
                },
            ),
            ev(
                2,
                SessionEventPayload::SystemMessage {
                    content: "NEW".into(),
                    sections: vec![],
                    origin: "pack:jobhunt".into(),
                },
            ),
            ev(
                3,
                SessionEventPayload::UserMessage {
                    content: "hi".into(),
                    attachments: vec![],
                    origin: "user".into(),
                },
            ),
        ];
        let d = derive_agent_input(&events).unwrap();
        assert_eq!(d.system_prompt.as_deref(), Some("NEW"));
        assert!(d.history.is_empty());
        assert_eq!(d.user_message, "hi");
    }

    #[test]
    fn jsonl_round_trips_through_encode_and_parse() {
        let events = vec![
            ev(1, SessionEventPayload::TurnStart { turn: 1 }),
            ev(
                2,
                SessionEventPayload::ToolResult {
                    id: "t1".into(),
                    content: "body".into(),
                    is_error: true,
                    duration_ms: 12,
                },
            ),
        ];
        let text = to_jsonl(&events).unwrap();
        assert_eq!(parse_jsonl(&text).unwrap(), events);
    }

    #[test]
    fn a_malformed_line_names_its_line_number() {
        let err =
            parse_jsonl("{\"seq\":1,\"ts\":0,\"type\":\"turn/start\",\"turn\":1}\nnot json\n")
                .unwrap_err();
        assert!(err.starts_with("line 2:"), "unexpected error: {err}");
    }
}
