//! Recorded-session replay: the regression net every later kernel change runs
//! against (design spec §3.5).
//!
//! No API key required — the fixtures *are* the recording. Anything that changes
//! the agent loop, the prompt assembly or the tool set has to keep these logs
//! replaying to the same tool sequence, or say why the sequence moved.

use nevoflux_daemon::replay;
use nevoflux_protocol::session_event::SessionEvent;

fn fixture(name: &str) -> Vec<SessionEvent> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    replay::parse_jsonl(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn a_single_tool_session_replays_to_one_call_and_one_result() {
    let events = fixture("single_tool.jsonl");
    assert_eq!(
        replay::tool_sequence(&events),
        vec!["read_file(t1) -> ok".to_string()]
    );
}

#[test]
fn a_two_step_session_replays_tools_in_execution_order() {
    let events = fixture("two_step_tools.jsonl");
    assert_eq!(
        replay::tool_sequence(&events),
        vec![
            "browser_navigate(t1) -> ok".to_string(),
            "browser_get_content(t2) -> error".to_string(),
        ]
    );
}

#[test]
fn a_single_turn_derives_an_input_with_no_history() {
    // Everything after the live user message belongs to the turn being
    // replayed, not to its history — so a one-turn log derives an empty
    // history even though it contains an assistant reply.
    let events = fixture("two_step_tools.jsonl");
    let derived = replay::derive_agent_input(&events).expect("derivable");
    assert_eq!(derived.system_prompt.as_deref(), Some("SYS"));
    assert_eq!(derived.user_message, "open example.com and read it");
    assert!(
        derived.history.is_empty(),
        "single-turn history should be empty, got {:?}",
        derived.history
    );
}

#[test]
fn a_second_turn_carries_the_first_turn_into_history() {
    let events = fixture("two_turns.jsonl");
    let derived = replay::derive_agent_input(&events).expect("derivable");
    assert_eq!(derived.user_message, "now double it");
    assert_eq!(
        derived
            .history
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect::<Vec<_>>(),
        vec![("user", "what is 2+2"), ("assistant", "4")]
    );
    // The system prompt was logged once, on turn 1, and still applies.
    assert_eq!(derived.system_prompt.as_deref(), Some("SYS"));
}

#[test]
fn jsonl_round_trips_through_parse_and_encode() {
    for name in ["single_tool.jsonl", "two_step_tools.jsonl", "two_turns.jsonl"] {
        let events = fixture(name);
        let encoded = replay::to_jsonl(&events).unwrap();
        assert_eq!(
            replay::parse_jsonl(&encoded).unwrap(),
            events,
            "{name}: export must be lossless or session interchange does not hold"
        );
    }
}

#[test]
fn a_truncated_log_replays_the_prefix_it_has() {
    // The `--until` case (spec §3.5): a log cut short still replays up to the cut.
    let all = fixture("two_step_tools.jsonl");
    let prefix: Vec<_> = all.iter().filter(|e| e.seq <= 9).cloned().collect();
    assert_eq!(
        replay::tool_sequence(&prefix),
        vec!["browser_navigate(t1) -> ok".to_string()],
        "only the first tool is inside the prefix"
    );
}

#[test]
fn a_log_cut_between_a_call_and_its_result_shows_the_call_unfinished() {
    // This is why `tool/call` is logged before the tool runs: a run that died
    // mid-tool must still say what it attempted.
    let all = fixture("two_step_tools.jsonl");
    let prefix: Vec<_> = all.iter().filter(|e| e.seq <= 6).cloned().collect();
    assert_eq!(
        replay::tool_sequence(&prefix),
        vec!["browser_navigate(t1) -> (no result)".to_string()]
    );
}

#[test]
fn every_fixture_pairs_its_turn_and_step_boundaries() {
    use nevoflux_protocol::session_event::SessionEventPayload as P;
    for name in ["single_tool.jsonl", "two_step_tools.jsonl", "two_turns.jsonl"] {
        let events = fixture(name);
        let mut turns: i32 = 0;
        let mut steps: i32 = 0;
        for e in &events {
            match e.payload {
                P::TurnStart { .. } => turns += 1,
                P::TurnEnd { .. } => turns -= 1,
                P::StepStart { .. } => steps += 1,
                P::StepEnd { .. } => steps -= 1,
                _ => {}
            }
            assert!(turns >= 0 && steps >= 0, "{name}: boundary closed before it opened");
        }
        assert_eq!(turns, 0, "{name}: unbalanced turn boundaries");
        assert_eq!(steps, 0, "{name}: unbalanced step boundaries");
    }
}
