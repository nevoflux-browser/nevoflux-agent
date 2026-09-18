//! On-device tool loading.
//!
//! A cloud turn advertises every tool its mode owns — sixty-odd schemas, tens
//! of thousands of tokens. A local context of 16-32K cannot hold that and the
//! conversation, so on-device mode advertises `tool_search` alone (plus a small
//! resident set in chat mode) and loads the rest as the model asks for them.
//! Loaded tools go into the real `tools` array and are called natively; nothing
//! here re-implements tool dispatch.
//!
//! The search, the weights and the wire strings below are a faithful port of
//! the V0 experiment's harness (`v0-experiment/run_set_a.py`), because that
//! harness is what produced the selection numbers the ship decision rests on:
//! browser and agent measured 1.00/1.00 with exactly this search. Changing a
//! weight or a result line means the shipped behaviour is no longer the
//! behaviour that was measured.

use crate::types::{AgentMode, SkillSummary, ToolDefinition, ToolSearchResult};
use std::collections::HashSet;

/// How many tools may stay loaded at once.
///
/// Each loaded tool costs its schema on every subsequent request, so an
/// unbounded set would slowly recreate the very problem deferred loading
/// exists to solve. At the cap the oldest is evicted: the model can always
/// search for it again, and paying one extra round-trip beats silently
/// crowding the context.
pub const LOADED_TOOLS_CAP: usize = 12;

/// Tools that never enter the local index.
///
/// `tool_search` is the entry point itself and `tool_call_dynamic` is its cloud
/// counterpart — locally, a discovered tool becomes a real entry in the `tools`
/// array, so there is nothing to route through a dynamic caller. The other
/// three are cloud-only orchestration that a local model cannot use well and
/// that would only spend context.
pub const LOCAL_EXCLUDED_TOOLS: &[&str] = &[
    "switch_model",
    "tool_call_dynamic",
    "tool_search",
    "orchestrate",
    "load_computer_use_tools",
];

/// Per-mode resident set (v3 §23.1.1, user decision 2026-09-15).
///
/// Chat keeps these loaded from the first request; browser and agent start with
/// `tool_search` alone. This asymmetry is measured, not stylistic: residents
/// lift chat from 0.55 to 0.68-0.73, but a resident `browser_get_markdown` in
/// browser mode captures click/fill/navigate intents — the model reaches for
/// the page reader it can already see instead of searching for the action it
/// needs — and browser collapses from 0.90 to 0.20. The set must stay
/// chat-only.
pub fn resident_tools_for(mode: AgentMode) -> &'static [&'static str] {
    match mode {
        AgentMode::Chat => &["web_search", "memory_search", "browser_get_markdown"],
        #[allow(deprecated)]
        AgentMode::Browser | AgentMode::Agent | AgentMode::Code => &[],
    }
}

/// The one tool a local turn always carries.
///
/// Description and schema are copied from the V0 harness's `TOOL_SEARCH`; the
/// model's ability to drive this loop was measured against this exact wording.
pub fn local_tool_search_def() -> ToolDefinition {
    ToolDefinition {
        name: "tool_search".into(),
        description: "Find and load tools. `select:name1,name2` loads exact names (including \
             `skill/<name>` for skills); otherwise the query is matched against tool and \
             skill names and descriptions. Loaded tools become callable by name."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "`select:<names>` or keywords"},
                "max_results": {"type": "integer", "default": 5},
            },
            "required": ["query"],
        }),
    }
}

/// What a `tool_search` query turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchQuery<'a> {
    /// Exact names to load, already split and trimmed.
    Select(Vec<&'a str>),
    /// Free text to match against names and descriptions.
    Keywords(&'a str),
}

/// Split a query on its `select:` prefix.
///
/// Prefix matching is case-insensitive and the names are trimmed, because the
/// model writes `Select: a, b` about as often as `select:a,b`.
///
/// This deliberately does NOT recognise a bare comma-separated list of tool
/// names as a selection — deciding that requires knowing which names exist.
/// [`LocalToolIndex::resolve_query`] does it.
pub fn parse_query(q: &str) -> SearchQuery<'_> {
    let trimmed = q.trim();
    // `get` rather than a byte slice: a query may open with multi-byte
    // characters, and `&trimmed[..7]` would panic partway through one.
    let is_select = trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("select:"));
    if is_select {
        return SearchQuery::Select(split_names(&trimmed[7..]));
    }
    SearchQuery::Keywords(trimmed)
}

fn split_names(s: &str) -> Vec<&str> {
    s.split(',')
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .collect()
}

/// Tokenise for scoring: runs of ASCII alphanumerics, plus each CJK character
/// as a token of its own.
///
/// The per-character CJK rule is not an approximation of word segmentation —
/// it is what the measured harness did, and the case set is bilingual, so a
/// different rule would score Chinese queries differently from the run the
/// numbers came from.
fn tokens(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut run = String::new();
    for ch in s.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            run.push(ch);
            continue;
        }
        if !run.is_empty() {
            out.insert(std::mem::take(&mut run));
        }
        if ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            out.insert(ch.to_string());
        }
    }
    if !run.is_empty() {
        out.insert(run);
    }
    out
}

/// A name match is worth three description matches: someone asking for
/// "screenshot" wants the screenshot tool, not every tool that mentions one.
fn score(query: &HashSet<String>, name_spaced: &str, description: &str) -> usize {
    let name = tokens(name_spaced);
    let desc = tokens(description);
    query.intersection(&name).count() * 3 + query.intersection(&desc).count()
}

/// Truncate on a character boundary.
///
/// Slicing bytes would panic partway through a multi-byte character, and these
/// descriptions are bilingual.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// One search result, before it is turned into a loaded tool.
#[derive(Debug, Clone)]
pub enum SearchHit {
    /// A built-in tool from this mode's set.
    Tool(ToolDefinition),
    /// A skill, addressed as `skill/<name>`; loading it returns its text.
    Skill(SkillSummary),
    /// An MCP or knowledge-base tool the host found.
    Dynamic(ToolSearchResult),
}

impl SearchHit {
    /// The name the model will use to call it — skills keep their `skill/`
    /// prefix, since that is how they are addressed.
    pub fn name(&self) -> String {
        match self {
            SearchHit::Tool(t) => t.name.clone(),
            SearchHit::Skill(s) => format!("skill/{}", s.name),
            SearchHit::Dynamic(d) => d.name.clone(),
        }
    }
}

/// A dynamic result carries the same three fields a tool definition needs.
pub fn tool_from_dynamic(result: &ToolSearchResult) -> ToolDefinition {
    ToolDefinition {
        name: result.name.clone(),
        description: result.description.clone(),
        input_schema: result.input_schema.clone(),
    }
}

/// What a loaded tool reports back to the model.
pub fn result_line_for_tool(tool: &ToolDefinition) -> String {
    format!(
        "{}: loaded — {}",
        tool.name,
        truncate_chars(&tool.description, 160)
    )
}

/// A skill is not added to the tool array; its instructions are returned.
pub fn result_line_for_skill(name: &str) -> String {
    format!("skill/{name}: skill instructions loaded")
}

/// Said when a search matched nothing, so the model gets a definite answer
/// instead of an empty string it may read as a malfunction.
pub const NO_MATCHES: &str = "no matching tools";

/// The searchable set for one turn: this mode's built-in tools and the skills
/// the session can see.
pub struct LocalToolIndex {
    builtin: Vec<ToolDefinition>,
    skills: Vec<SkillSummary>,
}

impl LocalToolIndex {
    /// Build from the mode's tools, dropping the ones local mode never offers.
    pub fn new(mode_tools: Vec<ToolDefinition>, skills: Vec<SkillSummary>) -> Self {
        Self {
            builtin: mode_tools
                .into_iter()
                .filter(|t| !LOCAL_EXCLUDED_TOOLS.contains(&t.name.as_str()))
                .collect(),
            skills,
        }
    }

    /// Whether a name is one this index can load, in the same spelling the
    /// model would use (`skill/<name>` for skills).
    pub fn knows(&self, name: &str) -> bool {
        match name.strip_prefix("skill/") {
            Some(bare) => self.skills.iter().any(|s| s.name == bare),
            None => self.builtin.iter().any(|t| t.name == name),
        }
    }

    /// Decide what a query means, including the case `parse_query` cannot judge.
    ///
    /// The V0 probe found the model frequently passes exact tool names as a
    /// plain query — `browser_get_markdown`, or `browser_get_markdown,brain_create`
    /// — rather than using the `select:` prefix it was told about. Treating
    /// those as keywords works by accident (the name scores against itself) but
    /// is not reliable, so a query whose every comma-separated piece is a known
    /// name is taken as a selection. A keyword query that happens to equal a
    /// tool name loads that tool, which is the intended outcome anyway.
    pub fn resolve_query<'a>(&self, q: &'a str) -> SearchQuery<'a> {
        match parse_query(q) {
            SearchQuery::Select(names) => SearchQuery::Select(names),
            SearchQuery::Keywords(k) => {
                let parts = split_names(k);
                if !parts.is_empty() && parts.iter().all(|p| self.knows(p)) {
                    SearchQuery::Select(parts)
                } else {
                    SearchQuery::Keywords(k)
                }
            }
        }
    }

    /// Resolve exact names into tools, skills, and the ones that matched
    /// nothing — the unknowns are reported rather than dropped, so a model that
    /// guessed a name is told so instead of being met with silence.
    pub fn select(&self, names: &[&str]) -> (Vec<ToolDefinition>, Vec<String>, Vec<String>) {
        let mut tools = Vec::new();
        let mut skills = Vec::new();
        let mut unknown = Vec::new();
        for raw in names {
            let name = raw.trim();
            if name.is_empty() {
                continue;
            }
            match name.strip_prefix("skill/") {
                Some(bare) => match self.skills.iter().find(|s| s.name == bare) {
                    Some(skill) => skills.push(skill.name.clone()),
                    None => unknown.push(name.to_string()),
                },
                None => match self.builtin.iter().find(|t| t.name == name) {
                    Some(tool) => tools.push(tool.clone()),
                    None => unknown.push(name.to_string()),
                },
            }
        }
        (tools, skills, unknown)
    }

    /// Token-overlap search over names and descriptions.
    pub fn keyword_search(&self, q: &str, max: usize) -> Vec<SearchHit> {
        let query = tokens(&q.replace('_', " "));
        let mut scored: Vec<(usize, String, SearchHit)> = Vec::new();

        for tool in &self.builtin {
            let s = score(&query, &tool.name.replace('_', " "), &tool.description);
            if s > 0 {
                scored.push((s, tool.name.clone(), SearchHit::Tool(tool.clone())));
            }
        }
        // Skill names are hyphenated where tool names use underscores.
        for skill in &self.skills {
            let s = score(&query, &skill.name.replace('-', " "), &skill.description);
            if s > 0 {
                scored.push((
                    s,
                    format!("skill/{}", skill.name),
                    SearchHit::Skill(skill.clone()),
                ));
            }
        }

        // Highest score first, ties broken by name descending — the harness
        // sorted `(score, name)` tuples in reverse, and keeping that exact
        // order keeps ties resolving the way the measured run resolved them.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        scored
            .into_iter()
            .take(max)
            .map(|(_, _, hit)| hit)
            .collect()
    }

    /// The definitions for a set of already-loaded names, in the set's order.
    pub fn tools_named(&self, names: &[String]) -> Vec<ToolDefinition> {
        names
            .iter()
            .filter_map(|n| self.builtin.iter().find(|t| &t.name == n).cloned())
            .collect()
    }
}

/// The tools loaded so far this session, oldest first.
///
/// Insertion-ordered rather than a set because eviction needs an age, and the
/// order is what the daemon persists between turns.
#[derive(Debug, Clone, Default)]
pub struct LoadedSet {
    names: Vec<String>,
}

impl LoadedSet {
    /// Rebuild from what a previous turn left behind.
    pub fn from_names(names: &[String]) -> Self {
        let mut set = Self::default();
        for name in names {
            set.add(name);
        }
        set
    }

    /// Add a name. Returns whether it was new; re-loading something already
    /// loaded is a no-op rather than a reshuffle, so a tool the model keeps
    /// using does not keep pushing others out.
    pub fn add(&mut self, name: &str) -> bool {
        if self.names.iter().any(|n| n == name) {
            return false;
        }
        self.names.push(name.to_string());
        if self.names.len() > LOADED_TOOLS_CAP {
            self.names.remove(0);
        }
        true
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn skill(name: &str, description: &str) -> SkillSummary {
        SkillSummary {
            name: name.into(),
            description: description.into(),
            tags: Vec::new(),
        }
    }

    fn index() -> LocalToolIndex {
        LocalToolIndex::new(
            vec![
                tool("web_search", "Search the web for pages"),
                tool("browser_get_markdown", "Read the current page as markdown"),
                tool("browser_screenshot", "Take a screenshot of the page"),
                tool("tool_search", "should be excluded"),
                tool("orchestrate", "should be excluded"),
            ],
            vec![skill("app-builder", "Build a small application")],
        )
    }

    #[test]
    fn the_index_drops_the_tools_local_mode_never_offers() {
        let idx = index();
        assert!(!idx.knows("tool_search"));
        assert!(!idx.knows("orchestrate"));
        assert!(idx.knows("web_search"));
        assert!(idx.knows("skill/app-builder"));
        // A skill addressed without its prefix is not a tool name.
        assert!(!idx.knows("app-builder"));
    }

    #[test]
    fn a_select_prefix_is_case_insensitive_and_trims() {
        assert_eq!(
            parse_query("  Select: web_search , browser_screenshot "),
            SearchQuery::Select(vec!["web_search", "browser_screenshot"])
        );
        assert_eq!(
            parse_query("find me a browser"),
            SearchQuery::Keywords("find me a browser")
        );
        // Short inputs must not panic on the 7-byte prefix probe.
        assert_eq!(parse_query("sel"), SearchQuery::Keywords("sel"));
        assert_eq!(parse_query(""), SearchQuery::Keywords(""));
        // A byte slice would panic here: the seventh byte falls inside a
        // character. The case set is bilingual, so this is ordinary input.
        assert_eq!(
            parse_query("知识库查询"),
            SearchQuery::Keywords("知识库查询")
        );
    }

    /// R22: the model passes bare names far more often than `select:`.
    #[test]
    fn a_query_of_only_known_names_is_a_selection() {
        let idx = index();
        assert_eq!(
            idx.resolve_query("browser_get_markdown"),
            SearchQuery::Select(vec!["browser_get_markdown"])
        );
        assert_eq!(
            idx.resolve_query("web_search, skill/app-builder"),
            SearchQuery::Select(vec!["web_search", "skill/app-builder"])
        );
        // One unknown piece and it is keywords again.
        assert_eq!(
            idx.resolve_query("web_search, nonsense"),
            SearchQuery::Keywords("web_search, nonsense")
        );
        assert_eq!(idx.resolve_query("brain"), SearchQuery::Keywords("brain"));
    }

    #[test]
    fn select_reports_what_it_could_not_find() {
        let idx = index();
        let (tools, skills, unknown) = idx.select(&[
            "web_search",
            "skill/app-builder",
            "switch_model",
            "skill/nope",
        ]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "web_search");
        assert_eq!(skills, vec!["app-builder".to_string()]);
        assert_eq!(
            unknown,
            vec!["switch_model".to_string(), "skill/nope".to_string()]
        );
    }

    #[test]
    fn a_name_match_outweighs_a_description_match() {
        let idx = index();
        let hits = idx.keyword_search("screenshot", 5);
        assert_eq!(hits[0].name(), "browser_screenshot");
    }

    #[test]
    fn search_finds_skills_alongside_tools() {
        let idx = index();
        let names: Vec<String> = idx
            .keyword_search("application", 5)
            .iter()
            .map(|h| h.name())
            .collect();
        assert!(names.contains(&"skill/app-builder".to_string()));
    }

    #[test]
    fn search_matches_chinese_per_character() {
        let idx = LocalToolIndex::new(vec![tool("brain_search", "搜索知识库")], Vec::new());
        let hits = idx.keyword_search("知识库", 5);
        assert_eq!(hits.len(), 1, "每个汉字都要能单独参与匹配");
    }

    #[test]
    fn search_returns_nothing_rather_than_everything_when_nothing_matches() {
        let idx = index();
        assert!(idx.keyword_search("zzzz", 5).is_empty());
    }

    #[test]
    fn the_loaded_set_evicts_the_oldest_at_the_cap() {
        let mut set = LoadedSet::default();
        for i in 0..LOADED_TOOLS_CAP {
            assert!(set.add(&format!("tool_{i}")));
        }
        assert_eq!(set.names().len(), LOADED_TOOLS_CAP);
        set.add("one_too_many");
        assert_eq!(set.names().len(), LOADED_TOOLS_CAP);
        assert_eq!(set.names()[0], "tool_1", "最旧的那个应当被挤出去");
        assert_eq!(set.names().last().unwrap(), "one_too_many");
    }

    #[test]
    fn reloading_a_tool_does_not_reshuffle_the_set() {
        let mut set = LoadedSet::from_names(&["a".into(), "b".into()]);
        assert!(!set.add("a"));
        assert_eq!(set.names(), &["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn residents_are_chat_only() {
        assert_eq!(resident_tools_for(AgentMode::Chat).len(), 3);
        assert!(resident_tools_for(AgentMode::Browser).is_empty());
        assert!(resident_tools_for(AgentMode::Agent).is_empty());
    }

    #[test]
    fn a_long_description_is_truncated_on_a_character_boundary() {
        let long = "页".repeat(200);
        let line = result_line_for_tool(&tool("x", &long));
        assert_eq!(line.chars().filter(|c| *c == '页').count(), 160);
    }
}
