//! Jev visibility (spec §5.4 request ②, §5.6): which tool results are
//! graded, the questions asked, how answers combine, and the renditions.
//! Pure functions; the I/O lives in `render.rs`.

use std::collections::BTreeMap;
use std::ops::Range;

use super::wire::{Answer, JevResponse, Question};
use crate::turn_stats::estimate_tokens;

/// Results up to this size are not graded: they go in full (spec §5.4).
pub const SMALL: usize = 4096;
/// Lines per block Noul (spec §5.6.6, S4).
pub const BLOCK_LINES: usize = 25;
/// A block is kept at this Noul or above (S4: recall 85%, keeps ~17%).
pub const KEEP_THRESHOLD: f64 = 0.3;
/// A long grade keeping fewer lines than this is shown short (Q26).
pub const MIN_KEPT_LINES: usize = 3;
/// Estimated tokens per request: 70% of the 64k cap, because the estimate
/// (chars/4) under-counts JSON, code and numbers. Content is counted twice
/// (state and block questions) plus a fixed cost per block.
pub const REQUEST_TOKENS: u64 = 44_800;
/// The query as repeated inside every block question is cut to this many
/// chars; the state carries it whole once.
const QUERY_NOUL_CHARS: usize = 600;
/// Instructions, criteria and JSON keys of one block question, in tokens.
const NOUL_OVERHEAD: u64 = 60;
/// The visibility question and the state wrapper, in tokens.
const REQUEST_OVERHEAD: u64 = 300;
/// Parts asked in parallel; content beyond them is not graded.
pub const MAX_PARTS: usize = 4;
/// Longer lines are split, so one minified line cannot fill a block.
pub const LINE_MAX_CHARS: usize = 2_000;
/// Upper bound on any short rendition, stub included.
pub const SHORT_MAX: usize = 2_600;
/// Line text shown inside a block question (S4 used 300).
const BLOCK_LINE_CHARS: usize = 300;
const SHORT_HEAD_BYTES: usize = 1_500;
const SHORT_TAIL_BYTES: usize = 500;

/// Visibility levels, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Hide,
    Short,
    Long,
    Full,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Hide => "hide",
            Level::Short => "short",
            Level::Long => "long",
            Level::Full => "full",
        }
    }

    fn from_choice(s: &str) -> Option<Self> {
        match s {
            "hide" => Some(Level::Hide),
            "short" => Some(Level::Short),
            "long" => Some(Level::Long),
            "full" => Some(Level::Full),
            _ => None,
        }
    }
}

/// A run of consecutive lines graded in one request.
#[derive(Debug, Clone)]
pub struct Part {
    /// Index of the first line in the whole result (0-based).
    pub first_line: usize,
    pub lines: Vec<String>,
}

/// The result's lines, with lines over [`LINE_MAX_CHARS`] split.
pub fn pseudo_lines(content: &str) -> Vec<String> {
    let body = content.strip_suffix('\n').unwrap_or(content);
    let mut out = Vec::new();
    for line in body.split('\n') {
        if line.chars().count() <= LINE_MAX_CHARS {
            out.push(line.to_string());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for piece in chars.chunks(LINE_MAX_CHARS) {
            out.push(piece.iter().collect());
        }
    }
    out
}

/// Split into at most [`MAX_PARTS`] parts, each of which assembles into a
/// request under [`REQUEST_TOKENS`] (content twice, the query once in the
/// state and once per block question, plus fixed overheads); also returns how
/// many trailing lines were left ungraded.
pub fn parts(lines: &[String], query: &str) -> (Vec<Part>, usize) {
    let query_noul: String = query.chars().take(QUERY_NOUL_CHARS).collect();
    let per_block = estimate_tokens(&query_noul) + NOUL_OVERHEAD;
    let base = estimate_tokens(query) + REQUEST_OVERHEAD;
    let mut out: Vec<Part> = Vec::new();
    let mut current = Part {
        first_line: 0,
        lines: Vec::new(),
    };
    let (mut content, mut blocks) = (0u64, 0u64);
    for (i, line) in lines.iter().enumerate() {
        // One line is at most LINE_MAX_CHARS chars, far under the budget.
        let cost = estimate_tokens(line) + 1;
        let opens_block = current.lines.is_empty() || i % BLOCK_LINES == 0;
        let next_blocks = blocks + u64::from(opens_block);
        let total = base + 2 * (content + cost) + next_blocks * per_block;
        if !current.lines.is_empty() && total > REQUEST_TOKENS {
            out.push(std::mem::replace(
                &mut current,
                Part {
                    first_line: i,
                    lines: Vec::new(),
                },
            ));
            content = 0;
            blocks = 0;
            if out.len() == MAX_PARTS {
                return (out, lines.len() - i);
            }
            blocks += 1; // this line opens a block in the new part
        } else {
            blocks = next_blocks;
        }
        current.lines.push(line.clone());
        content += cost;
    }
    if !current.lines.is_empty() {
        out.push(current);
    }
    (out, 0)
}

/// Block index of a line (blocks are global, so parts never disagree).
fn block_of(line: usize) -> usize {
    line / BLOCK_LINES
}

/// For each block a part touches: its key and the part's lines in it
/// (global indices).
fn part_blocks(part: &Part) -> Vec<(String, Range<usize>)> {
    let end = part.first_line + part.lines.len();
    let mut out = Vec::new();
    let mut start = part.first_line;
    while start < end {
        let b = block_of(start);
        let stop = ((b + 1) * BLOCK_LINES).min(end);
        out.push((format!("b{b:03}"), start..stop));
        start = stop;
    }
    out
}

/// The S4 visibility question (`test3_grade.py`), verbatim.
fn visibility_question() -> Question {
    let mut options = BTreeMap::new();
    options.insert(
        "hide".into(),
        "Not useful for this query; leave it out.".into(),
    );
    options.insert("short".into(), "Only a brief summary is useful.".into());
    options.insert(
        "long".into(),
        "Specific parts are needed; keep the relevant lines.".into(),
    );
    options.insert(
        "full".into(),
        "Most of the content is needed verbatim.".into(),
    );
    Question::Choice {
        instructions: "An AI agent is working on the user's query. The state holds one tool \
            result (or a short summary of it). Decide how visible this tool result should be in \
            the agent's context for answering the query."
            .into(),
        options,
    }
}

/// One request's state and questions for `part`: the visibility Choice plus
/// one block Noul per block (spec §5.4: merged into request ②).
pub fn questions(query: &str, part: &Part) -> (serde_json::Value, BTreeMap<String, Question>) {
    let state = serde_json::json!({
        "query": query,
        "tool_result": part.lines.join("\n"),
    });
    let query_noul: String = query.chars().take(QUERY_NOUL_CHARS).collect();
    let mut qs = BTreeMap::new();
    qs.insert("visibility".to_string(), visibility_question());
    for (key, range) in part_blocks(part) {
        let block: Vec<String> = range
            .map(|i| {
                part.lines[i - part.first_line]
                    .chars()
                    .take(BLOCK_LINE_CHARS)
                    .collect()
            })
            .collect();
        qs.insert(
            key,
            Question::Noul {
                instructions: format!(
                    "Query: \"{query_noul}\"\nDo these lines contain information that helps \
                     answer the query?\n---\n{}",
                    block.join("\n")
                ),
                when_true: "These lines help answer the query.".into(),
                when_false: "These lines do not help answer the query.".into(),
            },
        );
    }
    (state, qs)
}

/// A combined grade: the level and, for long, the kept line ranges.
#[derive(Debug, Clone, PartialEq)]
pub struct Grade {
    pub level: Level,
    pub kept: Vec<Range<usize>>,
}

/// Combine the answers of every part (`answers[i]` belongs to `parts[i]`).
/// The highest level wins; a missing or malformed choice counts as full,
/// so a bad answer never loses content.
pub fn combine(parts: &[Part], answers: &[JevResponse]) -> Grade {
    let mut level = Level::Hide;
    let mut kept: Vec<Range<usize>> = Vec::new();
    for (part, resp) in parts.iter().zip(answers) {
        let l = match resp.answer_for("visibility") {
            Some(Answer::Choice { choice, .. }) => {
                Level::from_choice(choice).unwrap_or(Level::Full)
            }
            _ => Level::Full,
        };
        level = level.max(l);
        for (key, range) in part_blocks(part) {
            if resp.noul(&key).is_some_and(|p| p >= KEEP_THRESHOLD) {
                match kept.last_mut() {
                    Some(last) if last.end == range.start => last.end = range.end,
                    _ => kept.push(range),
                }
            }
        }
    }
    if parts.is_empty() {
        level = Level::Full;
    }
    let kept_lines: usize = kept.iter().map(|r| r.len()).sum();
    if level == Level::Long && kept_lines < MIN_KEPT_LINES {
        level = Level::Short;
    }
    Grade { level, kept }
}

/// What the stub line names.
pub struct Meta<'a> {
    pub id: &'a str,
    pub tool: &'a str,
    pub bytes: usize,
    /// `jev`, `fallback` or `sensitive`; only non-`jev` is shown.
    pub graded_by: &'a str,
}

/// The longest prefix of `s` within `n` bytes, on a char boundary.
pub(crate) fn cut_bytes(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn stub(level: Level, lines: usize, meta: &Meta) -> String {
    let by = if meta.graded_by == "jev" {
        String::new()
    } else {
        format!(" · graded by {}", meta.graded_by)
    };
    format!(
        "[{} · {} · {} bytes, {} lines · {}{}] — recall(\"{}\") returns the full text",
        meta.id,
        meta.tool,
        meta.bytes,
        lines,
        level.as_str(),
        by,
        meta.id
    )
}

/// Lines in iteration order until `budget` bytes, cutting the last one.
fn take_bytes<'a>(lines: impl Iterator<Item = &'a String>, budget: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut used = 0;
    for l in lines {
        if used >= budget {
            break;
        }
        let piece = cut_bytes(l, budget - used);
        used += piece.len() + 1;
        out.push(piece.to_string());
    }
    out
}

/// Gap marker, 1-based inclusive line numbers.
fn omitted(a: usize, b: usize) -> String {
    format!("… lines {}–{} omitted …", a + 1, b)
}

/// The text the model sees at `level`. `content` is the original result
/// (offsets in `recall` count its bytes); `lines` are its pseudo-lines.
pub fn render(
    level: Level,
    content: &str,
    lines: &[String],
    kept: &[Range<usize>],
    meta: &Meta,
    max_bytes: usize,
) -> String {
    let n = lines.len();
    match level {
        Level::Hide => stub(level, n, meta),
        Level::Short => {
            let mut out = stub(level, n, meta);
            let head = take_bytes(lines.iter(), SHORT_HEAD_BYTES);
            let shown_head = head.len();
            out.push('\n');
            out.push_str(&head.join("\n"));
            if shown_head < n {
                let mut tail = take_bytes(lines[shown_head..].iter().rev(), SHORT_TAIL_BYTES);
                tail.reverse();
                let skipped = n - shown_head - tail.len();
                if skipped > 0 {
                    out.push('\n');
                    out.push_str(&omitted(shown_head, shown_head + skipped));
                }
                if !tail.is_empty() {
                    out.push('\n');
                    out.push_str(&tail.join("\n"));
                }
            }
            cut_bytes(&out, SHORT_MAX).to_string()
        }
        Level::Long => {
            let mut out = stub(level, n, meta);
            let mut at = 0;
            for r in kept {
                if r.start > at {
                    out.push('\n');
                    out.push_str(&omitted(at, r.start));
                }
                for l in &lines[r.start.min(n)..r.end.min(n)] {
                    out.push('\n');
                    out.push_str(l);
                }
                at = r.end;
            }
            if at < n {
                out.push('\n');
                out.push_str(&omitted(at, n));
            }
            cut_bytes(&out, max_bytes).to_string()
        }
        Level::Full => {
            if content.len() <= max_bytes {
                return content.to_string();
            }
            let head = cut_bytes(content, max_bytes.saturating_sub(200));
            format!(
                "{head}\n… truncated at {} bytes — recall(\"{}\", offset={}) returns the rest",
                head.len(),
                meta.id,
                head.len()
            )
        }
    }
}

/// Which page a tool result shows, for the privacy scope (spec §5.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageKind {
    /// A browser tab: the host looks up the tab's actual URL.
    Browser,
    /// Content fetched from this URL.
    Url(String),
    /// Page or third-party content from an unknown source: metadata only.
    Unknown,
    /// Not page content (local files, shell, the agent's own reasoning).
    None,
}

/// Tools whose results are local, never page or third-party content. Every
/// other tool is `Unknown` until proven otherwise (MCP results, subagents,
/// flows, memory and knowledge tools all stay out of Jev).
const NO_PAGE_TOOLS: &[&str] = &[
    "think",
    "plan",
    "web_search",
    "read",
    "write",
    "edit",
    "bash",
    "glob",
    "grep",
    "tool_search",
    "skill_load",
];

/// The page kind of a call. `tool_call_dynamic` is judged by the tool it
/// wraps: that is how MCP results reach a cloud turn.
pub fn page_kind(tool: &str, args: &serde_json::Value) -> PageKind {
    if tool == "tool_call_dynamic" {
        let inner = args.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");
        if inner.starts_with("browser_") || inner == "web_fetch" {
            let inner_args = args.get("arguments").unwrap_or(&serde_json::Value::Null);
            return page_kind(inner, inner_args);
        }
        return PageKind::Unknown;
    }
    if tool.starts_with("browser_") {
        return PageKind::Browser;
    }
    if tool == "web_fetch" {
        return args
            .get("url")
            .and_then(|u| u.as_str())
            .filter(|u| !u.trim().is_empty())
            .map_or(PageKind::Unknown, |u| PageKind::Url(u.to_string()));
    }
    if NO_PAGE_TOOLS.contains(&tool) {
        return PageKind::None;
    }
    PageKind::Unknown
}

/// URLs of the tabs a browser result may come from, from a `list_tabs`
/// result: the tab named by `tab_id`, else every active tab. `None` when that
/// cannot be told (no list, tab not found, a candidate without a URL).
pub fn candidate_urls(tabs: &serde_json::Value, args: &serde_json::Value) -> Option<Vec<String>> {
    let list = tabs.get("tabs").and_then(|t| t.as_array())?;
    let url_of = |t: &serde_json::Value| {
        t.get("url")
            .and_then(|u| u.as_str())
            .filter(|u| !u.trim().is_empty())
            .map(str::to_string)
    };
    if let Some(id) = args.get("tab_id").and_then(|i| i.as_i64()) {
        let tab = list
            .iter()
            .find(|t| t.get("id").and_then(|i| i.as_i64()) == Some(id))?;
        return url_of(tab).map(|u| vec![u]);
    }
    let active: Vec<&serde_json::Value> = list
        .iter()
        .filter(|t| t.get("active").and_then(|a| a.as_bool()) == Some(true))
        .collect();
    if active.is_empty() {
        return None;
    }
    active.into_iter().map(url_of).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::wire::{Answer, JevResponse, JevUsage};

    fn resp(choice: &str, nouls: &[(&str, f64)]) -> JevResponse {
        let mut answers = BTreeMap::new();
        answers.insert(
            "visibility".into(),
            Answer::Choice {
                choice: choice.into(),
                probabilities: BTreeMap::new(),
            },
        );
        for (k, p) in nouls {
            answers.insert((*k).into(), Answer::Noul(*p));
        }
        JevResponse {
            answers,
            usage: JevUsage::default(),
        }
    }

    fn numbered(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn a_single_huge_line_is_split_into_pseudo_lines() {
        let one = "x".repeat(200_000);
        let lines = pseudo_lines(&one);
        assert_eq!(lines.len(), 100);
        assert!(lines.iter().all(|l| l.chars().count() <= LINE_MAX_CHARS));
    }

    #[test]
    fn cjk_parts_respect_the_token_budget() {
        let lines: Vec<String> = (0..2000).map(|_| "中文内容测试".repeat(10)).collect();
        let (ps, rest) = parts(&lines, "q");
        assert!(ps.len() <= MAX_PARTS);
        for p in &ps {
            let (state, qs) = questions("q", p);
            let body = serde_json::to_string(&serde_json::json!({"state": state, "questions": qs}))
                .unwrap();
            assert!(
                crate::turn_stats::estimate_tokens(&body) <= REQUEST_TOKENS,
                "part over budget"
            );
        }
        assert_eq!(ps.iter().map(|p| p.lines.len()).sum::<usize>() + rest, 2000);
    }

    #[test]
    fn small_inputs_are_one_part_with_nothing_left_over() {
        let lines = numbered(60);
        let (ps, rest) = parts(&lines, "q");
        assert_eq!((ps.len(), rest), (1, 0));
        assert_eq!(ps[0].first_line, 0);
    }

    #[test]
    fn questions_carry_the_visibility_choice_and_one_noul_per_block() {
        let lines = numbered(60);
        let (ps, _) = parts(&lines, "q");
        let (state, qs) = questions("find line 30", &ps[0]);
        assert_eq!(state["query"], "find line 30");
        assert!(state["tool_result"].as_str().unwrap().contains("line 59"));
        assert!(
            matches!(qs.get("visibility"), Some(Question::Choice { options, .. }) if options.len() == 4)
        );
        assert_eq!(qs.keys().filter(|k| k.starts_with('b')).count(), 3); // 25 + 25 + 10
    }

    #[test]
    fn combine_takes_the_highest_level_and_the_blocks_over_threshold() {
        let lines = numbered(60);
        let (ps, _) = parts(&lines, "q");
        let g = combine(
            &ps,
            &[resp(
                "long",
                &[("b000", 0.1), ("b001", 0.31), ("b002", 0.0)],
            )],
        );
        assert_eq!(g.level, Level::Long);
        assert_eq!(g.kept, vec![25..50]);
    }

    #[test]
    fn long_with_fewer_than_three_kept_lines_is_short() {
        let lines = numbered(52); // last block has 2 lines
        let (ps, _) = parts(&lines, "q");
        let g = combine(
            &ps,
            &[resp("long", &[("b000", 0.0), ("b001", 0.0), ("b002", 0.9)])],
        );
        assert_eq!(g.level, Level::Short);
    }

    #[test]
    fn a_malformed_choice_keeps_the_content() {
        let lines = numbered(30);
        let (ps, _) = parts(&lines, "q");
        let mut r = resp("long", &[]);
        r.answers.insert(
            "visibility".into(),
            Answer::Other(serde_json::json!({"x": 1})),
        );
        assert_eq!(combine(&ps, &[r]).level, Level::Full);
    }

    #[test]
    fn short_is_bounded_for_any_input() {
        for content in [
            "a".repeat(500_000),
            "中".repeat(300_000),
            (0..20_000).map(|i| format!("{i}\n")).collect(),
        ] {
            let lines = pseudo_lines(&content);
            let meta = Meta {
                id: "call_1",
                tool: "browser_get_markdown",
                bytes: content.len(),
                graded_by: "fallback",
            };
            let s = render(Level::Short, &content, &lines, &[], &meta, 32_000);
            assert!(s.len() <= SHORT_MAX, "{}", s.len());
            assert!(s.contains("recall(\"call_1\")"));
            assert!(s.contains("graded by fallback"));
        }
    }

    #[test]
    fn hide_is_the_stub_and_long_marks_the_gaps() {
        let lines = numbered(100);
        let meta = Meta {
            id: "c1",
            tool: "grep",
            bytes: 900,
            graded_by: "jev",
        };
        let content = lines.join("\n");
        let hide = render(Level::Hide, &content, &lines, &[], &meta, 32_000);
        assert_eq!(hide.lines().count(), 1);
        assert!(hide.contains("recall(\"c1\")"));
        assert!(!hide.contains("graded by"), "jev grades are not labelled");
        let long = render(Level::Long, &content, &lines, &[25..50], &meta, 32_000);
        assert!(long.contains("line 25") && long.contains("line 49"));
        assert!(!long.contains("line 50\n") && !long.contains("line 24\n"));
        assert!(long.contains("lines 1–25 omitted"));
        assert!(long.contains("lines 51–100 omitted"));
    }

    #[test]
    fn full_over_the_cap_points_at_recall_with_an_offset() {
        let content = "y".repeat(50_000);
        let lines = pseudo_lines(&content);
        let meta = Meta {
            id: "c2",
            tool: "read",
            bytes: content.len(),
            graded_by: "jev",
        };
        let f = render(Level::Full, &content, &lines, &[], &meta, 32_000);
        assert!(f.len() <= 32_200, "{}", f.len());
        assert!(f.contains("recall(\"c2\", offset="));
    }

    #[test]
    fn page_kind_by_tool() {
        let none = serde_json::json!({});
        assert_eq!(page_kind("browser_get_markdown", &none), PageKind::Browser);
        assert_eq!(
            page_kind(
                "web_fetch",
                &serde_json::json!({"url": "https://c.example/"})
            ),
            PageKind::Url("https://c.example/".into())
        );
        assert_eq!(page_kind("web_fetch", &none), PageKind::Unknown);
        assert_eq!(page_kind("read", &none), PageKind::None);
        assert_eq!(page_kind("bash", &none), PageKind::None);
    }

    #[test]
    fn anything_not_known_to_be_local_is_unknown() {
        // Review C1: MCP results arrive as tool_call_dynamic, never as `x__y`.
        let none = serde_json::json!({});
        for tool in [
            "tool_call_dynamic",
            "gmail__search",
            "memory_search",
            "run_flow",
            "orchestrate",
            "subagent_wait",
        ] {
            assert_eq!(page_kind(tool, &none), PageKind::Unknown, "{tool}");
        }
        let gmail = serde_json::json!({"tool_name": "search_mail", "arguments": {"q": "x"}});
        assert_eq!(page_kind("tool_call_dynamic", &gmail), PageKind::Unknown);
        let browser = serde_json::json!({"tool_name": "browser_get_markdown", "arguments": {}});
        assert_eq!(page_kind("tool_call_dynamic", &browser), PageKind::Browser);
        let fetch = serde_json::json!({"tool_name": "web_fetch", "arguments": {"url": "https://d.example/"}});
        assert_eq!(
            page_kind("tool_call_dynamic", &fetch),
            PageKind::Url("https://d.example/".into())
        );
    }

    #[test]
    fn the_target_tab_is_the_named_one_or_every_active_one() {
        let tabs = serde_json::json!({"tabs": [
            {"id": 1, "url": "https://news.example/", "active": true},
            {"id": 2, "url": "https://www.paypal.com/x", "active": false},
            {"id": 3, "url": "https://other.example/", "active": true}
        ]});
        assert_eq!(
            candidate_urls(&tabs, &serde_json::json!({"tab_id": 2})),
            Some(vec!["https://www.paypal.com/x".to_string()])
        );
        assert_eq!(
            candidate_urls(&tabs, &serde_json::json!({})),
            Some(vec![
                "https://news.example/".to_string(),
                "https://other.example/".to_string()
            ])
        );
        assert_eq!(
            candidate_urls(&tabs, &serde_json::json!({"tab_id": 9})),
            None
        );
        assert_eq!(
            candidate_urls(&serde_json::json!({"tabs": []}), &serde_json::json!({})),
            None
        );
        assert_eq!(
            candidate_urls(&serde_json::json!(null), &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn an_assembled_request_stays_under_the_cap_with_a_long_query_and_short_lines() {
        // Review I2: per-block overhead and the repeated query count too.
        let query = "please summarise ".repeat(150); // ~2.5k chars
        let lines: Vec<String> = (0..40_000).map(|i| format!("{i:06}")).collect();
        let (ps, _) = parts(&lines, &query);
        assert!(!ps.is_empty());
        for p in &ps {
            let (state, qs) = questions(&query, p);
            let body = serde_json::to_string(&serde_json::json!({"state": state, "questions": qs}))
                .unwrap();
            assert!(
                crate::turn_stats::estimate_tokens(&body) <= REQUEST_TOKENS,
                "request of {} estimated tokens",
                crate::turn_stats::estimate_tokens(&body)
            );
        }
    }
}
