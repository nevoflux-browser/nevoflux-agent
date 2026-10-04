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
/// The provider name an on-device turn is logged under.
const LOCAL_PROVIDER: &str = "local";

/// A chunk's latest grade, from its `jev/visibility` events.
#[derive(Debug, Clone, PartialEq)]
pub struct Grade {
    pub chunk: String,
    pub level: Level,
    pub kept: Vec<Range<usize>>,
    pub graded_by: String,
    /// URLs of the pages the chunk may come from (empty: no page).
    pub pages: Vec<String>,
}

/// What the messages table knows about an earlier turn: the user's own
/// words (the log holds the request as sent, with the turn's injections)
/// and, when another soul answered, its display name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableTurn {
    pub user: String,
    pub other_speaker: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryOpts {
    /// Turns that do not fit whole are condensed to their question and
    /// answer; condensed turns that do not fit are dropped from the front.
    pub max_messages: usize,
    /// The per-result cap a full rendition is cut to.
    pub max_bytes: usize,
    /// No native tool pairs (providers that need each assistant tool-call
    /// message's reasoning back, which the log does not keep).
    pub text_only: bool,
    /// Earlier turns from the messages table, oldest first.
    pub table: Vec<TableTurn>,
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

/// chunk id → latest grade. A re-grade keeps the chunk id and its pages.
/// Events without a call id (logs from before P2-3b) cannot be placed and
/// are skipped.
pub fn grades(events: &[SessionEvent]) -> HashMap<String, Grade> {
    let mut out: HashMap<String, Grade> = HashMap::new();
    for e in events {
        if let SessionEventPayload::JevVisibility {
            id,
            level,
            graded_by,
            call_id: Some(_),
            kept,
            pages,
            ..
        } = &e.payload
        {
            if let Some(level) = level_of(level) {
                let pages = if pages.is_empty() {
                    out.get(id).map(|g| g.pages.clone()).unwrap_or_default()
                } else {
                    pages.clone()
                };
                out.insert(
                    id.clone(),
                    Grade {
                        chunk: id.clone(),
                        level,
                        kept: kept.iter().map(|[a, b]| *a as usize..*b as usize).collect(),
                        graded_by: graded_by.clone(),
                        pages,
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
    content: Option<String>,
    /// The chunk the result was stored under, when it was graded.
    chunk: Option<String>,
    spilled: bool,
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
    /// Answered on-device: its tool results never leave the machine.
    local: bool,
}

impl Turn {
    /// The first call logged under `id` that `taken` says is still open.
    /// Ids are not unique (Gemini uses the function name), so events are
    /// matched to calls in order, not by id alone.
    fn open_call(&mut self, id: &str, taken: impl Fn(&Call) -> bool) -> Option<&mut Call> {
        self.steps
            .iter_mut()
            .flat_map(|s| s.calls.iter_mut())
            .find(|c| c.rid() == id && !taken(c))
    }
}

/// Which events belong to a nested run: a subagent's host shares the
/// session, so its turn is logged inside the parent's open tool call. A
/// `turn/start` while a call waits for its result (or inside a nested run)
/// opens one; its `turn/end` closes it.
pub(crate) fn nested(events: &[SessionEvent]) -> Vec<bool> {
    let mut out = Vec::with_capacity(events.len());
    let mut depth = 0u32;
    let mut open: Vec<&str> = Vec::new();
    for e in events {
        match &e.payload {
            SessionEventPayload::TurnStart { .. } if depth > 0 || !open.is_empty() => {
                depth += 1;
                out.push(true);
            }
            SessionEventPayload::TurnStart { .. } => {
                open.clear();
                out.push(false);
            }
            SessionEventPayload::TurnEnd { .. } if depth > 0 => {
                depth -= 1;
                out.push(true);
            }
            _ if depth > 0 => out.push(true),
            payload => {
                match payload {
                    SessionEventPayload::ToolCall { id, .. } => open.push(id),
                    SessionEventPayload::ToolResult { id, .. } => {
                        if let Some(i) = open.iter().position(|o| *o == id.as_str()) {
                            open.remove(i);
                        }
                    }
                    _ => {}
                }
                out.push(false);
            }
        }
    }
    out
}

/// Index of the current (last) turn's `turn/start`, nested runs aside.
pub fn current_turn_start(events: &[SessionEvent]) -> Option<usize> {
    let nested = nested(events);
    events.iter().enumerate().rposition(|(i, e)| {
        !nested[i] && matches!(e.payload, SessionEventPayload::TurnStart { .. })
    })
}

/// The turns before `before_turn`, in order.
fn turns(events: &[SessionEvent], before_turn: u32) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    let mut current: Option<Turn> = None;
    let nested = nested(events);
    for (e, _) in events.iter().zip(&nested).filter(|(_, n)| !**n) {
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
                    local: false,
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
                        provider,
                        ..
                    } => {
                        t.local |= provider == LOCAL_PROVIDER;
                        t.steps.push(Step {
                            text: content.clone(),
                            calls: tool_calls
                                .iter()
                                .map(|c| Call {
                                    id: c.id.clone(),
                                    result_id: None,
                                    name: c.name.clone(),
                                    args: c.args.clone(),
                                    content: None,
                                    chunk: None,
                                    spilled: false,
                                })
                                .collect(),
                        })
                    }
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
                        if let Some(c) = t.open_call(id, |c| c.content.is_some()) {
                            c.content = Some(content.clone());
                        }
                    }
                    SessionEventPayload::ToolSpill { id, .. } => {
                        if let Some(c) = t.open_call(id, |c| c.spilled) {
                            c.spilled = true;
                        }
                    }
                    // A re-grade names its chunk; only the first grade places
                    // a chunk on its call.
                    SessionEventPayload::JevVisibility {
                        id,
                        call_id: Some(call),
                        graded_by,
                        ..
                    } if graded_by != "rebuild" => {
                        if let Some(c) = t.open_call(call, |c| c.chunk.is_some()) {
                            c.chunk = Some(id.clone());
                        }
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

/// One graded result of an earlier turn.
#[derive(Debug, Clone)]
pub struct WindowChunk {
    pub call_id: String,
    pub tool: String,
    pub grade: Grade,
    pub content: String,
}

/// The graded results of the turns before `before_turn`, newest first, at
/// most [`CHUNK_WINDOW`]. On-device turns are not in it.
pub fn window(
    events: &[SessionEvent],
    before_turn: u32,
    grades: &HashMap<String, Grade>,
) -> Vec<WindowChunk> {
    let mut all: Vec<WindowChunk> = Vec::new();
    for t in turns(events, before_turn) {
        if t.local {
            continue;
        }
        for s in &t.steps {
            for c in &s.calls {
                let (Some(chunk), Some(content)) = (&c.chunk, &c.content) else {
                    continue;
                };
                if let Some(g) = grades.get(chunk) {
                    all.push(WindowChunk {
                        call_id: c.rid().to_string(),
                        tool: c.name.clone(),
                        grade: g.clone(),
                        content: content.clone(),
                    });
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
        None if call.spilled => {
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

/// A turn with its tool calls and results, results at their grades.
fn whole(
    t: &Turn,
    user: Option<&str>,
    grades: &HashMap<String, Grade>,
    in_window: &HashSet<String>,
    opts: &HistoryOpts,
) -> Vec<Message> {
    let mut msgs = Vec::new();
    if let Some(u) = user {
        msgs.push(Message::user(u.to_string()));
    }
    for s in &t.steps {
        let mut text = s.text.clone();
        let mut calls = Vec::new();
        let mut tools = Vec::new();
        for c in &s.calls {
            let Some(content) = &c.content else {
                continue;
            };
            let grade = c.chunk.as_ref().and_then(|k| grades.get(k));
            let in_window = c.chunk.as_ref().is_some_and(|k| in_window.contains(k));
            let mut fold = |line: &str| {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(line);
            };
            match show(c, content, grade, in_window, opts.max_bytes) {
                Shown::Pair(body) if opts.text_only => {
                    fold(&format!("[{} result]\n{body}", c.name));
                }
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
                Shown::Folded(line) => fold(&line),
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
    msgs
}

/// A turn as its question and final answer, as the messages table had it.
/// Another soul's answer is attributed as a user line, as
/// `convert_history_messages` does.
fn condensed(t: &Turn, user: Option<&str>, other_speaker: Option<&str>) -> Vec<Message> {
    let mut msgs = Vec::new();
    if let Some(u) = user {
        msgs.push(Message::user(u.to_string()));
    }
    let answer = t
        .steps
        .iter()
        .rev()
        .map(|s| s.text.as_str())
        .find(|s| !s.trim().is_empty());
    if let Some(a) = answer {
        msgs.push(match other_speaker {
            Some(name) => Message::user(format!("[{name}] {a}")),
            None => Message::assistant(a.to_string()),
        });
    }
    msgs
}

/// Each turn's entry in the messages table, matched newest first: a turn's
/// logged request contains the user's own words.
fn match_table<'a>(turns: &[Turn], table: &'a [TableTurn]) -> Vec<Option<&'a TableTurn>> {
    let mut out = vec![None; turns.len()];
    let mut rest = table.len();
    for (i, t) in turns.iter().enumerate().rev() {
        let Some(logged) = &t.user else { continue };
        if let Some(j) = (0..rest)
            .rev()
            .find(|&j| !table[j].user.is_empty() && logged.contains(&table[j].user))
        {
            out[i] = Some(&table[j]);
            rest = j;
        }
    }
    out
}

/// Earlier turns (all turns before `before_turn`) as LLM messages, tool
/// results shown at their grades; hidden and short ones folded into the
/// step's text so no call is left without its result. Newest turns are kept
/// whole while they fit; from the first that does not, turns are condensed
/// to question and answer, so the latest turns are never lost.
pub fn derive_history(
    events: &[SessionEvent],
    before_turn: u32,
    grades: &HashMap<String, Grade>,
    opts: &HistoryOpts,
) -> Vec<Message> {
    let in_window: HashSet<String> = window(events, before_turn, grades)
        .into_iter()
        .map(|w| w.grade.chunk)
        .collect();
    let all = turns(events, before_turn);
    let table = match_table(&all, &opts.table);
    let mut kept: Vec<Vec<Message>> = Vec::new();
    let mut count = 0;
    let mut whole_ok = true;
    for (t, row) in all.iter().zip(&table).rev() {
        let user = row.map(|r| r.user.as_str()).or(t.user.as_deref());
        let other = row.and_then(|r| r.other_speaker.as_deref());
        let mut msgs = Vec::new();
        if whole_ok && !t.local && other.is_none() {
            msgs = whole(t, user, grades, &in_window, opts);
            if count + msgs.len() > opts.max_messages {
                whole_ok = false;
            }
        }
        if msgs.is_empty() || count + msgs.len() > opts.max_messages {
            msgs = condensed(t, user, other);
        }
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
            ..HistoryOpts::default()
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
        assert_eq!(w[0].call_id, "t64");
        let h = derive_history(
            &ev,
            2,
            &g,
            &HistoryOpts {
                max_messages: 1_000,
                max_bytes: 32_000,
                ..HistoryOpts::default()
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

    fn assistant_by(provider: &str, text: &str, calls: &[(&str, &str)]) -> serde_json::Value {
        let mut v = assistant(text, calls);
        v["provider"] = json!(provider);
        v
    }

    #[test]
    fn a_turn_longer_than_the_cap_is_condensed_not_dropped() {
        let mut lines = vec![turn(1), user("dig through the logs")];
        for i in 0..30 {
            let id = format!("t{i}");
            lines.push(assistant("", &[(&id, "read")]));
            lines.push(call(&id, "read"));
            lines.push(result(&id, "small"));
        }
        lines.push(assistant("found it: disk full", &[]));
        lines.push(turn(2));
        let h = derive(&log(lines), 2);
        assert_eq!(roles(&h), vec!["user", "assistant"]);
        assert_eq!(h[0].content, "dig through the logs");
        assert_eq!(h[1].content, "found it: disk full");
    }

    #[test]
    fn same_name_call_ids_keep_their_own_results_and_grades() {
        // Gemini reuses the function name as the call id.
        let a = hundred_lines();
        let b = hundred_lines().replace("line", "row");
        let ev = log(vec![
            turn(1),
            user("q"),
            assistant("", &[("read", "read")]),
            call("read", "read"),
            result("read", &a),
            graded("read", "cA", "hide", &[]),
            assistant("", &[("read", "read")]),
            call("read", "read"),
            result("read", &b),
            graded("read", "cB", "full", &[]),
            assistant("done", &[]),
            turn(2),
        ]);
        let h = derive(&ev, 2);
        assert!(h[1].content.contains("recall(\"cA\")"), "{}", h[1].content);
        let tools: Vec<&Message> = h
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].content.contains("row 0") && !tools[0].content.contains("line 0"));
    }

    #[test]
    fn on_device_turns_send_no_tool_results() {
        let ev = log(vec![
            turn(1),
            user("check my notes"),
            assistant_by("local", "", &[("t1", "read")]),
            call("t1", "read"),
            result("t1", "PRIVATE NOTES"),
            assistant_by("local", "they mention a dentist", &[]),
            turn(2),
        ]);
        let h = derive(&ev, 2);
        assert!(
            h.iter().all(|m| !m.content.contains("PRIVATE NOTES")),
            "{h:?}"
        );
        assert_eq!(roles(&h), vec!["user", "assistant"]);
        assert_eq!(h[1].content, "they mention a dentist");
    }

    fn with_table(table: Vec<TableTurn>) -> HistoryOpts {
        HistoryOpts { table, ..opts() }
    }

    #[test]
    fn the_users_own_words_come_from_the_table() {
        // The logged request carries the turn's injections (open tabs, the
        // voice hint); the table holds what the user typed.
        let ev = log(vec![
            turn(1),
            user("[Open tabs: https://old.example]\n[read aloud]\n\nbook a table"),
            assistant("booked", &[]),
            turn(2),
        ]);
        let table = vec![TableTurn {
            user: "book a table".into(),
            other_speaker: None,
        }];
        let h = derive_history(&ev, 2, &grades(&ev), &with_table(table));
        assert_eq!(h[0].content, "book a table");
    }

    #[test]
    fn another_souls_turn_is_attributed_and_condensed() {
        let ev = log(vec![
            turn(1),
            user("research this"),
            assistant("", &[("t1", "read")]),
            call("t1", "read"),
            result("t1", "notes"),
            assistant("here is what I found", &[]),
            turn(2),
        ]);
        let table = vec![TableTurn {
            user: "research this".into(),
            other_speaker: Some("Researcher".into()),
        }];
        let h = derive_history(&ev, 2, &grades(&ev), &with_table(table));
        assert_eq!(roles(&h), vec!["user", "user"]);
        assert_eq!(h[1].content, "[Researcher] here is what I found");
    }

    #[test]
    fn text_only_folds_kept_pairs_into_the_text() {
        let ev = log(vec![
            turn(1),
            user("read a.txt"),
            assistant("", &[("t1", "read")]),
            call("t1", "read"),
            result("t1", "small"),
            assistant("it says small", &[]),
            turn(2),
        ]);
        let o = HistoryOpts {
            text_only: true,
            ..opts()
        };
        let h = derive_history(&ev, 2, &grades(&ev), &o);
        assert!(h
            .iter()
            .all(|m| m.tool_calls.is_empty() && m.tool_call_id.is_none()));
        assert!(
            h[1].content.contains("[read result]\nsmall"),
            "{}",
            h[1].content
        );
    }

    #[test]
    fn a_subagents_turn_inside_a_tool_call_is_not_a_turn() {
        // A subagent's host shares the session: its turn is logged inside
        // the parent's open tool call.
        let ev = log(vec![
            turn(1),
            user("summarise the repo"),
            assistant("", &[("t1", "subagent")]),
            call("t1", "subagent"),
            turn(1),
            user("SUBAGENT TASK"),
            assistant("sub answer", &[]),
            json!({"type": "turn/end", "turn": 1}),
            result("t1", "summary"),
            assistant("here it is", &[]),
            json!({"type": "turn/end", "turn": 1}),
            turn(2),
            user("thanks"),
        ]);
        let h = derive(&ev, u32::MAX);
        assert!(h.iter().all(|m| !m.content.contains("SUBAGENT")), "{h:?}");
        assert_eq!(
            roles(&h),
            vec!["user", "assistant", "tool", "assistant", "user"]
        );
        assert_eq!(h[2].content, "summary");
        // The current turn starts at the parent's turn/start, not the subagent's.
        assert_eq!(current_turn_start(&ev), Some(11));
    }
}
