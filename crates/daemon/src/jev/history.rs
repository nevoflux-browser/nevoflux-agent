//! Earlier turns from the session log (spec §5.2, §5.6.5): with Jev on, the
//! history keeps tool calls and results, each result shown at its graded
//! level. Pure: the log is the input; the full text of every result is in
//! its `tool/result` event, so nothing is read from disk.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use nevoflux_builtin_wasm::{Message, ToolCall};
use nevoflux_protocol::session_event::{SessionEvent, SessionEventPayload};

use super::visibility::{cut_bytes, pseudo_lines, render, Level, Meta, SMALL};

/// Graded chunks re-graded at a rebuild and kept at their grade; older ones
/// are hidden (spec §5.6.4).
pub const CHUNK_WINDOW: usize = 60;
/// The page snapshot the loop appends to a user message; it is the page at
/// that moment, not what the user said.
const SNAPSHOT_MARK: &str = "\n\nCurrent page state:";
/// Head kept of a large result whose full text was never stored.
const UNSTORED_HEAD: usize = 4_000;

/// A chunk's latest grade, from its `jev/visibility` event.
#[derive(Debug, Clone, PartialEq)]
pub struct Grade {
    pub chunk: String,
    pub level: Level,
    pub kept: Vec<Range<usize>>,
    pub graded_by: String,
}

pub struct HistoryOpts {
    /// Whole turns are dropped from the front to stay within this.
    pub max_messages: usize,
    /// The per-result cap a full rendition is cut to.
    pub max_bytes: usize,
}

fn level_of(s: &str) -> Option<Level> {
    match s {
        "hide" => Some(Level::Hide),
        "short" => Some(Level::Short),
        "long" => Some(Level::Long),
        "full" => Some(Level::Full),
        _ => None,
    }
}

/// tool_call_id → latest grade. Events without a call id (logs from before
/// P2-3b) cannot be matched and are skipped.
pub fn grades(events: &[SessionEvent]) -> HashMap<String, Grade> {
    let mut out = HashMap::new();
    for e in events {
        if let SessionEventPayload::JevVisibility {
            id,
            level,
            graded_by,
            call_id: Some(call),
            kept,
            ..
        } = &e.payload
        {
            if let Some(level) = level_of(level) {
                out.insert(
                    call.clone(),
                    Grade {
                        chunk: id.clone(),
                        level,
                        kept: kept.iter().map(|[a, b]| *a as usize..*b as usize).collect(),
                        graded_by: graded_by.clone(),
                    },
                );
            }
        }
    }
    out
}

struct Call {
    /// Id the provider gave the call (assistant message).
    id: String,
    /// Id the result was logged under (`tool/call`), when it differs.
    result_id: Option<String>,
    name: String,
    args: serde_json::Value,
}

impl Call {
    fn rid(&self) -> &str {
        self.result_id.as_deref().unwrap_or(&self.id)
    }
}

struct Step {
    text: String,
    calls: Vec<Call>,
}

struct Turn {
    user: Option<String>,
    steps: Vec<Step>,
    results: HashMap<String, String>,
    spilled: HashSet<String>,
}

/// The turns before `before_turn`, in order.
fn turns(events: &[SessionEvent], before_turn: u32) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    let mut current: Option<Turn> = None;
    for e in events {
        match &e.payload {
            SessionEventPayload::TurnStart { turn } => {
                if let Some(t) = current.take() {
                    out.push(t);
                }
                if *turn >= before_turn {
                    break;
                }
                current = Some(Turn {
                    user: None,
                    steps: Vec::new(),
                    results: HashMap::new(),
                    spilled: HashSet::new(),
                });
            }
            payload => {
                let Some(t) = current.as_mut() else {
                    continue;
                };
                match payload {
                    SessionEventPayload::UserMessage { content, .. } if t.user.is_none() => {
                        let said = content.split(SNAPSHOT_MARK).next().unwrap_or("");
                        t.user = Some(said.to_string());
                    }
                    SessionEventPayload::AssistantMessage {
                        content,
                        tool_calls,
                        ..
                    } => t.steps.push(Step {
                        text: content.clone(),
                        calls: tool_calls
                            .iter()
                            .map(|c| Call {
                                id: c.id.clone(),
                                result_id: None,
                                name: c.name.clone(),
                                args: c.args.clone(),
                            })
                            .collect(),
                    }),
                    SessionEventPayload::ToolCall { id, name, .. } => {
                        if let Some(step) = t.steps.last_mut() {
                            if let Some(c) = step
                                .calls
                                .iter_mut()
                                .find(|c| c.result_id.is_none() && c.name == *name)
                            {
                                c.result_id = Some(id.clone());
                            }
                        }
                    }
                    SessionEventPayload::ToolResult { id, content, .. } => {
                        t.results.insert(id.clone(), content.clone());
                    }
                    SessionEventPayload::ToolSpill { id, .. } => {
                        t.spilled.insert(id.clone());
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(t) = current {
        out.push(t);
    }
    out
}

/// The graded calls of the turns before `before_turn`, newest first, at most
/// [`CHUNK_WINDOW`]: (tool_call_id, grade).
pub fn window(
    events: &[SessionEvent],
    before_turn: u32,
    grades: &HashMap<String, Grade>,
) -> Vec<(String, Grade)> {
    let mut all: Vec<(String, Grade)> = Vec::new();
    for t in turns(events, before_turn) {
        for s in &t.steps {
            for c in &s.calls {
                if let Some(g) = grades.get(c.rid()) {
                    all.push((c.rid().to_string(), g.clone()));
                }
            }
        }
    }
    all.reverse();
    all.truncate(CHUNK_WINDOW);
    all
}

enum Shown {
    /// Kept as a native call/result pair with this content.
    Pair(String),
    /// Folded into the step's assistant text (action log).
    Folded(String),
}

fn show(
    call: &Call,
    content: &str,
    grade: Option<&Grade>,
    in_window: bool,
    spilled: bool,
    max_bytes: usize,
) -> Shown {
    let lines = || pseudo_lines(content);
    match grade {
        Some(g) if !in_window => Shown::Folded(format!(
            "[{} · {} · hidden] recall(\"{}\")",
            g.chunk, call.name, g.chunk
        )),
        Some(g) => {
            let meta = Meta {
                id: &g.chunk,
                tool: &call.name,
                bytes: content.len(),
                graded_by: &g.graded_by,
            };
            match g.level {
                Level::Hide => Shown::Folded(format!(
                    "[{} · {} · hidden] recall(\"{}\")",
                    g.chunk, call.name, g.chunk
                )),
                Level::Short => Shown::Folded(render(
                    Level::Short,
                    content,
                    &lines(),
                    &[],
                    &meta,
                    max_bytes,
                )),
                level => Shown::Pair(render(level, content, &lines(), &g.kept, &meta, max_bytes)),
            }
        }
        None if content.len() <= SMALL => Shown::Pair(content.to_string()),
        None if spilled => {
            let meta = Meta {
                id: call.rid(),
                tool: &call.name,
                bytes: content.len(),
                graded_by: "local",
            };
            Shown::Folded(render(
                Level::Short,
                content,
                &lines(),
                &[],
                &meta,
                max_bytes,
            ))
        }
        None => Shown::Pair(format!(
            "{}\n… [{} bytes, not stored]",
            cut_bytes(content, UNSTORED_HEAD),
            content.len()
        )),
    }
}

/// Earlier turns (all turns before `before_turn`) as LLM messages, tool
/// results shown at their grades; hidden and short ones folded into the
/// step's text so no call is left without its result.
pub fn derive_history(
    events: &[SessionEvent],
    before_turn: u32,
    grades: &HashMap<String, Grade>,
    opts: &HistoryOpts,
) -> Vec<Message> {
    let in_window: HashSet<String> = window(events, before_turn, grades)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut per_turn: Vec<Vec<Message>> = Vec::new();
    for t in turns(events, before_turn) {
        let mut msgs = Vec::new();
        if let Some(u) = &t.user {
            msgs.push(Message::user(u.clone()));
        }
        for s in &t.steps {
            let mut text = s.text.clone();
            let mut calls = Vec::new();
            let mut tools = Vec::new();
            for c in &s.calls {
                let Some(content) = t.results.get(c.rid()) else {
                    continue;
                };
                let shown = show(
                    c,
                    content,
                    grades.get(c.rid()),
                    in_window.contains(c.rid()),
                    t.spilled.contains(c.rid()),
                    opts.max_bytes,
                );
                match shown {
                    Shown::Pair(body) => {
                        calls.push(ToolCall {
                            id: c.id.clone(),
                            call_id: c.result_id.clone().filter(|r| *r != c.id),
                            name: c.name.clone(),
                            arguments: c.args.clone(),
                            signature: None,
                        });
                        tools.push(Message::tool(c.rid().to_string(), body));
                    }
                    Shown::Folded(line) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&line);
                    }
                }
            }
            if text.is_empty() && calls.is_empty() {
                continue;
            }
            msgs.push(Message::assistant_with_tool_calls_and_reasoning(
                text, calls, None,
            ));
            msgs.extend(tools);
        }
        per_turn.push(msgs);
    }
    let mut kept: Vec<Vec<Message>> = Vec::new();
    let mut count = 0;
    for msgs in per_turn.into_iter().rev() {
        if count + msgs.len() > opts.max_messages {
            break;
        }
        count += msgs.len();
        kept.push(msgs);
    }
    kept.into_iter().rev().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_builtin_wasm::MessageRole;
    use serde_json::json;

    fn log(lines: Vec<serde_json::Value>) -> Vec<SessionEvent> {
        lines
            .into_iter()
            .enumerate()
            .map(|(i, v)| SessionEvent {
                seq: i as i64 + 1,
                ts: 1_790_000_000_000 + i as i64,
                payload: serde_json::from_value(v).unwrap(),
            })
            .collect()
    }

    fn turn(n: u32) -> serde_json::Value {
        json!({"type": "turn/start", "turn": n})
    }
    fn user(text: &str) -> serde_json::Value {
        json!({"type": "user/message", "content": text, "origin": "user"})
    }
    fn assistant(text: &str, calls: &[(&str, &str)]) -> serde_json::Value {
        let calls: Vec<_> = calls
            .iter()
            .map(|(id, name)| json!({"id": id, "name": name, "args": {}}))
            .collect();
        json!({"type": "assistant/message", "content": text, "tool_calls": calls, "model": "m", "provider": "p"})
    }
    fn call(id: &str, name: &str) -> serde_json::Value {
        json!({"type": "tool/call", "id": id, "name": name, "args": {}, "origin": "model"})
    }
    fn result(id: &str, content: &str) -> serde_json::Value {
        json!({"type": "tool/result", "id": id, "content": content, "is_error": false, "duration_ms": 1})
    }
    fn graded(call_id: &str, chunk: &str, level: &str, kept: &[[u64; 2]]) -> serde_json::Value {
        json!({"type": "jev/visibility", "id": chunk, "tool": "read", "bytes": 9000, "level": level,
               "graded_by": "jev", "kept_lines": 0, "elapsed_ms": 1, "call_id": call_id, "kept": kept})
    }
    fn spill(id: &str) -> serde_json::Value {
        json!({"type": "tool/spill", "id": id, "path": format!("/s/{id}.txt"), "bytes": 9000})
    }
    fn hundred_lines() -> String {
        (0..100)
            .map(|i| format!("{:<59}\n", format!("line {i}")))
            .collect()
    }
    fn opts() -> HistoryOpts {
        HistoryOpts {
            max_messages: 50,
            max_bytes: 32_000,
        }
    }
    fn derive(events: &[SessionEvent], before: u32) -> Vec<Message> {
        derive_history(events, before, &grades(events), &opts())
    }
    fn roles(ms: &[Message]) -> Vec<&'static str> {
        ms.iter()
            .map(|m| match m.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
                MessageRole::System => "system",
            })
            .collect()
    }

    #[test]
    fn a_turn_becomes_user_assistant_tool_messages() {
        let ev = log(vec![
            turn(1),
            user("read a.txt"),
            assistant("", &[("t1", "read")]),
            call("t1", "read"),
            result("t1", "small"),
            assistant("it says small", &[]),
            turn(2),
        ]);
        let h = derive(&ev, 2);
        assert_eq!(roles(&h), vec!["user", "assistant", "tool", "assistant"]);
        assert_eq!(h[1].tool_calls[0].id, "t1");
        assert_eq!(h[2].tool_call_id.as_deref(), Some("t1"));
        assert_eq!(h[2].content, "small");
        assert_eq!(h[3].content, "it says small");
    }

    #[test]
    fn the_current_turn_is_not_history() {
        let ev = log(vec![
            turn(1),
            user("one"),
            assistant("a1", &[]),
            turn(2),
            user("two"),
        ]);
        let h = derive(&ev, 2);
        assert_eq!(roles(&h), vec!["user", "assistant"]);
        assert_eq!(h[0].content, "one");
    }

    #[test]
    fn repeated_user_messages_in_a_turn_are_one_and_lose_the_page_snapshot() {
        let ev = log(vec![
            turn(1),
            user("go\n\nCurrent page state:\n# Shop\n[e0] button"),
            assistant("", &[("t1", "think")]),
            call("t1", "think"),
            result("t1", "ok"),
            user("go\n\nCurrent page state:\n# Shop\n[e0] button"),
            assistant("done", &[]),
            turn(2),
        ]);
        let h = derive(&ev, 2);
        assert_eq!(
            h.iter()
                .filter(|m| matches!(m.role, MessageRole::User))
                .count(),
            1
        );
        assert_eq!(h[0].content, "go");
    }

    #[test]
    fn a_long_grade_is_re_rendered_from_the_logged_text() {
        let full = hundred_lines();
        let ev = log(vec![
            turn(1),
            user("find line 30"),
            assistant("", &[("t1", "read")]),
            call("t1", "read"),
            result("t1", &full),
            spill("cA"),
            graded("t1", "cA", "long", &[[25, 50]]),
            assistant("found", &[]),
            turn(2),
        ]);
        let h = derive(&ev, 2);
        let tool = h
            .iter()
            .find(|m| matches!(m.role, MessageRole::Tool))
            .unwrap();
        assert!(tool.content.contains("line 25") && tool.content.contains("line 49"));
        assert!(!tool.content.contains("line 50 "));
        assert!(tool.content.contains("recall(\"cA\")"));
    }

    #[test]
    fn hidden_and_short_fold_into_the_assistant_text() {
        let full = hundred_lines();
        let ev = log(vec![
            turn(1),
            user("q"),
            assistant("looking", &[("t1", "read"), ("t2", "read")]),
            call("t1", "read"),
            result("t1", &full),
            graded("t1", "cH", "hide", &[]),
            call("t2", "read"),
            result("t2", &full),
            graded("t2", "cF", "full", &[]),
            assistant("done", &[]),
            turn(2),
        ]);
        let h = derive(&ev, 2);
        assert_eq!(h[1].tool_calls.len(), 1);
        assert_eq!(h[1].tool_calls[0].id, "t2");
        assert!(h[1].content.contains("looking"));
        assert!(
            h[1].content.contains("· hidden] recall(\"cH\")"),
            "{}",
            h[1].content
        );
        assert_eq!(
            h.iter()
                .filter(|m| matches!(m.role, MessageRole::Tool))
                .count(),
            1
        );
    }

    #[test]
    fn a_partly_hidden_step_keeps_its_pairs_matched() {
        let full = hundred_lines();
        let ev = log(vec![
            turn(1),
            user("q"),
            assistant("", &[("a", "read"), ("b", "read"), ("c", "read")]),
            call("a", "read"),
            result("a", &full),
            graded("a", "c1", "hide", &[]),
            call("b", "read"),
            result("b", &full),
            graded("b", "c2", "short", &[]),
            call("c", "read"),
            result("c", &full),
            graded("c", "c3", "long", &[[0, 25]]),
            assistant("done", &[]),
            turn(2),
        ]);
        let h = derive(&ev, 2);
        let calls: Vec<String> = h
            .iter()
            .flat_map(|m| m.tool_calls.iter().map(|c| c.id.clone()))
            .collect();
        let results: Vec<String> = h.iter().filter_map(|m| m.tool_call_id.clone()).collect();
        assert_eq!(calls, vec!["c".to_string()]);
        assert_eq!(results, calls, "every call has exactly its result");
        assert!(h[1].content.contains("· short]"), "{}", h[1].content);
    }

    #[test]
    fn turns_without_grades_use_the_local_rule() {
        let small = "s".repeat(3_000);
        let big = (0..1000).map(|i| format!("row {i}\n")).collect::<String>();
        let ev = log(vec![
            turn(1),
            user("q"),
            assistant("", &[("a", "read"), ("b", "read"), ("c", "read")]),
            call("a", "read"),
            result("a", &small),
            call("b", "read"),
            result("b", &big),
            spill("b"),
            call("c", "read"),
            result("c", &big),
            assistant("done", &[]),
            turn(2),
        ]);
        let h = derive(&ev, 2);
        let tools: Vec<&Message> = h
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        let by_id = |id: &str| {
            tools
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(id))
                .map(|m| m.content.clone())
        };
        assert_eq!(by_id("a").unwrap(), small);
        // b: stored on disk → local short rule with recall; folded like any short
        assert!(by_id("b").is_none());
        let a_text = &h[1].content;
        assert!(a_text.contains("recall(\"b\")"), "{a_text}");
        // c: not stored → a head and "not stored", no recall
        let c = by_id("c").expect("c kept as a pair");
        assert!(c.contains("not stored") && !c.contains("recall("), "{c}");
        assert!(c.len() <= 4_200);
    }

    #[test]
    fn older_graded_chunks_beyond_the_window_are_hidden() {
        let full = hundred_lines();
        let mut lines = vec![turn(1), user("q")];
        for i in 0..65 {
            let id = format!("t{i}");
            lines.push(assistant("", &[(&id, "read")]));
            lines.push(call(&id, "read"));
            lines.push(result(&id, &full));
            lines.push(graded(&id, &format!("c{i}"), "full", &[]));
        }
        lines.push(assistant("done", &[]));
        lines.push(turn(2));
        let ev = log(lines);
        let g = grades(&ev);
        let w = window(&ev, 2, &g);
        assert_eq!(w.len(), CHUNK_WINDOW);
        assert_eq!(w[0].0, "t64");
        let h = derive_history(
            &ev,
            2,
            &g,
            &HistoryOpts {
                max_messages: 1_000,
                max_bytes: 32_000,
            },
        );
        let kept: Vec<String> = h.iter().filter_map(|m| m.tool_call_id.clone()).collect();
        assert_eq!(kept.len(), CHUNK_WINDOW);
        assert!(!kept.contains(&"t0".to_string()) && kept.contains(&"t5".to_string()));
    }

    #[test]
    fn a_long_session_is_bounded() {
        let mut lines = vec![];
        for t in 1..=300u32 {
            lines.push(turn(t));
            lines.push(user(&format!("q{t}")));
            lines.push(assistant(&format!("a{t}"), &[]));
        }
        lines.push(turn(301));
        let ev = log(lines);
        let h = derive(&ev, 301);
        assert!(h.len() <= 50);
        assert!(matches!(h[0].role, MessageRole::User));
        assert_eq!(h.last().unwrap().content, "a300");
    }
}
