//! Acceptance for design spec §3.6: a session can be exported and replayed to
//! the same tool sequence.
//!
//! Goes through the real storage layer and the same `to_jsonl` the
//! `session export` subcommand uses, so it covers the path a user actually
//! takes rather than only the in-memory helpers.

use nevoflux_daemon::replay;
use nevoflux_protocol::session_event::{
    LoggedToolCall, SessionEventPayload as P, TokenUsage, ToolOrigin,
};
use nevoflux_storage::Storage;

/// Append a realistic two-step run to `session_id`.
fn record_a_run(storage: &Storage, session_id: &str) {
    let repo = storage.session_events();
    let mut append = |p: P| {
        repo.append(session_id, &p).expect("append");
    };

    append(P::TurnStart { turn: 1 });
    append(P::SystemMessage {
        content: "You are NevoFlux.".into(),
        sections: vec![],
        origin: "kernel".into(),
    });
    append(P::UserMessage {
        content: "open example.com and tell me the title".into(),
        attachments: vec![],
        origin: "user".into(),
    });
    append(P::RequestHeader {
        provider: "anthropic".into(),
        model: "claude-opus-5".into(),
        tools_hash: "0123456789abcdef".into(),
        reason: "initial".into(),
    });

    append(P::StepStart { step: 0, turn: 1 });
    append(P::AssistantMessage {
        content: String::new(),
        tool_calls: vec![LoggedToolCall {
            id: "t1".into(),
            name: "browser_navigate".into(),
            args: serde_json::json!({ "url": "https://example.com" }),
        }],
        usage: Some(TokenUsage {
            input_tokens: Some(1200),
            output_tokens: Some(40),
            cache_read_tokens: None,
            cache_write_tokens: None,
        }),
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
    });
    append(P::ToolCall {
        id: "t1".into(),
        name: "browser_navigate".into(),
        args: serde_json::json!({ "url": "https://example.com" }),
        origin: ToolOrigin::model(),
        tab_url: None,
    });
    append(P::ToolResult {
        id: "t1".into(),
        content: "navigated".into(),
        is_error: false,
        duration_ms: 143,
    });
    append(P::StepEnd { step: 0, turn: 1 });

    append(P::StepStart { step: 1, turn: 1 });
    append(P::ToolCall {
        id: "t2".into(),
        name: "browser_get_content".into(),
        args: serde_json::json!({}),
        origin: ToolOrigin::model(),
        tab_url: Some("https://example.com/".into()),
    });
    append(P::ToolResult {
        id: "t2".into(),
        content: "Example Domain".into(),
        is_error: false,
        duration_ms: 91,
    });
    append(P::AssistantMessage {
        content: "The title is \"Example Domain\".".into(),
        tool_calls: vec![],
        usage: None,
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
    });
    append(P::StepEnd { step: 1, turn: 1 });
    append(P::TurnEnd { turn: 1 });
}

#[test]
fn a_stored_session_exports_and_replays_to_the_same_tool_sequence() {
    let storage = Storage::open_in_memory().unwrap();
    record_a_run(&storage, "sess-accept");

    let from_db = storage.session_events().list("sess-accept").unwrap();
    let direct = replay::tool_sequence(&from_db);

    // The export/import round trip the `session export` subcommand performs.
    let jsonl = replay::to_jsonl(&from_db).unwrap();
    let reimported = replay::parse_jsonl(&jsonl).unwrap();
    let after_roundtrip = replay::tool_sequence(&reimported);

    assert_eq!(
        direct,
        vec![
            "browser_navigate(t1) -> ok".to_string(),
            "browser_get_content(t2) -> ok".to_string(),
        ]
    );
    assert_eq!(
        after_roundtrip, direct,
        "export -> replay must reproduce the tool sequence (spec §3.6)"
    );
}

#[test]
fn an_export_round_trip_preserves_every_event_byte_for_byte() {
    let storage = Storage::open_in_memory().unwrap();
    record_a_run(&storage, "sess-accept");

    let from_db = storage.session_events().list("sess-accept").unwrap();
    let reimported = replay::parse_jsonl(&replay::to_jsonl(&from_db).unwrap()).unwrap();

    assert_eq!(
        reimported, from_db,
        "a lossy export would break session interchange with dsh-cloud (ADR A3)"
    );
}

#[test]
fn a_stored_session_derives_the_input_that_produced_it() {
    let storage = Storage::open_in_memory().unwrap();
    record_a_run(&storage, "sess-accept");

    let events = storage.session_events().list("sess-accept").unwrap();
    let derived = replay::derive_agent_input(&events).expect("derivable");

    assert_eq!(derived.system_prompt.as_deref(), Some("You are NevoFlux."));
    assert_eq!(
        derived.user_message,
        "open example.com and tell me the title"
    );
    // Single turn: the assistant replies belong to this turn, not to its history.
    assert!(derived.history.is_empty());
}

#[test]
fn replaying_a_prefix_stops_where_the_log_was_cut() {
    let storage = Storage::open_in_memory().unwrap();
    record_a_run(&storage, "sess-accept");

    // The `--until` path: `list_until` is what the subcommand calls.
    let cut = storage
        .session_events()
        .list_until("sess-accept", 9)
        .unwrap();
    assert_eq!(
        replay::tool_sequence(&cut),
        vec!["browser_navigate(t1) -> ok".to_string()],
        "only the first tool is inside the prefix"
    );
}

#[test]
fn two_sessions_in_one_database_do_not_bleed_into_each_other() {
    let storage = Storage::open_in_memory().unwrap();
    record_a_run(&storage, "sess-a");
    record_a_run(&storage, "sess-b");

    let a = storage.session_events().list("sess-a").unwrap();
    let b = storage.session_events().list("sess-b").unwrap();

    assert_eq!(a.len(), b.len());
    assert_eq!(a.first().unwrap().seq, 1, "each session numbers from 1");
    assert_eq!(b.first().unwrap().seq, 1);
    assert_eq!(replay::tool_sequence(&a), replay::tool_sequence(&b));
}
