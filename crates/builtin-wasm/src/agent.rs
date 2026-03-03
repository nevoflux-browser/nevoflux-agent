//! Agent loop implementation.
//!
//! This module contains the core agent logic that:
//! - Constructs prompts based on mode
//! - Calls the LLM
//! - Executes tool calls
//! - Manages the conversation loop

use crate::host::{HostFunctions, HostResult};
use crate::types::*;
use nevoflux_protocol::{Artifact, LocalFileRef, PlanProposal, PlanStep};
use std::cell::{Cell, RefCell};

/// Format local file references for injection into user message.
fn format_local_files(files: &[LocalFileRef]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let mut result = String::from("用户附加了以下本地文件/目录：\n");

    for file in files {
        let type_str = if file.is_directory {
            "目录"
        } else {
            "文件"
        };
        let size_str = file.size.map(format_file_size).unwrap_or_default();

        if file.is_directory {
            result.push_str(&format!("- {} ({})\n", file.path, type_str));
        } else {
            result.push_str(&format!("- {} ({}, {})\n", file.path, type_str, size_str));
        }
    }

    result.push_str("\n如需查看内容，请使用 read_file 或 list_directory 工具。\n\n");
    result
}

/// Format file size in human-readable form.
fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format a `ReadResult` into model-readable text with line numbers and metadata.
fn format_read_result(result: &ReadResult, path: &str) -> String {
    let mut output = String::new();
    let end_line = result.offset + result.returned_lines;
    let size_str = if result.total_bytes >= 1024 * 1024 {
        format!("{:.1}MB", result.total_bytes as f64 / (1024.0 * 1024.0))
    } else if result.total_bytes >= 1024 {
        format!("{:.0}KB", result.total_bytes as f64 / 1024.0)
    } else {
        format!("{}B", result.total_bytes)
    };

    output.push_str(&format!(
        "[File: {} | Lines: {}-{} of {} | {}]\n",
        path,
        result.offset + 1,
        end_line,
        result.total_lines,
        size_str
    ));

    for (i, line) in result.content.lines().enumerate() {
        let line_num = result.offset + i as u64 + 1;
        output.push_str(&format!("{:>4}|{}\n", line_num, line));
    }

    if result.truncated {
        let remaining = result.total_lines - end_line;
        output.push_str(&format!(
            "[Truncated: {} lines remaining. Use offset={} to continue.]",
            remaining, end_line
        ));
    }

    output
}

/// Format a `GrepResult` into model-readable text with match locations.
fn format_grep_result(result: &GrepResult, pattern: &str) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "[Search: \"{}\" | {} matches in {} files | showing {}]\n",
        pattern, result.total_matches, result.total_files, result.returned
    ));

    for m in &result.results {
        output.push_str(&format!("{}:{}: {}\n", m.file, m.line, m.content));
    }

    if result.truncated {
        let remaining = result.total_matches - result.returned;
        output.push_str(&format!(
            "[Truncated: {} more matches. Narrow your pattern or use max_results.]",
            remaining
        ));
    }

    output
}

/// Format a `BashResult` into model-readable text with status and output.
fn format_bash_result(result: &BashResult) -> String {
    let mut output = String::new();

    let status_str = match result.status {
        BashStatus::Success => format!("exit={}", result.exit_code.unwrap_or(0)),
        BashStatus::Error => format!("exit={} | error", result.exit_code.unwrap_or(-1)),
        BashStatus::Timeout => "timeout".into(),
        BashStatus::Killed => "killed".into(),
    };

    output.push_str(&format!(
        "[Bash: {} | {} lines]\n",
        status_str, result.returned_lines
    ));

    if !result.stdout.is_empty() {
        if result.stderr.is_some() {
            output.push_str("STDOUT:\n");
        }
        output.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            output.push('\n');
        }
    }

    if let Some(stderr) = &result.stderr {
        if !stderr.is_empty() {
            output.push_str("STDERR:\n");
            output.push_str(stderr);
            if !stderr.ends_with('\n') {
                output.push('\n');
            }
        }
    }

    if result.truncated {
        output.push_str(&format!(
            "[Truncated: showing {} of {} lines]",
            result.returned_lines, result.total_lines
        ));
    }

    if let Some(hint) = &result.hint {
        output.push_str(&format!("[Hint: {}]", hint));
    }

    output
}

/// Maximum iterations in the agent loop to prevent infinite loops.
const MAX_ITERATIONS: usize = 100;

/// Agent configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum iterations before stopping.
    pub max_iterations: usize,
    /// Whether to use streaming.
    pub use_streaming: bool,
    /// Suppress streaming output (for sub-agents that only return final result).
    ///
    /// When true, intermediate results are not sent to the host.
    /// This is useful for sub-agents where only the final result matters.
    pub suppress_streaming: bool,
    /// Whether this agent is running as a sub-agent (restricts to read-only browser tools).
    pub is_subagent: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: MAX_ITERATIONS,
            use_streaming: true,
            suppress_streaming: false,
            is_subagent: false,
        }
    }
}

impl AgentConfig {
    /// Create a new config for a sub-agent with streaming suppressed.
    pub fn for_subagent() -> Self {
        Self {
            max_iterations: MAX_ITERATIONS,
            use_streaming: false,
            suppress_streaming: true,
            is_subagent: true,
        }
    }

    /// Set whether to suppress streaming output.
    pub fn with_suppress_streaming(mut self, suppress: bool) -> Self {
        self.suppress_streaming = suppress;
        self
    }
}

/// Bounding rectangle for a cached element.
#[derive(Debug, Clone)]
struct ElementRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// A cached element from browser_get_elements.
#[derive(Debug, Clone)]
struct CachedElement {
    id: String,
    role: String,
    name: String,
    selector: String,
    rect: Option<ElementRect>,
    /// Preserve all original fields for browser_element_info.
    raw: serde_json::Value,
}

/// Cache of elements from the last browser_get_elements call.
struct ElementsCache {
    elements: Vec<CachedElement>,
}

/// Interactive ARIA roles that represent actionable UI elements.
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "checkbox",
    "radio",
    "combobox",
    "menuitem",
    "tab",
    "slider",
    "switch",
    "spinbutton",
    "searchbox",
];

/// Parse element data from browser_get_elements into a cache.
///
/// Supports two formats:
/// 1. `refs` map format (from browser sidebar): `{ "refs": { "e10": {...}, ... } }`
/// 2. `elements` array format: `{ "elements": [{ "id": "e10", ... }, ...] }`
fn parse_elements_from_data(data: &serde_json::Value) -> Option<ElementsCache> {
    let mut elements = Vec::new();

    if let Some(refs) = data.get("refs").and_then(|r| r.as_object()) {
        // Format 1: refs map { "e10": { "name": "...", "role": "...", ... } }
        for (id, elem) in refs {
            let cached = parse_single_element(id, elem);
            elements.push(cached);
        }
    } else if let Some(elems) = data.get("elements").and_then(|e| e.as_array()) {
        // Format 2: elements array [{ "id": "e10", "name": "...", ... }]
        for elem in elems {
            let id = elem
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cached = parse_single_element(&id, elem);
            elements.push(cached);
        }
    } else {
        return None;
    }

    if elements.is_empty() {
        return None;
    }

    Some(ElementsCache { elements })
}

/// Extract the best CSS selector from an element's selectors array or legacy selector field.
fn extract_best_selector(elem: &serde_json::Value) -> String {
    // New format: "selectors" array of {type, strategy, value}
    if let Some(selectors) = elem.get("selectors").and_then(|v| v.as_array()) {
        // Prefer CSS selectors (skip a11y: locators)
        for s in selectors {
            if s.get("type").and_then(|t| t.as_str()) == Some("css") {
                if let Some(val) = s.get("value").and_then(|v| v.as_str()) {
                    return val.to_string();
                }
            }
        }
        // Fallback: first selector of any type
        if let Some(first) = selectors.first() {
            if let Some(val) = first.get("value").and_then(|v| v.as_str()) {
                return val.to_string();
            }
        }
    }
    // Legacy format: "selector" string
    elem.get("selector")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Parse a single element value into a CachedElement.
fn parse_single_element(id: &str, elem: &serde_json::Value) -> CachedElement {
    let role = elem
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = elem
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let selector = extract_best_selector(elem);
    let rect = elem.get("rect").and_then(|r| {
        Some(ElementRect {
            x: r.get("x")?.as_f64()?,
            y: r.get("y")?.as_f64()?,
            width: r.get("width")?.as_f64()?,
            height: r.get("height")?.as_f64()?,
        })
    });

    CachedElement {
        id: id.to_string(),
        role,
        name,
        selector,
        rect,
        raw: elem.clone(),
    }
}

/// Build a compact summary of cached elements for the LLM.
fn build_elements_summary(elements: &[CachedElement]) -> String {
    use std::collections::HashMap;

    // Count elements by role
    let mut role_counts: HashMap<&str, usize> = HashMap::new();
    for elem in elements {
        *role_counts.entry(&elem.role).or_insert(0) += 1;
    }

    // Collect interactive elements with non-empty names (max 20)
    let interactive_elements: Vec<serde_json::Value> = elements
        .iter()
        .filter(|e| {
            !e.name.is_empty()
                && INTERACTIVE_ROLES
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(&e.role))
        })
        .take(20)
        .map(|e| {
            let mut obj = serde_json::json!({
                "id": e.id,
                "role": e.role,
                "name": e.name,
            });
            if let Some(ref rect) = e.rect {
                obj["rect"] = serde_json::json!({
                    "x": rect.x as i64,
                    "y": rect.y as i64,
                    "width": rect.width as i64,
                    "height": rect.height as i64,
                });
            }
            obj
        })
        .collect();

    // Collect unnamed interactive elements (all of them)
    let unnamed_interactive: Vec<serde_json::Value> = elements
        .iter()
        .filter(|e| {
            e.name.is_empty()
                && INTERACTIVE_ROLES
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(&e.role))
        })
        .map(|e| {
            let mut obj = serde_json::json!({
                "id": e.id,
                "role": e.role,
                "name": "",
            });
            if let Some(ref rect) = e.rect {
                obj["rect"] = serde_json::json!({
                    "x": rect.x as i64,
                    "y": rect.y as i64,
                    "width": rect.width as i64,
                    "height": rect.height as i64,
                });
            }
            obj
        })
        .collect();

    let unnamed_count = unnamed_interactive.len();

    // Build role counts map sorted by count descending
    let mut sorted_roles: Vec<(&str, usize)> = role_counts.into_iter().collect();
    sorted_roles.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut roles_map = serde_json::Map::new();
    for (role, count) in sorted_roles {
        roles_map.insert(role.to_string(), serde_json::json!(count));
    }

    let summary = serde_json::json!({
        "success": true,
        "element_count": elements.len(),
        "roles": roles_map,
        "interactive_elements": interactive_elements,
        "unnamed_interactive_count": unnamed_count,
        "unnamed_interactive_elements": unnamed_interactive,
        "hint": "IMPORTANT: Do NOT summarize this data in natural language. You MUST follow this workflow: (1) Call browser_screenshot to capture the current page. (2) For unnamed elements, cross-reference their rect coordinates with the screenshot to determine their visual meaning/label. (3) Use browser_find_elements to search by role/name/selector/position. (4) Use browser_element_info(id) for full element details. (5) Output structured data, not a prose summary."
    });

    summary.to_string()
}

/// The built-in agent.
pub struct Agent<H: HostFunctions> {
    /// Host functions interface.
    host: H,
    /// Configuration.
    config: AgentConfig,
    /// Cached elements from browser_get_elements (replaced on each call).
    elements_cache: RefCell<Option<ElementsCache>>,
    /// Cached screenshot base64 from browser_screenshot (consumed once per call).
    screenshot_cache: RefCell<Option<String>>,
    /// Pending plan proposal from the plan tool (consumed by run_loop to break out).
    pending_plan: RefCell<Option<PlanProposal>>,
    /// Pending artifact from the create_artifact tool (consumed by run_loop to break out).
    pending_artifact: RefCell<Option<Artifact>>,
    /// Monotonic counter for generating unique artifact IDs.
    artifact_counter: Cell<u32>,
}

// Static base prompts, compiled into the binary
const CHAT_PROMPT: &str = include_str!("../prompts/chat.md");
const BROWSER_PROMPT: &str = include_str!("../prompts/browser.md");
const AGENT_PROMPT: &str = include_str!("../prompts/agent.md");
const SUBAGENT_BROWSER_PROMPT: &str = include_str!("../prompts/subagent_browser.md");
const SUBAGENT_AGENT_PROMPT: &str = include_str!("../prompts/subagent_agent.md");
const CODE_MODE_PROMPT: &str = "You are in Code Mode. You MUST write a Python script to accomplish the task. Do NOT make individual tool calls — write a single ```python-exec script that orchestrates everything.\n\nThe code runs in a sandboxed Python interpreter (Monty).\n\nSupported syntax: variables, def, if/elif/else, for/while, try/except, comprehensions, f-strings, lambda, slicing.\nDO NOT use: class, match/case, import, with, async/await, yield, decorators.\n\nPre-injected functions (call directly, no import):\n- read_file(path) → str\n- write_file(path, content) → str\n- list_files(path) → list[str]\n- web_search(query) → list[dict] with keys: title, url, snippet\n- fetch_page(url) → str (markdown content)\n- canvas_render(files, entry, title) → dict (renders React/Vue/Svelte app in browser canvas)\n\nReturn code in a ```python-exec block. Use ```python (without -exec) ONLY for display-only code examples shown to the user.";

impl<H: HostFunctions> Agent<H> {
    /// Create a new agent with the given host functions.
    pub fn new(host: H) -> Self {
        Self {
            host,
            config: AgentConfig::default(),
            elements_cache: RefCell::new(None),
            screenshot_cache: RefCell::new(None),
            pending_plan: RefCell::new(None),
            pending_artifact: RefCell::new(None),
            artifact_counter: Cell::new(0),
        }
    }

    /// Create a new agent with custom configuration.
    pub fn with_config(host: H, config: AgentConfig) -> Self {
        Self {
            host,
            config,
            elements_cache: RefCell::new(None),
            screenshot_cache: RefCell::new(None),
            pending_plan: RefCell::new(None),
            pending_artifact: RefCell::new(None),
            artifact_counter: Cell::new(0),
        }
    }

    /// Run the agent for a single turn.
    pub fn run(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        // Use custom system prompt if provided, otherwise use mode-based prompt
        let base_prompt = match &input.custom_system_prompt {
            Some(custom) => custom.clone(),
            None => {
                let skills = self.host.skill_list().unwrap_or_default();
                Self::build_system_prompt(input.mode, &skills, &input.available_models)
            }
        };

        // Append soul document context if available
        let base_prompt = if let Some(ref soul) = input.soul_context {
            format!("{}\n\n{}", base_prompt, soul)
        } else {
            base_prompt
        };

        // Prepend skill context with high priority if present
        let system_prompt = match &input.skill_context {
            Some(skill) => {
                let files_section = if !skill.available_files.is_empty() {
                    let file_list: String = skill
                        .available_files
                        .iter()
                        .map(|f| format!("- {}", f))
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!(
                        r#"

<available_files base_path="{}">
{}
</available_files>
To read files listed above, use the `read` tool with just the filename (e.g., `resume.md`). Do NOT fabricate absolute paths."#,
                        skill.base_path, file_list
                    )
                } else {
                    String::new()
                };

                format!(
                    r#"<CRITICAL_INSTRUCTIONS priority="highest">
The following skill instructions MUST be followed exactly. These instructions take absolute priority over all other guidance.

<skill name="{}" base_path="{}">
{}
</skill>{}
</CRITICAL_INSTRUCTIONS>

{}"#,
                    skill.name, skill.base_path, skill.content, files_section, base_prompt
                )
            }
            None => base_prompt,
        };

        let mut tools = if self.config.is_subagent {
            self.get_subagent_tools_for_mode(input.mode)
        } else {
            self.get_tools_for_mode(input.mode)
        };

        // When user attached specific tabs, update browser tool tab_id descriptions
        // to guide the LLM toward the attached tabs instead of defaulting to current_tab.
        if !input.tab_ids.is_empty() {
            let attached: Vec<String> = input
                .tab_ids
                .iter()
                .map(|t| format!("{} (\"{}\")", t.tab_id, t.tab_title))
                .collect();
            let hint = format!(
                "Tab ID. The user attached tabs: {}. Unless the user explicitly asks for the current tab, use the attached tab's ID.",
                attached.join(", ")
            );
            for tool in &mut tools {
                if matches!(
                    tool.name.as_str(),
                    "browser_get_markdown" | "browser_get_content" | "browser_screenshot"
                ) {
                    if let Some(props) = tool
                        .input_schema
                        .get_mut("properties")
                        .and_then(|p| p.as_object_mut())
                    {
                        if let Some(tab_id_prop) = props.get_mut("tab_id") {
                            tab_id_prop["description"] = serde_json::Value::String(hint.clone());
                        }
                    }
                }
            }
        }

        self.run_loop(input, &system_prompt, &tools)
    }

    /// Get tools for a specific mode.
    fn get_tools_for_mode(&self, mode: AgentMode) -> Vec<ToolDefinition> {
        match mode {
            AgentMode::Chat => self.get_chat_tools(),
            AgentMode::Browser => self.get_browser_tools(),
            AgentMode::Agent => self.get_agent_tools(),
            AgentMode::Code => self.get_agent_tools(),
        }
    }

    /// Get the static base prompt for a mode.
    fn base_prompt_for_mode(mode: AgentMode) -> &'static str {
        match mode {
            AgentMode::Chat => CHAT_PROMPT,
            AgentMode::Browser => BROWSER_PROMPT,
            AgentMode::Agent => AGENT_PROMPT,
            AgentMode::Code => CODE_MODE_PROMPT,
        }
    }

    /// Get the static subagent prompt for a mode.
    pub fn subagent_prompt_for_mode(mode: AgentMode) -> &'static str {
        match mode {
            // Chat mode doesn't expose subagent tools,
            // but fallback to browser-level if somehow called
            AgentMode::Chat => SUBAGENT_BROWSER_PROMPT,
            AgentMode::Browser => SUBAGENT_BROWSER_PROMPT,
            AgentMode::Agent => SUBAGENT_AGENT_PROMPT,
            AgentMode::Code => SUBAGENT_AGENT_PROMPT,
        }
    }

    /// Build the full system prompt with dynamic sections appended.
    /// Called once per session; result should be cached.
    fn build_system_prompt(
        mode: AgentMode,
        skills: &[SkillSummary],
        models: &[(String, String)],
    ) -> String {
        let mut prompt = Self::base_prompt_for_mode(mode).to_string();

        if !models.is_empty() {
            prompt.push_str("\n\n# Available models\n\n");
            for (provider, model) in models {
                prompt.push_str(&format!("- {}: {}\n", provider, model));
            }
        }

        if !skills.is_empty() {
            prompt.push_str("\n\n# Skills\n\n");
            prompt.push_str(&format_skill_summaries(skills));
            prompt.push_str("\n\nUse skill_load(name) to load a skill's full content.");
        }

        prompt
    }

    /// Get tools for a subagent based on mode (read-only browser access).
    fn get_subagent_tools_for_mode(&self, mode: AgentMode) -> Vec<ToolDefinition> {
        match mode {
            AgentMode::Chat => self.get_chat_tools(),
            AgentMode::Browser => self.get_subagent_browser_read_tools(),
            AgentMode::Agent | AgentMode::Code => {
                let mut tools = self.get_subagent_browser_read_tools();
                tools.extend(self.get_file_tools());
                tools
            }
        }
    }

    /// Get read-only browser tools for subagents.
    ///
    /// Includes chat tools + browser_get_content, browser_get_markdown, browser_screenshot.
    /// Does NOT include interaction tools (click, type, fill, scroll, navigate, etc.).
    fn get_subagent_browser_read_tools(&self) -> Vec<ToolDefinition> {
        // Start with chat tools (think, plan, switch_model, web_search, web_fetch, ask_user,
        // memory_search, skill_load, browser_get_content, browser_get_markdown, browser_screenshot)
        // Chat tools already include browser_get_content, browser_get_markdown, browser_screenshot
        self.get_chat_tools()
    }

    /// Get file/system tools (read, write, edit, bash, glob, grep, tool_search, tool_call_dynamic).
    fn get_file_tools(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "read".into(),
                description: "Read file contents. Returns partial content with metadata (total_lines, total_bytes). Default: first 200 lines. Use offset/limit to paginate.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The absolute path to read"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Line offset to start reading from"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum lines to read"
                        }
                    },
                    "required": ["file_path"]
                }),
            },
            ToolDefinition {
                name: "write".into(),
                description: "Write content to a file".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The absolute path to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write"
                        }
                    },
                    "required": ["file_path", "content"]
                }),
            },
            ToolDefinition {
                name: "edit".into(),
                description: "Edit a file using search and replace".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The file to edit"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "The text to find"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "The replacement text"
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace all occurrences"
                        }
                    },
                    "required": ["file_path", "old_string", "new_string"]
                }),
            },
            ToolDefinition {
                name: "bash".into(),
                description: "Execute a shell command. Default timeout: 30s. Output capped at 200 lines.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The command to execute"
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Timeout in milliseconds"
                        }
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "glob".into(),
                description: "Find files matching a pattern".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern like '**/*.rs'"
                        },
                        "path": {
                            "type": "string",
                            "description": "Base directory"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDefinition {
                name: "grep".into(),
                description: "Search file contents with regex. Returns structured matches with counts.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Search pattern (regex)"
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory to search"
                        },
                        "type": {
                            "type": "string",
                            "description": "File type filter (e.g., 'rs', 'py', 'js')"
                        },
                        "case_insensitive": {
                            "type": "boolean",
                            "description": "Case insensitive search (default: false)"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum results to return (default: 50)"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        ]
    }

    /// Core agent loop.
    fn run_loop(
        &self,
        input: &AgentInput,
        system_prompt: &str,
        tools: &[ToolDefinition],
    ) -> HostResult<AgentOutput> {
        let mut messages = vec![Message::system(system_prompt)];
        messages.extend(input.history.clone());

        // Build context prefixes for user message
        let local_files_prefix = format_local_files(&input.local_files);
        // Build active_tab TabInfo from tab_id if we don't have it in tab_ids
        let active_tab_info = input.tab_id.map(|id| {
            input
                .tab_ids
                .iter()
                .find(|t| t.tab_id == id)
                .cloned()
                .unwrap_or(TabInfo {
                    tab_id: id,
                    tab_title: String::new(),
                    url: String::new(),
                    space: String::new(),
                })
        });
        let tab_context_prefix = Self::format_tab_context(active_tab_info.as_ref(), &input.tab_ids);

        // For browser/agent mode: take initial viewport snapshot and append to user message
        let initial_snapshot = if matches!(input.mode, AgentMode::Browser | AgentMode::Agent) {
            let snapshot_text = self.get_viewport_snapshot_text(input.tab_id);
            if !snapshot_text.is_empty() {
                format!("\n\nCurrent page state:\n{}", snapshot_text)
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Combine prefixes with user message
        let user_content = match (local_files_prefix.is_empty(), tab_context_prefix.is_empty()) {
            (true, true) => input.user_message.clone(),
            (false, true) => format!("{}{}", local_files_prefix, input.user_message),
            (true, false) => format!("{}\n\n{}", tab_context_prefix, input.user_message),
            (false, false) => format!(
                "{}{}\n\n{}",
                local_files_prefix, tab_context_prefix, input.user_message
            ),
        };
        let user_content = format!("{}{}", user_content, initial_snapshot);

        // Create user message with optional attachments
        if input.attachments.is_empty() {
            messages.push(Message::user(&user_content));
        } else {
            messages.push(Message::user_with_attachments(
                &user_content,
                input.attachments.clone(),
            ));
        }

        let mut iterations = 0;
        let mut final_text = String::new();
        let mut all_tool_calls = Vec::new();

        loop {
            iterations += 1;
            let _ = self.host.set_iteration(iterations as u32);
            if iterations > self.config.max_iterations {
                break;
            }

            // Check for interrupt signal from sidebar
            if self.host.is_interrupted()? {
                break;
            }

            // Use streaming or non-streaming LLM based on config
            let response = if self.config.use_streaming && !self.config.suppress_streaming {
                self.call_llm_streaming(&messages, tools)?
            } else {
                // Call LLM non-streaming
                let request = LlmRequest {
                    messages: messages.clone(),
                    tools: tools.to_vec(),
                    stream: false,
                };
                self.host.llm_chat(&request)?
            };

            // If no tool calls, we're done
            if response.tool_calls.is_empty() {
                final_text = response.text;
                break;
            }

            // Execute tool calls - must include tool_calls in the assistant message
            messages.push(Message::assistant_with_tool_calls(
                &response.text,
                response.tool_calls.clone(),
            ));
            all_tool_calls.extend(response.tool_calls.clone());

            for tool_call in &response.tool_calls {
                eprintln!(
                    "[AGENT] Executing tool: name={}, id={}, call_id={:?}, args={}",
                    tool_call.name, tool_call.id, tool_call.call_id, tool_call.arguments
                );
                let result = self.execute_tool(tool_call)?;
                eprintln!(
                    "[AGENT] Tool result will use tool_call_id={}",
                    result.tool_call_id
                );
                // Find safe UTF-8 boundary for preview (handles multi-byte chars like Chinese)
                let preview = truncate_string_safe(&result.content, 200);
                eprintln!(
                    "[AGENT] Tool result: success={}, content_len={}, content={:?}",
                    result.success,
                    result.content.len(),
                    preview
                );

                // Dynamic truncation based on current message size
                let content = truncate_tool_result_if_needed(&messages, &result.content);

                // Check if there's a cached screenshot to attach (base64 stays out of content)
                let attachments = if tool_call.name == "browser_screenshot" {
                    self.screenshot_cache
                        .borrow_mut()
                        .take()
                        .map(|base64| {
                            // Detect actual image format from base64 magic bytes
                            let mime_type: String = if base64.starts_with("/9j/") {
                                "image/jpeg"
                            } else {
                                "image/png" // PNG or default fallback
                            }
                            .into();
                            vec![Attachment {
                                name: "screenshot.png".into(),
                                mime_type,
                                data: base64,
                            }]
                        })
                        .unwrap_or_default()
                } else {
                    vec![]
                };

                // Use call_id (from result.tool_call_id) for the tool message
                // This was set in execute_tool to use call_id when available
                messages.push(Message {
                    role: MessageRole::Tool,
                    content,
                    tool_call_id: Some(result.tool_call_id.clone()),
                    tool_calls: vec![],
                    attachments,
                });

                // Check interrupt after each tool execution
                if self.host.is_interrupted()? {
                    break;
                }
            }

            // Check if we should exit the outer loop due to interrupt
            if self.host.is_interrupted()? {
                break;
            }

            // Check if a plan was proposed — break out and return to runner
            if let Some(proposal) = self.pending_plan.borrow_mut().take() {
                // Signal end of stream if streaming was enabled
                if self.config.use_streaming && !self.config.suppress_streaming {
                    let _ = self.host.stream_end();
                }

                return Ok(AgentOutput {
                    text: final_text,
                    tool_calls: all_tool_calls,
                    continue_loop: false,
                    plan_proposal: Some(proposal),
                    artifact: None,
                });
            }

            // Check if an artifact was created — break out and return to runner
            if let Some(artifact) = self.pending_artifact.borrow_mut().take() {
                if self.config.use_streaming && !self.config.suppress_streaming {
                    let _ = self.host.stream_end();
                }

                return Ok(AgentOutput {
                    text: final_text,
                    tool_calls: all_tool_calls,
                    continue_loop: false,
                    plan_proposal: None,
                    artifact: Some(artifact),
                });
            }
        }

        // Signal end of stream if streaming was enabled
        if self.config.use_streaming && !self.config.suppress_streaming {
            let _ = self.host.stream_end();
        }

        Ok(AgentOutput {
            text: final_text,
            tool_calls: all_tool_calls,
            continue_loop: false,
            plan_proposal: None,
            artifact: None,
        })
    }

    /// Call LLM with streaming support.
    ///
    /// This method starts a stream, emits chunks to the sidebar, and returns
    /// the accumulated response.
    fn call_llm_streaming(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> HostResult<LlmResponse> {
        let request = LlmRequest {
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            stream: true,
        };

        // Start the stream
        let stream_id = self.host.llm_stream_start(&request)?;

        let mut accumulated_text = String::new();
        // Use a HashMap to deduplicate tool calls by id, preferring those with call_id set
        let mut tool_calls_map: std::collections::HashMap<String, ToolCall> =
            std::collections::HashMap::new();

        // Buffering state for text-based <tool_call> XML that may span multiple chunks
        let mut tool_call_buf = String::new();
        let mut in_tool_call = false;
        // Buffer for partial <tool_call> tag prefix split at chunk boundary
        let mut tag_buf = String::new();

        // Read chunks until done
        loop {
            // Check for interrupt
            if self.host.is_interrupted()? {
                self.host.llm_stream_close(stream_id)?;
                break;
            }

            match self.host.llm_stream_next(stream_id)? {
                Some(chunk) => {
                    // Accumulate text, detecting <tool_call> XML that some providers
                    // emit as plain text instead of structured tool_use blocks.
                    // The opening tag may be split across chunks (e.g. "<tool_call"
                    // in one chunk and ">" in the next), so we buffer partial prefixes.
                    if let Some(ref chunk_text) = chunk.text {
                        if !chunk_text.is_empty() {
                            // Prepend any partial tag prefix from previous chunk
                            let text = if !tag_buf.is_empty() {
                                std::mem::take(&mut tag_buf) + chunk_text
                            } else {
                                chunk_text.to_string()
                            };
                            let text = text.as_str();

                            if in_tool_call {
                                // Continue buffering a multi-chunk tool call
                                tool_call_buf.push_str(text);
                                // Check accumulated buffer (not just current chunk)
                                // so split </tool_call> tags are handled correctly
                                if tool_call_buf.contains("</tool_call>") {
                                    in_tool_call = false;
                                    let complete = std::mem::take(&mut tool_call_buf);
                                    let (clean, extracted) = parse_tool_calls_from_text(&complete);
                                    for tc in extracted {
                                        tool_calls_map.insert(tc.id.clone(), tc);
                                    }
                                    if !clean.is_empty() {
                                        accumulated_text.push_str(&clean);
                                        self.host.stream_emit(&clean)?;
                                    }
                                }
                            } else if text.contains("<tool_call>") {
                                if text.contains("</tool_call>") {
                                    // Complete tool call in a single chunk
                                    let (clean, extracted) = parse_tool_calls_from_text(text);
                                    for tc in extracted {
                                        tool_calls_map.insert(tc.id.clone(), tc);
                                    }
                                    if !clean.is_empty() {
                                        accumulated_text.push_str(&clean);
                                        self.host.stream_emit(&clean)?;
                                    }
                                } else {
                                    // Starts here, doesn't end — buffer
                                    let idx = text.find("<tool_call>").unwrap();
                                    let before = &text[..idx];
                                    if !before.trim().is_empty() {
                                        accumulated_text.push_str(before);
                                        self.host.stream_emit(before)?;
                                    }
                                    tool_call_buf = text[idx..].to_string();
                                    in_tool_call = true;
                                }
                            } else if let Some(split) = find_tool_call_tag_prefix_at_end(text) {
                                // Text ends with a partial <tool_call> prefix —
                                // hold it back until the next chunk confirms
                                let (emit, hold) = text.split_at(split);
                                tag_buf = hold.to_string();
                                if !emit.is_empty() {
                                    accumulated_text.push_str(emit);
                                    self.host.stream_emit(emit)?;
                                }
                            } else {
                                // Normal text — no tool call markers
                                accumulated_text.push_str(text);
                                self.host.stream_emit(text)?;
                            }
                        }
                    }

                    // Accumulate tool calls, preferring those with call_id set
                    // This handles the case where OpenAI Responses API sends both
                    // delta-accumulated tool calls (without call_id) and complete
                    // tool calls (with call_id) for the same id
                    for tc in chunk.tool_calls {
                        let should_insert = match tool_calls_map.get(&tc.id) {
                            None => true,
                            Some(existing) => {
                                // Prefer the one with call_id, or the newer one if both have it
                                tc.call_id.is_some() || existing.call_id.is_none()
                            }
                        };
                        if should_insert {
                            tool_calls_map.insert(tc.id.clone(), tc);
                        }
                    }

                    if chunk.done {
                        // Flush incomplete tag prefix buffer as plain text
                        if !tag_buf.is_empty() {
                            let buf = std::mem::take(&mut tag_buf);
                            accumulated_text.push_str(&buf);
                            self.host.stream_emit(&buf)?;
                        }
                        // Handle incomplete tool call buffer — the model
                        // truncated output before sending </tool_call>.
                        // Don't dump raw XML to the sidebar; try to extract
                        // the tool name for a helpful warning instead.
                        if in_tool_call {
                            let buf = std::mem::take(&mut tool_call_buf);
                            // Try to identify which tool was being called
                            let tool_name = buf
                                .find("\"name\"")
                                .and_then(|i| {
                                    let after = &buf[i + 6..];
                                    let colon = after.find(':')?;
                                    let after_colon = after[colon + 1..].trim_start();
                                    let quote_start = after_colon.strip_prefix('"')?;
                                    let end = quote_start.find('"')?;
                                    Some(&quote_start[..end])
                                })
                                .unwrap_or("unknown");
                            let warning =
                                format!("\n[Tool call `{}` was truncated by the model]", tool_name);
                            accumulated_text.push_str(&warning);
                            self.host.stream_emit(&warning)?;
                        }
                        break;
                    }
                }
                None => {
                    // No more chunks available, wait a bit and try again
                    // In WASM context, we might need to yield
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }

        // Close the stream
        self.host.llm_stream_close(stream_id)?;

        Ok(LlmResponse {
            text: accumulated_text,
            tool_calls: tool_calls_map.into_values().collect(),
        })
    }

    /// Execute a single tool call.
    fn execute_tool(&self, tool_call: &ToolCall) -> HostResult<ToolResult> {
        let content = match tool_call.name.as_str() {
            "think" => {
                // Think tool: no side effects, just returns acknowledgment.
                // The thought content is recorded in trace via the tool_call arguments.
                r#"{"status":"ok"}"#.to_string()
            }
            "plan" => {
                let summary = tool_call.arguments["summary"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let steps: Vec<PlanStep> = tool_call.arguments["steps"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|s| PlanStep {
                                description: s["description"].as_str().unwrap_or("").to_string(),
                                model: s["model"].as_str().map(|m| m.to_string()),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                *self.pending_plan.borrow_mut() = Some(PlanProposal { summary, steps });

                "Plan submitted for user review.".to_string()
            }
            "create_artifact" => {
                let title = tool_call.arguments["title"]
                    .as_str()
                    .unwrap_or("Untitled")
                    .to_string();
                let content_type = tool_call.arguments["content_type"]
                    .as_str()
                    .unwrap_or("text/html")
                    .to_string();
                let content = tool_call.arguments["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                // Extract optional multi-file project fields.
                // Accept both object (Anthropic) and JSON string (OpenAI strict mode).
                let files: Option<std::collections::HashMap<String, String>> =
                    tool_call.arguments.get("files").and_then(|f| {
                        // Try as object first (Anthropic/non-strict providers)
                        if let Some(obj) = f.as_object() {
                            Some(
                                obj.iter()
                                    .filter_map(|(k, v)| {
                                        v.as_str().map(|s| (k.clone(), s.to_string()))
                                    })
                                    .collect(),
                            )
                        } else if let Some(s) = f.as_str() {
                            // Try as JSON string (OpenAI strict mode)
                            serde_json::from_str(s).ok()
                        } else {
                            None
                        }
                    });
                let entry = tool_call
                    .arguments
                    .get("entry")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string());
                let description = tool_call
                    .arguments
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string());
                // Generate a unique artifact ID using timestamp + counter.
                // LLM-generated tool call IDs (e.g. "call_1") reset each turn
                // and would cause collisions, so we use server-side generation.
                let seq = self.artifact_counter.get() + 1;
                self.artifact_counter.set(seq);
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let id = format!("art-{}-{}", ts, seq);

                *self.pending_artifact.borrow_mut() = Some(Artifact {
                    id: id.clone(),
                    title,
                    content_type,
                    description,
                    content,
                    files,
                    entry,
                });

                format!("Artifact created and sent to canvas: {}", id)
            }
            "switch_model" => {
                let provider = tool_call.arguments["provider"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let model = tool_call.arguments["model"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();

                if provider.is_empty() || model.is_empty() {
                    r#"{"error":"provider and model are required"}"#.to_string()
                } else {
                    match self.host.set_model_override(&provider, &model) {
                        Ok(()) => format!(
                            r#"{{"status":"ok","provider":"{}","model":"{}"}}"#,
                            provider, model
                        ),
                        Err(e) => format!(r#"{{"error":"{}"}}"#, e.message),
                    }
                }
            }
            "web_search" => {
                let query = tool_call.arguments["query"].as_str().unwrap_or("");
                self.host.tool_web_search(query)?
            }
            "web_fetch" => {
                let url = tool_call.arguments["url"].as_str().unwrap_or("");
                let prompt = tool_call.arguments["prompt"]
                    .as_str()
                    .unwrap_or("Extract the main content");
                self.host.tool_web_fetch(url, prompt)?
            }
            "ask_user" => {
                let question = tool_call.arguments["question"].as_str().unwrap_or("");
                let options: Vec<String> = tool_call.arguments["options"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                self.host.tool_ask_user(question, &options)?
            }
            "read" => {
                let path = tool_call.arguments["file_path"].as_str().unwrap_or("");
                let offset = tool_call.arguments["offset"].as_u64();
                let limit = tool_call.arguments["limit"].as_u64();
                let result = self.host.tool_read(path, offset, limit)?;
                format_read_result(&result, path)
            }
            "write" => {
                let path = tool_call.arguments["file_path"].as_str().unwrap_or("");
                let content = tool_call.arguments["content"].as_str().unwrap_or("");
                self.host.tool_write(path, content)?;
                "File written successfully.".to_string()
            }
            "edit" => {
                let path = tool_call.arguments["file_path"].as_str().unwrap_or("");
                let old_string = tool_call.arguments["old_string"].as_str().unwrap_or("");
                let new_string = tool_call.arguments["new_string"].as_str().unwrap_or("");
                let replace_all = tool_call.arguments["replace_all"]
                    .as_bool()
                    .unwrap_or(false);
                self.host
                    .tool_edit(path, old_string, new_string, replace_all)?;
                "File edited successfully.".to_string()
            }
            "bash" => {
                let command = tool_call.arguments["command"].as_str().unwrap_or("");
                let timeout = tool_call.arguments["timeout"].as_u64();
                let result = self.host.tool_bash(command, timeout)?;
                format_bash_result(&result)
            }
            "glob" => {
                let pattern = tool_call.arguments["pattern"].as_str().unwrap_or("*");
                let path = tool_call.arguments["path"].as_str();
                let files = self.host.tool_glob(pattern, path)?;
                files.join("\n")
            }
            "grep" => {
                let pattern = tool_call.arguments["pattern"].as_str().unwrap_or("");
                let path = tool_call.arguments["path"].as_str();
                let file_type = tool_call.arguments["type"].as_str();
                let case_insensitive = tool_call.arguments["case_insensitive"].as_bool();
                let max_results = tool_call.arguments["max_results"].as_u64();
                let result =
                    self.host
                        .tool_grep(pattern, path, file_type, case_insensitive, max_results)?;
                format_grep_result(&result, pattern)
            }
            "memory_search" => {
                let query = tool_call.arguments["query"].as_str().unwrap_or("");
                let limit = tool_call.arguments["limit"].as_u64().unwrap_or(10) as usize;
                let chunks = self.host.memory_search(query, limit)?;
                serde_json::to_string_pretty(&chunks).unwrap_or_default()
            }
            "memory_create" => {
                let content = tool_call.arguments["content"].as_str().unwrap_or("");
                let metadata = tool_call
                    .arguments
                    .get("metadata")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let id = self.host.memory_create(content, &metadata)?;
                serde_json::json!({"id": id, "status": "created"}).to_string()
            }
            "memory_update" => {
                let id = tool_call.arguments["id"].as_str().unwrap_or("");
                let content = tool_call.arguments["content"].as_str().unwrap_or("");
                self.host.memory_update(id, content)?;
                serde_json::json!({"id": id, "status": "updated"}).to_string()
            }
            "memory_delete" => {
                let id = tool_call.arguments["id"].as_str().unwrap_or("");
                self.host.memory_delete(id)?;
                serde_json::json!({"id": id, "status": "deleted"}).to_string()
            }
            "knowledge_teach" => {
                let category = tool_call.arguments["category"]
                    .as_str()
                    .unwrap_or("user_preference");
                let summary = tool_call.arguments["summary"].as_str().unwrap_or("");
                let details = tool_call.arguments["details"].as_str().unwrap_or("");
                let domain = tool_call.arguments.get("domain").and_then(|v| v.as_str());
                let id = self
                    .host
                    .knowledge_teach(category, summary, details, domain)?;
                serde_json::json!({"id": id, "status": "taught"}).to_string()
            }
            "skill_load" => {
                let name = tool_call.arguments["name"].as_str().unwrap_or("");
                self.host.skill_load(name)?
            }
            "tool_search" => {
                let query = tool_call.arguments["query"].as_str().unwrap_or("");
                let max_results = tool_call.arguments["max_results"].as_u64().unwrap_or(5) as usize;
                let results = self.host.tool_search(query, max_results)?;
                serde_json::to_string_pretty(&results).unwrap_or_default()
            }
            "tool_call_dynamic" => {
                let tool_name = tool_call.arguments["tool_name"].as_str().unwrap_or("");
                let arguments_str = tool_call.arguments["arguments"].as_str().unwrap_or("{}");
                let arguments: serde_json::Value =
                    serde_json::from_str(arguments_str).unwrap_or(serde_json::json!({}));
                self.host.tool_call_dynamic(tool_name, &arguments)?
            }
            // Computer tools
            "computer_screenshot" => {
                let monitor = tool_call.arguments.get("monitor").and_then(|v| v.as_i64());
                self.host.computer_screenshot(monitor)?
            }
            "computer_mouse_move" => {
                let x = tool_call.arguments["x"].as_i64().unwrap_or(0);
                let y = tool_call.arguments["y"].as_i64().unwrap_or(0);
                let click = tool_call.arguments.get("click").and_then(|v| v.as_str());
                self.host.computer_mouse_move(x, y, click)?
            }
            "computer_click" => {
                let x = tool_call.arguments["x"].as_i64().unwrap_or(0);
                let y = tool_call.arguments["y"].as_i64().unwrap_or(0);
                let button = tool_call.arguments.get("button").and_then(|v| v.as_str());
                let click_type = tool_call
                    .arguments
                    .get("click_type")
                    .and_then(|v| v.as_str());
                self.host.computer_click(x, y, button, click_type)?
            }
            "computer_type_text" => {
                let text = tool_call.arguments["text"].as_str().unwrap_or("");
                let delay_ms = tool_call.arguments.get("delay_ms").and_then(|v| v.as_u64());
                self.host.computer_type_text(text, delay_ms)?
            }
            "computer_key" => {
                let key = tool_call.arguments["key"].as_str().unwrap_or("");
                let modifiers: Vec<String> = tool_call
                    .arguments
                    .get("modifiers")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let repeat = tool_call.arguments.get("repeat").and_then(|v| v.as_u64());
                self.host.computer_key(key, &modifiers, repeat)?
            }
            "computer_scroll" => {
                let x = tool_call.arguments["x"].as_i64().unwrap_or(0);
                let y = tool_call.arguments["y"].as_i64().unwrap_or(0);
                let direction = tool_call.arguments["direction"].as_str().unwrap_or("down");
                let amount = tool_call.arguments.get("amount").and_then(|v| v.as_u64());
                self.host.computer_scroll(x, y, direction, amount)?
            }
            // Browser tools
            "browser_navigate" => {
                let url = tool_call.arguments["url"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_navigate(url, tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "navigation", tab_id)
            }
            "browser_go_back" => {
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_go_back(tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "navigation", tab_id)
            }
            "browser_go_forward" => {
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_go_forward(tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "navigation", tab_id)
            }
            "browser_click" => {
                let selector = tool_call.arguments["selector"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_click(selector, tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "interaction", tab_id)
            }
            "browser_click_by_id" => {
                let element_id = tool_call.arguments["element_id"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_click_by_id(element_id, tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "interaction", tab_id)
            }
            "browser_type" => {
                let selector = tool_call.arguments["selector"].as_str().unwrap_or("");
                let text = tool_call.arguments["text"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_type(selector, text, tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "interaction", tab_id)
            }
            "browser_type_by_id" => {
                let element_id = tool_call.arguments["element_id"].as_str().unwrap_or("");
                let text = tool_call.arguments["text"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_type_by_id(element_id, text, tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "interaction", tab_id)
            }
            "browser_fill" => {
                let selector = tool_call.arguments["selector"].as_str().unwrap_or("");
                let value = tool_call.arguments["value"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_fill(selector, value, tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "interaction", tab_id)
            }
            "browser_fill_by_id" => {
                let element_id = tool_call.arguments["element_id"].as_str().unwrap_or("");
                let value = tool_call.arguments["value"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_fill_by_id(element_id, value, tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "interaction", tab_id)
            }
            "browser_get_content" => {
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_get_content(tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_get_markdown" => {
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_get_markdown(tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_screenshot" => {
                let full_page = tool_call.arguments["full_page"].as_bool().unwrap_or(false);
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_screenshot(full_page, tab_id)?;
                if let Some(base64_data) = &result.screenshot {
                    // Cache the screenshot for attachment (stripped from content)
                    *self.screenshot_cache.borrow_mut() = Some(base64_data.clone());
                    r#"{"success":true,"screenshot_available":true}"#.to_string()
                } else {
                    serde_json::to_string(&result).unwrap_or_default()
                }
            }
            "browser_eval_js" => {
                let script = tool_call.arguments["script"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_eval_js(script, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_scroll" => {
                let direction = tool_call.arguments["direction"].as_str().unwrap_or("down");
                let amount = tool_call.arguments["amount"].as_str().unwrap_or("page");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_scroll(direction, amount, tab_id)?;
                let result_str = serde_json::to_string(&result).unwrap_or_default();
                self.auto_snapshot_after_action(&result_str, "scroll", tab_id)
            }
            "browser_wait_for" => {
                let selector = tool_call.arguments["selector"].as_str().unwrap_or("");
                let timeout_ms = tool_call.arguments["timeout_ms"].as_u64().unwrap_or(10000);
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_wait_for(selector, timeout_ms, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_get_elements" => {
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_get_elements(tab_id, None)?;
                // Parse and cache elements, return compact summary
                if let Some(data) = &result.data {
                    if let Some(cache) = parse_elements_from_data(data) {
                        let summary = build_elements_summary(&cache.elements);
                        *self.elements_cache.borrow_mut() = Some(cache);
                        summary
                    } else {
                        // Parse failed — fallback to original behavior
                        serde_json::to_string(&result).unwrap_or_default()
                    }
                } else {
                    serde_json::to_string(&result).unwrap_or_default()
                }
            }
            "browser_find_elements" => {
                let cache = self.elements_cache.borrow();
                match cache.as_ref() {
                    None => r#"{"success":false,"error":"No elements cached. Call browser_get_elements first."}"#.to_string(),
                    Some(cache) => {
                        let role_filter = tool_call.arguments.get("role").and_then(|v| v.as_str());
                        let name_filter = tool_call.arguments.get("name").and_then(|v| v.as_str());
                        let selector_filter = tool_call.arguments.get("selector").and_then(|v| v.as_str());
                        let near_x = tool_call.arguments.get("near_x").and_then(|v| v.as_f64());
                        let near_y = tool_call.arguments.get("near_y").and_then(|v| v.as_f64());
                        let radius = tool_call.arguments.get("radius").and_then(|v| v.as_f64()).unwrap_or(50.0);
                        let unnamed_only = tool_call.arguments.get("unnamed_only").and_then(|v| v.as_bool()).unwrap_or(false);
                        let limit = tool_call.arguments.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

                        let results: Vec<serde_json::Value> = cache
                            .elements
                            .iter()
                            .filter(|e| {
                                if let Some(role) = role_filter {
                                    if !e.role.eq_ignore_ascii_case(role) {
                                        return false;
                                    }
                                }
                                if let Some(name) = name_filter {
                                    if !e.name.to_lowercase().contains(&name.to_lowercase()) {
                                        return false;
                                    }
                                }
                                if let Some(sel) = selector_filter {
                                    if !e.selector.contains(sel) {
                                        return false;
                                    }
                                }
                                if unnamed_only && !e.name.is_empty() {
                                    return false;
                                }
                                if let (Some(nx), Some(ny)) = (near_x, near_y) {
                                    if let Some(ref rect) = e.rect {
                                        let cx = rect.x + rect.width / 2.0;
                                        let cy = rect.y + rect.height / 2.0;
                                        let dist = ((cx - nx).powi(2) + (cy - ny).powi(2)).sqrt();
                                        if dist > radius {
                                            return false;
                                        }
                                    } else {
                                        return false;
                                    }
                                }
                                true
                            })
                            .take(limit)
                            .map(|e| {
                                let mut obj = serde_json::json!({
                                    "id": e.id,
                                    "role": e.role,
                                    "name": e.name,
                                    "selector": e.selector,
                                });
                                if let Some(ref rect) = e.rect {
                                    obj["rect"] = serde_json::json!({
                                        "x": rect.x as i64,
                                        "y": rect.y as i64,
                                        "width": rect.width as i64,
                                        "height": rect.height as i64,
                                    });
                                }
                                obj
                            })
                            .collect();

                        serde_json::json!({
                            "success": true,
                            "count": results.len(),
                            "elements": results,
                        })
                        .to_string()
                    }
                }
            }
            "browser_element_info" => {
                let id = tool_call
                    .arguments
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let cache = self.elements_cache.borrow();
                match cache.as_ref() {
                    None => r#"{"success":false,"error":"No elements cached. Call browser_get_elements first."}"#.to_string(),
                    Some(cache) => {
                        match cache.elements.iter().find(|e| e.id == id) {
                            Some(el) => serde_json::json!({
                                "success": true,
                                "element": el.raw,
                            })
                            .to_string(),
                            None => format!(r#"{{"success":false,"error":"Element '{}' not found"}}"#, id),
                        }
                    }
                }
            }
            // Artifact editing tools
            "browser_read_artifact" => {
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let params = serde_json::json!({
                    "id": tool_call.arguments.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                    "offset": tool_call.arguments.get("offset"),
                    "limit": tool_call.arguments.get("limit"),
                    "grep": tool_call.arguments.get("grep"),
                    "context": tool_call.arguments.get("context"),
                });
                let result = self.host.browser_read_artifact(&params, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_edit_artifact" => {
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let params = serde_json::json!({
                    "id": tool_call.arguments.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                    "old_str": tool_call.arguments.get("old_str").and_then(|v| v.as_str()).unwrap_or(""),
                    "new_str": tool_call.arguments.get("new_str").and_then(|v| v.as_str()).unwrap_or(""),
                });
                let result = self.host.browser_edit_artifact(&params, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            // Subagent tools
            "subagent_spawn" => {
                let task = tool_call.arguments["task"].as_str().unwrap_or("");
                let mode = tool_call.arguments["mode"].as_str().unwrap_or("agent");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let id = self.host.subagent_spawn(task, mode, tab_id)?;
                format!("Spawned sub-agent with ID: {}", id)
            }
            "subagent_wait_all" => {
                let ids: Vec<u64> = tool_call.arguments["ids"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
                    .unwrap_or_default();
                self.host.subagent_wait_all(&ids)?
            }
            "subagent_status" => {
                let id = tool_call.arguments["id"].as_u64().unwrap_or(0);
                let status = self.host.subagent_status(id)?;
                format!("Sub-agent {} status: {}", id, status)
            }
            "subagent_wait" => {
                let id = tool_call.arguments["id"].as_u64().unwrap_or(0);
                self.host.subagent_wait(id)?
            }
            "subagent_kill" => {
                let id = tool_call.arguments["id"].as_u64().unwrap_or(0);
                let killed = self.host.subagent_kill(id)?;
                if killed {
                    format!("Sub-agent {} was terminated", id)
                } else {
                    format!("Sub-agent {} had already completed", id)
                }
            }
            "subagent_list" => {
                let list = self.host.subagent_list()?;
                serde_json::to_string_pretty(&list).unwrap_or_default()
            }
            _ => {
                format!("Unknown tool: {}", tool_call.name)
            }
        };

        // Use call_id if available, otherwise fall back to id
        // OpenAI Responses API requires call_id to match tool results
        let tool_call_id = tool_call
            .call_id
            .clone()
            .unwrap_or_else(|| tool_call.id.clone());
        Ok(ToolResult {
            tool_call_id,
            content,
            success: true,
        })
    }

    /// Execute wait-for-stable + viewport snapshot and append to action result.
    fn auto_snapshot_after_action(
        &self,
        action_result: &str,
        wait_strategy: &str,
        tab_id: Option<i64>,
    ) -> String {
        // 1. Wait for page stability (ignore errors — best effort)
        let _ = self
            .host
            .browser_wait_for_stable(wait_strategy, 3000, tab_id);

        // 2. Take viewport snapshot
        let snapshot_text = self.get_viewport_snapshot_text(tab_id);

        if snapshot_text.is_empty() {
            action_result.to_string()
        } else {
            format!(
                "{}\n\nCurrent page state:\n{}",
                action_result, snapshot_text
            )
        }
    }

    /// Get viewport snapshot text for initial context.
    fn get_viewport_snapshot_text(&self, tab_id: Option<i64>) -> String {
        match self.host.browser_viewport_snapshot(tab_id) {
            Ok(result) => {
                if let Some(data) = &result.data {
                    if let Some(result_obj) = data.get("result") {
                        result_obj
                            .get("tree")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string()
                    } else if let Some(tree) = data.get("tree") {
                        tree.as_str().unwrap_or("").to_string()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            }
            Err(_) => String::new(),
        }
    }

    /// Format tab context as pure data for injection into user messages.
    fn format_tab_context(active_tab: Option<&TabInfo>, extra_tabs: &[TabInfo]) -> String {
        if extra_tabs.is_empty() && active_tab.is_none() {
            return String::new();
        }

        let mut ctx = String::from("\n\n## Active Tabs\n");

        if !extra_tabs.is_empty() {
            // When the user attaches specific tabs, those are the PRIMARY targets
            // of their request (e.g. "summarize this page" refers to the attached
            // tabs, not necessarily the sidebar's current_tab).
            ctx.push_str("IMPORTANT: The user has explicitly attached the tabs listed below. Unless the user explicitly asks to read the \"current tab\", you should use browser_get_markdown with the attached tab's tab_id to read their content. current_tab only indicates which tab the sidebar panel is open on.\n\n");
            let mut by_space: std::collections::BTreeMap<&str, Vec<&TabInfo>> =
                std::collections::BTreeMap::new();
            for tab in extra_tabs {
                let space = if tab.space.is_empty() {
                    "Default"
                } else {
                    &tab.space
                };
                by_space.entry(space).or_default().push(tab);
            }
            for (space, tabs) in &by_space {
                ctx.push_str(&format!("[{}]\n", space));
                for tab in tabs {
                    ctx.push_str(&format!(
                        "- {}: \"{}\" | {}\n",
                        tab.tab_id, tab.tab_title, tab.url
                    ));
                }
            }
            if let Some(tab) = active_tab {
                ctx.push_str(&format!("current_tab: {}\n", tab.tab_id));
            }
        } else if let Some(tab) = active_tab {
            ctx.push_str(&format!(
                "current_tab: {} | \"{}\" | {}\n",
                tab.tab_id, tab.tab_title, tab.url
            ));
        }

        ctx
    }

    /// Get available tools for chat mode.
    fn get_chat_tools(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "think".into(),
                description: "Use this tool to think through problems step by step before acting. Analyze the situation, reason about the best approach, reflect on results, or plan your next move. This tool has no side effects - it simply records your thought process.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "thought": {
                            "type": "string",
                            "description": "Your thought process, reasoning, or analysis"
                        }
                    },
                    "required": ["thought"]
                }),
            },
            ToolDefinition {
                name: "plan".into(),
                description: "Create an execution plan for the user to review and approve. The plan will be displayed in the sidebar. The user can provide feedback via chat to request changes, and you should revise and call plan() again. Use this for multi-step tasks.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "summary": {
                            "type": "string",
                            "description": "Brief overview of what the plan accomplishes"
                        },
                        "steps": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "description": {
                                        "type": "string",
                                        "description": "What this step does"
                                    },
                                    "model": {
                                        "type": "string",
                                        "description": "Optional: suggested model for this step"
                                    }
                                },
                                "required": ["description"]
                            },
                            "description": "Ordered list of steps"
                        }
                    },
                    "required": ["summary", "steps"]
                }),
            },
            ToolDefinition {
                name: "create_artifact".into(),
                description: "Create a rich artifact that opens in the browser canvas. For single-file content (HTML page, document), provide 'content'. For multi-file projects (React/Vue/Svelte apps), provide 'files' and 'entry' with content_type 'project'. The artifact opens in a dedicated canvas tab.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Human-readable title for the artifact"
                        },
                        "content_type": {
                            "type": "string",
                            "enum": ["text/html", "text/markdown", "text/plain", "application/json", "text/css", "text/javascript", "project"],
                            "description": "MIME type of the content. Use 'project' for multi-file React/Vue/Svelte apps."
                        },
                        "description": {
                            "type": "string",
                            "description": "Brief description of what this artifact contains (1-2 sentences)"
                        },
                        "content": {
                            "type": "string",
                            "description": "The full artifact content (for single-file artifacts: HTML, Markdown, code). Optional when using 'files'."
                        },
                        "files": {
                            "type": "string",
                            "description": "Multi-file project: a JSON-encoded map of file paths to file contents. Example: \"{\\\"src/App.jsx\\\": \\\"export default function App() {...}\\\", \\\"src/index.jsx\\\": \\\"import App from './App'; ...\\\"}\". Use with content_type 'project'."
                        },
                        "entry": {
                            "type": "string",
                            "description": "Entry point file path for multi-file projects (e.g. 'src/index.jsx'). Required when 'files' is provided."
                        }
                    },
                    "required": ["title"]
                }),
            },
            ToolDefinition {
                name: "switch_model".into(),
                description: "Switch the active LLM provider and model for subsequent operations. Use this when executing plan steps that specify a different model.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "provider": {
                            "type": "string",
                            "description": "Provider name (e.g., 'anthropic', 'openai', 'deepseek')"
                        },
                        "model": {
                            "type": "string",
                            "description": "Model name (e.g., 'claude-sonnet-4-20250514', 'gpt-4o')"
                        }
                    },
                    "required": ["provider", "model"]
                }),
            },
            ToolDefinition {
                name: "web_search".into(),
                description: "Search the web for information. Use for time-sensitive or unknown topics.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query"
                        }
                    },
                    "required": ["query"]
                }),
            },
            ToolDefinition {
                name: "web_fetch".into(),
                description: "Fetch full content of a specific URL and analyze it with a prompt.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to fetch"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "What to extract from the page"
                        }
                    },
                    "required": ["url", "prompt"]
                }),
            },
            ToolDefinition {
                name: "ask_user".into(),
                description: "Ask the user a question".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask"
                        },
                        "options": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional choices for the user"
                        }
                    },
                    "required": ["question"]
                }),
            },
            ToolDefinition {
                name: "memory_search".into(),
                description: "Search your memory for relevant information".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "What to search for"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results to return"
                        }
                    },
                    "required": ["query"]
                }),
            },
            ToolDefinition {
                name: "memory_create".into(),
                description: "Save information to your long-term memory. Use this to remember important facts, user preferences, patterns, or anything you want to recall in future conversations.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The information to remember"
                        },
                        "metadata": {
                            "type": "object",
                            "description": "Optional metadata (e.g., {\"category\": \"preference\", \"domain\": \"example.com\"})"
                        }
                    },
                    "required": ["content"]
                }),
            },
            ToolDefinition {
                name: "memory_update".into(),
                description: "Update an existing memory chunk with new content. Use the id from memory_search results.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "The memory chunk ID to update"
                        },
                        "content": {
                            "type": "string",
                            "description": "The new content to replace the existing content"
                        }
                    },
                    "required": ["id", "content"]
                }),
            },
            ToolDefinition {
                name: "memory_delete".into(),
                description: "Delete a memory chunk by ID. Use the id from memory_search results.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "The memory chunk ID to delete"
                        }
                    },
                    "required": ["id"]
                }),
            },
            ToolDefinition {
                name: "knowledge_teach".into(),
                description: "Store knowledge explicitly taught by the user. Use when the user asks you to remember, learn, or always do something a certain way.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "enum": ["user_preference", "site_interaction", "tool_optimization"],
                            "description": "Knowledge category"
                        },
                        "summary": {
                            "type": "string",
                            "description": "One-line summary of the knowledge"
                        },
                        "details": {
                            "type": "string",
                            "description": "Full details of the knowledge"
                        },
                        "domain": {
                            "type": "string",
                            "description": "Site domain if applicable (e.g., github.com)"
                        }
                    },
                    "required": ["category", "summary", "details"]
                }),
            },
            ToolDefinition {
                name: "skill_load".into(),
                description: "Load a skill's full content".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The skill name"
                        }
                    },
                    "required": ["name"]
                }),
            },
            // Browser content tools (also available in chat mode for tab context)
            ToolDefinition {
                name: "browser_get_content".into(),
                description: "Get full raw HTML of the page. High token cost. Use ONLY when user needs actual HTML/CSS code structure (e.g., 'build a page like this'). For content reading, use browser_get_markdown instead.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tab_id": {
                            "type": "integer",
                            "description": "Optional tab ID (uses active tab if not specified)"
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_get_markdown".into(),
                description: "Get page content as clean markdown. Low token cost. DEFAULT tool for reading pages. Use for: summarize, translate, analyze, extract info, Q&A. Prefer over browser_get_content unless user needs actual HTML/CSS structure.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tab_id": {
                            "type": "integer",
                            "description": "Optional tab ID (uses active tab if not specified)"
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_screenshot".into(),
                description: "Capture the current tab's visible viewport as an image. Use for: visual layout questions, design analysis, canvas/image content, or when get_markdown returns empty. Do NOT use for reading text content.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "full_page": {
                            "type": "boolean",
                            "description": "Whether to capture the full page (default: false)",
                            "default": false
                        },
                        "tab_id": {
                            "type": "integer",
                            "description": "Optional tab ID (uses active tab if not specified)"
                        }
                    }
                }),
            },
            // Artifact editing tools (available in all modes)
            ToolDefinition {
                name: "browser_read_artifact".into(),
                description: "Read the source code of a canvas artifact. Returns full content by default (with line numbers). Use offset/limit for large artifacts, or grep to search for specific code sections. Only use when [Active Canvas] hint is present.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Artifact ID (from the [Active Canvas] context hint)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Start from line N (1-based)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Read N lines"
                        },
                        "grep": {
                            "type": "string",
                            "description": "Search keyword to find specific code sections"
                        },
                        "context": {
                            "type": "integer",
                            "description": "Lines of context around grep matches (default 5)"
                        }
                    },
                    "required": ["id"]
                }),
            },
            ToolDefinition {
                name: "browser_edit_artifact".into(),
                description: "Edit a canvas artifact using search-and-replace. The old_str must match exactly one location in the artifact. Include surrounding lines for uniqueness. The canvas updates in real-time after each edit. Only use when [Active Canvas] hint is present.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Artifact ID (from the [Active Canvas] context hint)"
                        },
                        "old_str": {
                            "type": "string",
                            "description": "Exact string to find in the artifact code"
                        },
                        "new_str": {
                            "type": "string",
                            "description": "Replacement string"
                        }
                    },
                    "required": ["id", "old_str", "new_str"]
                }),
            },
        ]
    }

    /// Get available tools for browser mode.
    fn get_browser_tools(&self) -> Vec<ToolDefinition> {
        let mut tools = self.get_chat_tools();

        // Browser navigation
        tools.push(ToolDefinition {
            name: "browser_navigate".into(),
            description: "Navigate to a specific URL. For returning to previous page, prefer browser_go_back. NEVER use navigate to 'go back'.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to navigate to"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID (uses active tab if not specified)"
                    }
                },
                "required": ["url"]
            }),
        });

        // History navigation
        tools.push(ToolDefinition {
            name: "browser_go_back".into(),
            description: "Go back in browser history. Preserves page state and form data. Preferred over navigate for returning to previous page.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID (uses active tab if not specified)"
                    }
                }
            }),
        });

        tools.push(ToolDefinition {
            name: "browser_go_forward".into(),
            description: "Go forward in browser history. Returns updated page state.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID (uses active tab if not specified)"
                    }
                }
            }),
        });

        // Click by selector
        tools.push(ToolDefinition {
            name: "browser_click".into(),
            description: "Click on an element by CSS selector".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for the element to click"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["selector"]
            }),
        });

        // Click by ID
        tools.push(ToolDefinition {
            name: "browser_click_by_id".into(),
            description: "Click an interactive element by its [eN] ID from the page state snapshot. Only use IDs from the MOST RECENT snapshot — they change after every action.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "element_id": {
                        "type": "string",
                        "description": "The ID attribute of the element to click"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["element_id"]
            }),
        });

        // Type by selector (keystrokes)
        tools.push(ToolDefinition {
            name: "browser_type".into(),
            description: "Type text into an element by CSS selector (simulates keystrokes)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for the input element"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["selector", "text"]
            }),
        });

        // Type by ID
        tools.push(ToolDefinition {
            name: "browser_type_by_id".into(),
            description: "Type text character by character into an element. Use ONLY for autocomplete, search boxes, or real-time validation. For normal form fields, prefer browser_fill_by_id.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "element_id": {
                        "type": "string",
                        "description": "The ID attribute of the input element"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["element_id", "text"]
            }),
        });

        // Fill by selector (set value)
        tools.push(ToolDefinition {
            name: "browser_fill".into(),
            description: "Fill an input element with a value by CSS selector (sets value directly)"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for the input element"
                    },
                    "value": {
                        "type": "string",
                        "description": "Value to fill"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["selector", "value"]
            }),
        });

        // Fill by ID
        tools.push(ToolDefinition {
            name: "browser_fill_by_id".into(),
            description: "Set a form field's value by element ID. DEFAULT for form filling. Faster than type_by_id. If fill doesn't trigger expected behavior, fall back to type_by_id.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "element_id": {
                        "type": "string",
                        "description": "The ID attribute of the input element"
                    },
                    "value": {
                        "type": "string",
                        "description": "Value to fill"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["element_id", "value"]
            }),
        });

        // NOTE: browser_get_content, browser_get_markdown, and browser_screenshot
        // are already included via get_chat_tools() above.

        // Eval JS
        tools.push(ToolDefinition {
            name: "browser_eval_js".into(),
            description: "Execute JavaScript code in the page context".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "JavaScript code to execute"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["script"]
            }),
        });

        // Scroll
        tools.push(ToolDefinition {
            name: "browser_scroll".into(),
            description: "Scroll the page to reveal more content. Use to find elements not in current snapshot. Returns updated page state.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"],
                        "description": "Direction to scroll"
                    },
                    "amount": {
                        "type": "string",
                        "description": "Scroll amount: 'page' (default), 'half', or pixel count as string (e.g., '300')"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["direction"]
            }),
        });

        // Wait for element
        tools.push(ToolDefinition {
            name: "browser_wait_for".into(),
            description: "Wait for an element to appear on the page".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for the element to wait for"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Maximum time to wait in milliseconds (default: 10000)",
                        "default": 10000
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                },
                "required": ["selector"]
            }),
        });

        // Get elements (usually not needed — page state is auto-injected)
        tools.push(ToolDefinition {
            name: "browser_get_elements".into(),
            description: "Get full accessibility tree (usually not needed — page state is auto-injected). Use browser_find_elements to search and browser_element_info(id) for full details.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID"
                    }
                }
            }),
        });

        // Find elements in cache (after browser_get_elements)
        tools.push(ToolDefinition {
            name: "browser_find_elements".into(),
            description: "Search cached elements from the last browser_get_elements call. Filters are AND-combined.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "Filter by ARIA role (exact match, case-insensitive)"
                    },
                    "name": {
                        "type": "string",
                        "description": "Substring match on element name/text (case-insensitive)"
                    },
                    "selector": {
                        "type": "string",
                        "description": "Substring match on CSS selector"
                    },
                    "near_x": {
                        "type": "integer",
                        "description": "Find elements near this X coordinate (requires near_y)"
                    },
                    "near_y": {
                        "type": "integer",
                        "description": "Find elements near this Y coordinate (requires near_x)"
                    },
                    "radius": {
                        "type": "integer",
                        "description": "Search radius in pixels (default: 50, requires near_x + near_y)",
                        "default": 50
                    },
                    "unnamed_only": {
                        "type": "boolean",
                        "description": "Only return elements with empty name (default: false)",
                        "default": false
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results (default: 20)",
                        "default": 20
                    }
                }
            }),
        });

        // Get full details for one element from cache
        tools.push(ToolDefinition {
            name: "browser_element_info".into(),
            description: "Get full details for a single element by ID from the cache. Call browser_get_elements first.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Element ID (e.g., \"e42\")"
                    }
                },
                "required": ["id"]
            }),
        });

        tools
    }

    /// Get available tools for agent mode.
    fn get_agent_tools(&self) -> Vec<ToolDefinition> {
        let mut tools = self.get_browser_tools();

        // Add file tools
        tools.push(ToolDefinition {
            name: "read".into(),
            description: "Read file contents. Returns partial content with metadata (total_lines). Default: first 200 lines. Use offset/limit for pagination. PREFER over bash cat.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line offset to start reading from"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum lines to read"
                    }
                },
                "required": ["file_path"]
            }),
        });

        tools.push(ToolDefinition {
            name: "write".into(),
            description: "Create a new file or overwrite an existing file entirely. WARNING: Overwrites the full file content. Use edit() for partial changes to existing files.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write"
                    }
                },
                "required": ["file_path", "content"]
            }),
        });

        tools.push(ToolDefinition {
            name: "edit".into(),
            description: "Find and replace exact text in a file. PREFER over write() for modifying existing files. Always read() the file first to see current content. Include surrounding context in old_string to ensure a unique match.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The text to find"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The replacement text"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace all occurrences"
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        });

        tools.push(ToolDefinition {
            name: "bash".into(),
            description: "Execute a shell command. Default timeout: 30s. Output capped at 200 lines. Use ONLY when specialized tools (read, grep, glob, edit) cannot accomplish the task.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in milliseconds"
                    }
                },
                "required": ["command"]
            }),
        });

        tools.push(ToolDefinition {
            name: "glob".into(),
            description: "Find files by glob pattern (e.g. 'src/**/*.rs'). PREFER over bash find."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern like '**/*.rs'"
                    },
                    "path": {
                        "type": "string",
                        "description": "Base directory"
                    }
                },
                "required": ["pattern"]
            }),
        });

        tools.push(ToolDefinition {
            name: "grep".into(),
            description: "Search file contents using regex. Returns structured matches with counts. PREFER over bash grep. Supports file type filter. Respects .gitignore.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Search pattern (regex)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search"
                    },
                    "type": {
                        "type": "string",
                        "description": "File type filter (e.g., 'rs', 'py', 'js') \u{2014} same types as ripgrep"
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Case insensitive search (default: false)"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 50)"
                    }
                },
                "required": ["pattern"]
            }),
        });

        // Dynamic tool discovery
        tools.push(ToolDefinition {
            name: "tool_search".into(),
            description: "Search for available external tools (MCP) by keyword. Always search first — never guess tool names or schemas.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords to search for (e.g., 'git', 'database', 'image')"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5)",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        });

        tools.push(ToolDefinition {
            name: "tool_call_dynamic".into(),
            description: "Call a tool discovered via tool_search. Read the returned schema carefully before calling.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "The exact name of the tool to call"
                    },
                    "arguments": {
                        "type": "string",
                        "description": "JSON string of arguments to pass to the tool (e.g., '{\"key\": \"value\"}')"
                    }
                },
                "required": ["tool_name", "arguments"],
                "additionalProperties": false
            }),
        });

        // Computer control tools
        tools.push(ToolDefinition {
            name: "computer_screenshot".into(),
            description: "Take a screenshot of the entire screen or a specific monitor".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "monitor": {
                        "type": "integer",
                        "description": "Monitor index (0-based). If not specified, captures primary monitor.",
                        "minimum": 0
                    }
                }
            }),
        });

        tools.push(ToolDefinition {
            name: "computer_mouse_move".into(),
            description: "Move the mouse cursor to a specified position on screen".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "integer",
                        "description": "X coordinate in pixels"
                    },
                    "y": {
                        "type": "integer",
                        "description": "Y coordinate in pixels"
                    },
                    "click": {
                        "type": "string",
                        "description": "Optional click action after moving",
                        "enum": ["left", "right", "middle", "double"]
                    }
                },
                "required": ["x", "y"]
            }),
        });

        tools.push(ToolDefinition {
            name: "computer_click".into(),
            description: "Click at a specific screen position".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "integer",
                        "description": "X coordinate in pixels"
                    },
                    "y": {
                        "type": "integer",
                        "description": "Y coordinate in pixels"
                    },
                    "button": {
                        "type": "string",
                        "description": "Mouse button to click",
                        "enum": ["left", "right", "middle"],
                        "default": "left"
                    },
                    "click_type": {
                        "type": "string",
                        "description": "Type of click to perform",
                        "enum": ["single", "double", "triple"],
                        "default": "single"
                    }
                },
                "required": ["x", "y"]
            }),
        });

        tools.push(ToolDefinition {
            name: "computer_type_text".into(),
            description: "Type text using the keyboard at the current cursor position".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The text to type"
                    },
                    "delay_ms": {
                        "type": "integer",
                        "description": "Delay between keystrokes in milliseconds",
                        "default": 0,
                        "minimum": 0
                    }
                },
                "required": ["text"]
            }),
        });

        tools.push(ToolDefinition {
            name: "computer_key".into(),
            description: "Press keyboard keys or key combinations".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Key to press (e.g., 'Enter', 'Tab', 'Escape', 'a', 'F1')"
                    },
                    "modifiers": {
                        "type": "array",
                        "description": "Modifier keys to hold while pressing the key",
                        "items": {
                            "type": "string",
                            "enum": ["ctrl", "alt", "shift", "meta", "super"]
                        },
                        "default": []
                    },
                    "repeat": {
                        "type": "integer",
                        "description": "Number of times to repeat the key press",
                        "default": 1,
                        "minimum": 1,
                        "maximum": 100
                    }
                },
                "required": ["key"]
            }),
        });

        tools.push(ToolDefinition {
            name: "computer_scroll".into(),
            description: "Scroll at a specific screen position".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "integer",
                        "description": "X coordinate in pixels for scroll position"
                    },
                    "y": {
                        "type": "integer",
                        "description": "Y coordinate in pixels for scroll position"
                    },
                    "direction": {
                        "type": "string",
                        "description": "Direction to scroll",
                        "enum": ["up", "down", "left", "right"]
                    },
                    "amount": {
                        "type": "integer",
                        "description": "Number of scroll units",
                        "default": 3,
                        "minimum": 1,
                        "maximum": 100
                    }
                },
                "required": ["x", "y", "direction"]
            }),
        });

        // Subagent tools for parallel work
        tools.push(ToolDefinition {
            name: "subagent_spawn".into(),
            description: "Spawn a lightweight subagent for a focused parallel task. Returns an ID. Subagents have NO page interaction — read-only browser access (with tab_id) and web search. Use for: parallel research, parallel summarization, independent subtasks.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The task description for the sub-agent to execute"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["chat", "browser", "agent"],
                        "description": "Execution mode for the sub-agent (default: agent)",
                        "default": "agent"
                    },
                    "tab_id": {
                        "type": "integer",
                        "description": "Optional tab ID for the sub-agent to read page content from (read-only access)"
                    }
                },
                "required": ["task"]
            }),
        });

        tools.push(ToolDefinition {
            name: "subagent_status".into(),
            description: "Check the current status of a sub-agent.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The sub-agent ID returned by subagent_spawn"
                    }
                },
                "required": ["id"]
            }),
        });

        tools.push(ToolDefinition {
            name: "subagent_wait".into(),
            description: "Wait for a sub-agent to complete and get its result. Blocks until \
                          the sub-agent finishes execution."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The sub-agent ID returned by subagent_spawn"
                    }
                },
                "required": ["id"]
            }),
        });

        tools.push(ToolDefinition {
            name: "subagent_wait_all".into(),
            description: "Wait for multiple subagents to complete. Returns all results at once. PREFERRED over calling subagent_wait multiple times.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "Sub-agent IDs to wait for"
                    }
                },
                "required": ["ids"]
            }),
        });

        tools.push(ToolDefinition {
            name: "subagent_kill".into(),
            description: "Terminate a running sub-agent. Returns true if the sub-agent was \
                          killed, false if it had already completed."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The sub-agent ID to terminate"
                    }
                },
                "required": ["id"]
            }),
        });

        tools.push(ToolDefinition {
            name: "subagent_list".into(),
            description: "List all sub-agents with their IDs, tasks, modes, and statuses.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        });

        tools
    }
}

/// Check if `text` ends with a non-empty prefix of `<tool_call>`.
///
/// Returns `Some(index)` where the partial prefix starts, or `None`.
/// This handles the case where the `<tool_call>` opening tag is split across
/// streaming chunks (e.g. `<tool_call` in one chunk and `>` in the next).
fn find_tool_call_tag_prefix_at_end(text: &str) -> Option<usize> {
    const TAG: &str = "<tool_call>";
    for prefix_len in (1..TAG.len()).rev() {
        if text.ends_with(&TAG[..prefix_len]) {
            return Some(text.len() - prefix_len);
        }
    }
    None
}

/// Parse `<tool_call>...</tool_call>` XML from text into `ToolCall` structs.
///
/// Returns `(cleaned_text_without_markers, extracted_tool_calls)`.
/// Text-based tool calls are emitted by some LLM providers as plain text instead
/// of structured `tool_use` blocks.
fn parse_tool_calls_from_text(text: &str) -> (String, Vec<ToolCall>) {
    let mut tool_calls = Vec::new();
    let mut cleaned = String::new();
    let mut remaining = text;

    loop {
        let Some(start_idx) = remaining.find("<tool_call>") else {
            cleaned.push_str(remaining);
            break;
        };

        // Add text before the marker
        cleaned.push_str(&remaining[..start_idx]);

        let after_start = &remaining[start_idx + "<tool_call>".len()..];

        let Some(end_idx) = after_start.find("</tool_call>") else {
            // No closing tag — keep the raw text as-is
            cleaned.push_str(&remaining[start_idx..]);
            break;
        };

        let json_str = after_start[..end_idx].trim();

        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
            let id = parsed
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = parsed
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = parsed
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
                call_id: None,
                signature: None,
            });
        } else {
            // Malformed JSON — keep the raw text
            cleaned.push_str(
                &remaining
                    [start_idx..start_idx + "<tool_call>".len() + end_idx + "</tool_call>".len()],
            );
        }

        remaining = &after_start[end_idx + "</tool_call>".len()..];
    }

    let cleaned = cleaned.trim().to_string();
    (cleaned, tool_calls)
}

/// Format skill summaries for system prompt injection.
fn format_skill_summaries(skills: &[SkillSummary]) -> String {
    skills
        .iter()
        .map(|s| format!("- **{}**: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Safely truncate a string to at most `max_bytes` bytes at a valid UTF-8 char boundary.
fn truncate_string_safe(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Find the last char boundary at or before max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Calculate the total size of all messages in bytes.
fn calculate_messages_size(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            let mut size = m.content.len();
            // Add tool calls size for assistant messages
            for tc in &m.tool_calls {
                size += tc.id.len() + tc.name.len();
                // arguments is a serde_json::Value, estimate its string size
                size += tc.arguments.to_string().len();
                if let Some(ref call_id) = tc.call_id {
                    size += call_id.len();
                }
            }
            size
        })
        .sum()
}

/// Truncate tool result content if it would exceed the total message size limit.
///
/// This function dynamically calculates how much space is available based on:
/// - Current total message size
/// - Maximum allowed total size (300KB, ~75K tokens)
/// - Reserved space for LLM output (50KB)
/// - Minimum tool result size (10KB)
fn truncate_tool_result_if_needed(messages: &[Message], content: &str) -> String {
    // Total message size limit (~75K tokens for most models)
    const MAX_TOTAL_MESSAGE_SIZE: usize = 300 * 1024; // 300KB
                                                      // Reserved space for LLM output
    const RESERVED_OUTPUT_SIZE: usize = 50 * 1024; // 50KB
                                                   // Minimum tool result size (don't truncate below this)
    const MIN_TOOL_RESULT_SIZE: usize = 10 * 1024; // 10KB

    let current_size = calculate_messages_size(messages);
    let available_space = MAX_TOTAL_MESSAGE_SIZE
        .saturating_sub(current_size)
        .saturating_sub(RESERVED_OUTPUT_SIZE);

    // Calculate max size for this tool result
    let max_result_size = available_space.max(MIN_TOOL_RESULT_SIZE);

    if content.len() <= max_result_size {
        return content.to_string();
    }

    // Need to truncate
    eprintln!(
        "[AGENT] Tool result truncated: {} -> {} bytes (current_msgs={}, available={})",
        content.len(),
        max_result_size,
        current_size,
        available_space
    );

    // Use safe UTF-8 truncation
    let truncated = truncate_string_safe(content, max_result_size);

    format!(
        "{}...\n\n[Content truncated: {} bytes total, showing first {} bytes]",
        truncated,
        content.len(),
        truncated.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MockHostFunctions;

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.max_iterations, 100);
        assert!(config.use_streaming);
        assert!(!config.suppress_streaming);
    }

    #[test]
    fn test_agent_config_for_subagent() {
        let config = AgentConfig::for_subagent();
        assert_eq!(config.max_iterations, 100);
        assert!(!config.use_streaming);
        assert!(config.suppress_streaming);
    }

    #[test]
    fn test_agent_config_with_suppress_streaming() {
        let config = AgentConfig::default().with_suppress_streaming(true);
        assert!(config.suppress_streaming);
    }

    #[test]
    fn test_agent_new() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);
        assert_eq!(agent.config.max_iterations, 100);
    }

    #[test]
    fn test_agent_with_config() {
        let mock = MockHostFunctions::new();
        let config = AgentConfig {
            max_iterations: 50,
            use_streaming: false,
            suppress_streaming: false,
            is_subagent: false,
        };
        let agent = Agent::with_config(mock, config);
        assert_eq!(agent.config.max_iterations, 50);
    }

    #[test]
    fn test_agent_run_with_custom_system_prompt() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Agent,
            user_message: "Search for files".into(),
            history: vec![],
            attachments: vec![],
            local_files: vec![],
            custom_system_prompt: Some(
                "You are a specialized file search sub-agent. Focus only on finding files.".into(),
            ),
            tab_id: None,
            tab_ids: vec![],
            skill_context: None,
            available_models: vec![],
            mcp_servers: vec![],
            soul_context: None,
        };

        // Should run successfully with custom prompt
        let output = agent.run(&input).unwrap();
        assert!(!output.continue_loop);
    }

    #[test]
    fn test_build_system_prompt() {
        let prompt = Agent::<MockHostFunctions>::build_system_prompt(AgentMode::Chat, &[], &[]);
        assert!(!prompt.is_empty());
        assert_eq!(prompt, CHAT_PROMPT);

        let prompt = Agent::<MockHostFunctions>::build_system_prompt(AgentMode::Browser, &[], &[]);
        assert_eq!(prompt, BROWSER_PROMPT);

        let prompt = Agent::<MockHostFunctions>::build_system_prompt(AgentMode::Agent, &[], &[]);
        assert_eq!(prompt, AGENT_PROMPT);
    }

    #[test]
    fn test_format_tab_context_single_tab() {
        let tab = TabInfo {
            tab_id: 42,
            tab_title: "Test Page".into(),
            url: "https://example.com".into(),
            space: String::new(),
        };
        let ctx = Agent::<MockHostFunctions>::format_tab_context(Some(&tab), &[]);
        assert!(ctx.contains("current_tab: 42"));
        assert!(ctx.contains("\"Test Page\""));
        assert!(ctx.contains("https://example.com"));
        // Must NOT contain behavioral instructions
        assert!(!ctx.contains("IMPORTANT"));
        assert!(!ctx.contains("browser_get_markdown"));
    }

    #[test]
    fn test_format_tab_context_multi_tab() {
        let tabs = vec![
            TabInfo {
                tab_id: 1,
                tab_title: "Tab A".into(),
                url: "https://a.com".into(),
                space: "Work".into(),
            },
            TabInfo {
                tab_id: 2,
                tab_title: "Tab B".into(),
                url: "https://b.com".into(),
                space: "Work".into(),
            },
            TabInfo {
                tab_id: 3,
                tab_title: "Tab C".into(),
                url: "https://c.com".into(),
                space: String::new(),
            },
        ];
        let active = TabInfo {
            tab_id: 1,
            tab_title: "Tab A".into(),
            url: "https://a.com".into(),
            space: "Work".into(),
        };
        let ctx = Agent::<MockHostFunctions>::format_tab_context(Some(&active), &tabs);
        assert!(ctx.contains("[Work]"));
        assert!(ctx.contains("[Default]"));
        assert!(ctx.contains("current_tab: 1"));
        assert!(ctx.contains("\"Tab A\""));
        assert!(ctx.contains("https://a.com"));
    }

    #[test]
    fn test_format_tab_context_empty() {
        let ctx = Agent::<MockHostFunctions>::format_tab_context(None, &[]);
        assert!(ctx.is_empty());
    }

    #[test]
    fn test_get_tools_for_mode() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let chat_tools = agent.get_tools_for_mode(AgentMode::Chat);
        assert!(chat_tools.iter().any(|t| t.name == "web_search"));
        assert!(!chat_tools.iter().any(|t| t.name == "bash"));

        let browser_tools = agent.get_tools_for_mode(AgentMode::Browser);
        assert!(browser_tools.iter().any(|t| t.name == "browser_navigate"));

        let agent_tools = agent.get_tools_for_mode(AgentMode::Agent);
        assert!(agent_tools.iter().any(|t| t.name == "bash"));
        assert!(agent_tools.iter().any(|t| t.name == "subagent_spawn"));
    }

    #[test]
    fn test_agent_run_chat() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Chat,
            user_message: "Hello".into(),
            history: vec![],
            attachments: vec![],
            local_files: vec![],
            custom_system_prompt: None,
            tab_id: None,
            tab_ids: vec![],
            skill_context: None,
            available_models: vec![],
            mcp_servers: vec![],
            soul_context: None,
        };

        let output = agent.run(&input).unwrap();
        assert!(!output.continue_loop);
    }

    #[test]
    fn test_agent_run_browser() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Browser,
            user_message: "Click the button".into(),
            history: vec![],
            attachments: vec![],
            local_files: vec![],
            custom_system_prompt: None,
            tab_id: None,
            tab_ids: vec![],
            skill_context: None,
            available_models: vec![],
            mcp_servers: vec![],
            soul_context: None,
        };

        let output = agent.run(&input).unwrap();
        assert!(!output.continue_loop);
    }

    #[test]
    fn test_agent_run_agent_mode() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Agent,
            user_message: "List files".into(),
            history: vec![],
            attachments: vec![],
            local_files: vec![],
            custom_system_prompt: None,
            tab_id: None,
            tab_ids: vec![],
            skill_context: None,
            available_models: vec![],
            mcp_servers: vec![],
            soul_context: None,
        };

        let output = agent.run(&input).unwrap();
        assert!(!output.continue_loop);
    }

    #[test]
    fn test_agent_with_tool_calls() {
        let mock = MockHostFunctions::new();
        mock.add_llm_response(LlmResponse {
            text: "Let me search for that.".into(),
            tool_calls: vec![ToolCall {
                id: "call-001".into(),
                call_id: None,
                name: "web_search".into(),
                arguments: serde_json::json!({"query": "rust programming"}),
                signature: None,
            }],
        });
        mock.add_llm_response(LlmResponse {
            text: "Here's what I found about Rust.".into(),
            tool_calls: vec![],
        });

        // Use non-streaming config since mock doesn't support streaming responses
        let config = AgentConfig {
            max_iterations: 100,
            use_streaming: false,
            suppress_streaming: false,
            is_subagent: false,
        };
        let agent = Agent::with_config(mock, config);
        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Chat,
            user_message: "Tell me about Rust".into(),
            history: vec![],
            attachments: vec![],
            local_files: vec![],
            custom_system_prompt: None,
            tab_id: None,
            tab_ids: vec![],
            skill_context: None,
            available_models: vec![],
            mcp_servers: vec![],
            soul_context: None,
        };

        let output = agent.run(&input).unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert!(output.text.contains("Rust"));
    }

    #[test]
    fn test_static_prompts_content() {
        // Chat prompt should NOT mention interaction
        assert!(!CHAT_PROMPT.contains("browser_click"));

        // Browser prompt should contain interaction rules
        assert!(BROWSER_PROMPT.contains("element ID") || BROWSER_PROMPT.contains("[eN]"));

        // Agent prompt should contain tool strategy
        assert!(
            AGENT_PROMPT.contains("browser_get_markdown")
                || AGENT_PROMPT.contains("Tool selection")
        );

        // Subagent prompts
        assert!(SUBAGENT_BROWSER_PROMPT.contains("CANNOT interact"));
        assert!(SUBAGENT_AGENT_PROMPT.contains("sandbox"));
    }

    #[test]
    fn test_format_skill_summaries() {
        let skills = vec![
            SkillSummary {
                name: "code-review".into(),
                description: "Review code for issues".into(),
                tags: vec!["code".into()],
            },
            SkillSummary {
                name: "tdd".into(),
                description: "Test-driven development workflow".into(),
                tags: vec![],
            },
        ];

        let formatted = format_skill_summaries(&skills);
        assert!(formatted.contains("- **code-review**: Review code for issues"));
        assert!(formatted.contains("- **tdd**: Test-driven development workflow"));
        assert!(formatted.contains("\n")); // Multiple lines
    }

    #[test]
    fn test_format_skill_summaries_empty() {
        let skills: Vec<SkillSummary> = vec![];
        let formatted = format_skill_summaries(&skills);
        assert!(formatted.is_empty());
    }

    #[test]
    fn test_build_system_prompt_with_skills() {
        let skills = vec![SkillSummary {
            name: "web-tools".into(),
            description: "Web automation tools".into(),
            tags: vec![],
        }];
        let prompt = Agent::<MockHostFunctions>::build_system_prompt(AgentMode::Chat, &skills, &[]);
        assert!(prompt.contains("web-tools"));
        assert!(prompt.contains("# Skills"));
    }

    #[test]
    fn test_build_system_prompt_with_models() {
        let models = vec![("anthropic".into(), "claude-sonnet".into())];
        let prompt = Agent::<MockHostFunctions>::build_system_prompt(AgentMode::Chat, &[], &models);
        assert!(prompt.contains("claude-sonnet"));
        assert!(prompt.contains("# Available models"));
    }

    #[test]
    fn test_build_system_prompt_no_extras() {
        let prompt = Agent::<MockHostFunctions>::build_system_prompt(AgentMode::Chat, &[], &[]);
        // Should be exactly the static prompt, no extras
        assert_eq!(prompt, CHAT_PROMPT);
        assert!(!prompt.contains("# Skills"));
        assert!(!prompt.contains("# Available models"));
    }

    #[test]
    fn test_agent_get_tools() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let chat_tools = agent.get_chat_tools();
        assert!(chat_tools.iter().any(|t| t.name == "web_search"));
        assert!(chat_tools.iter().any(|t| t.name == "ask_user"));

        let browser_tools = agent.get_browser_tools();
        assert!(browser_tools.len() > chat_tools.len());
        // Browser tools should include all 13 browser-specific tools
        assert!(browser_tools.iter().any(|t| t.name == "browser_navigate"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_click"));
        assert!(browser_tools
            .iter()
            .any(|t| t.name == "browser_click_by_id"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_type"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_type_by_id"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_fill"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_fill_by_id"));
        assert!(browser_tools
            .iter()
            .any(|t| t.name == "browser_get_content"));
        assert!(browser_tools
            .iter()
            .any(|t| t.name == "browser_get_markdown"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_screenshot"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_eval_js"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_scroll"));
        assert!(browser_tools.iter().any(|t| t.name == "browser_wait_for"));

        let agent_tools = agent.get_agent_tools();
        assert!(agent_tools.iter().any(|t| t.name == "bash"));
        assert!(agent_tools.iter().any(|t| t.name == "read"));
        assert!(agent_tools.iter().any(|t| t.name == "write"));
    }

    #[test]
    fn test_execute_tool_web_search() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "web_search".into(),
            arguments: serde_json::json!({"query": "test"}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
        assert_eq!(result.tool_call_id, "call-001");
    }

    #[test]
    fn test_execute_tool_unknown() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "unknown_tool".into(),
            arguments: serde_json::json!({}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.content.contains("Unknown tool"));
    }

    #[test]
    fn test_execute_tool_search() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "tool_search".into(),
            arguments: serde_json::json!({"query": "file", "max_results": 5}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
        // Mock returns empty array
        assert!(result.content.contains("[]"));
    }

    #[test]
    fn test_execute_tool_call_dynamic() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "tool_call_dynamic".into(),
            arguments: serde_json::json!({
                "tool_name": "read_file",
                "arguments": r#"{"path": "/test.txt"}"#
            }),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("read_file"));
    }

    #[test]
    fn test_agent_tools_include_dynamic_tools() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let agent_tools = agent.get_agent_tools();
        assert!(agent_tools.iter().any(|t| t.name == "tool_search"));
        assert!(agent_tools.iter().any(|t| t.name == "tool_call_dynamic"));
    }

    #[test]
    fn test_execute_tool_browser_navigate() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_navigate".into(),
            arguments: serde_json::json!({"url": "https://example.com"}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("success"));
    }

    #[test]
    fn test_execute_tool_browser_click() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_click".into(),
            arguments: serde_json::json!({"selector": "#submit-btn"}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
    }

    #[test]
    fn test_execute_tool_browser_type() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_type".into(),
            arguments: serde_json::json!({"selector": "#input", "text": "Hello World"}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
    }

    #[test]
    fn test_execute_tool_browser_screenshot() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_screenshot".into(),
            arguments: serde_json::json!({"full_page": true}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("screenshot"));
    }

    #[test]
    fn test_execute_tool_browser_scroll() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_scroll".into(),
            arguments: serde_json::json!({"direction": "down", "amount": "page"}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
    }

    #[test]
    fn test_execute_tool_browser_wait_for() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_wait_for".into(),
            arguments: serde_json::json!({"selector": "#loading", "timeout_ms": 5000}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
    }

    #[test]
    fn test_browser_tools_count() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let browser_tools = agent.get_browser_tools();
        let chat_tools = agent.get_chat_tools();
        // Browser tools = chat tools + 15 browser-specific interaction tools
        // (browser_get_content, browser_get_markdown, browser_screenshot are already in chat tools)
        assert_eq!(browser_tools.len(), chat_tools.len() + 15);
    }

    #[test]
    fn test_agent_run_not_interrupted() {
        let mock = MockHostFunctions::new();
        // Default: not interrupted
        let agent = Agent::new(mock);

        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Chat,
            user_message: "Hello".into(),
            history: vec![],
            attachments: vec![],
            local_files: vec![],
            custom_system_prompt: None,
            tab_id: None,
            tab_ids: vec![],
            skill_context: None,
            available_models: vec![],
            mcp_servers: vec![],
            soul_context: None,
        };

        // Should complete normally
        let output = agent.run(&input).unwrap();
        assert!(!output.continue_loop);
    }

    #[test]
    fn test_agent_run_interrupted_before_llm_call() {
        // Create a mock that is interrupted immediately
        let mock = MockHostFunctions::new();
        mock.set_interrupted(true); // Interrupt immediately

        let agent = Agent::new(mock);

        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Chat,
            user_message: "Hello".into(),
            history: vec![],
            attachments: vec![],
            local_files: vec![],
            custom_system_prompt: None,
            tab_id: None,
            tab_ids: vec![],
            skill_context: None,
            available_models: vec![],
            mcp_servers: vec![],
            soul_context: None,
        };

        // Should exit early due to interrupt
        let output = agent.run(&input).unwrap();
        // Output text should be empty because we never called LLM
        assert!(output.text.is_empty());
        assert!(output.tool_calls.is_empty());
    }

    #[test]
    fn test_agent_tools_include_subagent_tools() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let agent_tools = agent.get_agent_tools();
        assert!(agent_tools.iter().any(|t| t.name == "subagent_spawn"));
        assert!(agent_tools.iter().any(|t| t.name == "subagent_status"));
        assert!(agent_tools.iter().any(|t| t.name == "subagent_wait"));
        assert!(agent_tools.iter().any(|t| t.name == "subagent_kill"));
        assert!(agent_tools.iter().any(|t| t.name == "subagent_list"));
    }

    #[test]
    fn test_execute_tool_subagent_spawn() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Search for files", "mode": "agent"}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("Spawned sub-agent with ID:"));
    }

    #[test]
    fn test_execute_tool_subagent_spawn_default_mode() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Do something"}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("Spawned sub-agent"));
    }

    #[test]
    fn test_execute_tool_subagent_status() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        // First spawn a subagent
        let spawn_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Test task"}),
            signature: None,
        };
        agent.execute_tool(&spawn_call).unwrap();

        // Then check its status
        let status_call = ToolCall {
            id: "call-002".into(),
            call_id: None,
            name: "subagent_status".into(),
            arguments: serde_json::json!({"id": 1}),
            signature: None,
        };

        let result = agent.execute_tool(&status_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("status:"));
    }

    #[test]
    fn test_execute_tool_subagent_wait() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        // First spawn a subagent
        let spawn_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Test task"}),
            signature: None,
        };
        agent.execute_tool(&spawn_call).unwrap();

        // Then wait for it
        let wait_call = ToolCall {
            id: "call-002".into(),
            call_id: None,
            name: "subagent_wait".into(),
            arguments: serde_json::json!({"id": 1}),
            signature: None,
        };

        let result = agent.execute_tool(&wait_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("Test task"));
    }

    #[test]
    fn test_execute_tool_subagent_kill() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        // First spawn a subagent
        let spawn_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Long running task"}),
            signature: None,
        };
        agent.execute_tool(&spawn_call).unwrap();

        // Then kill it
        let kill_call = ToolCall {
            id: "call-002".into(),
            call_id: None,
            name: "subagent_kill".into(),
            arguments: serde_json::json!({"id": 1}),
            signature: None,
        };

        let result = agent.execute_tool(&kill_call).unwrap();
        assert!(result.success);
        // Mock immediately completes, so it should say "already completed"
        assert!(result.content.contains("already completed"));
    }

    #[test]
    fn test_execute_tool_subagent_list() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        // First spawn some subagents
        let spawn_call1 = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Task 1", "mode": "agent"}),
            signature: None,
        };
        agent.execute_tool(&spawn_call1).unwrap();

        let spawn_call2 = ToolCall {
            id: "call-002".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Task 2", "mode": "browser"}),
            signature: None,
        };
        agent.execute_tool(&spawn_call2).unwrap();

        // Then list them
        let list_call = ToolCall {
            id: "call-003".into(),
            call_id: None,
            name: "subagent_list".into(),
            arguments: serde_json::json!({}),
            signature: None,
        };

        let result = agent.execute_tool(&list_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("Task 1"));
        assert!(result.content.contains("Task 2"));
    }

    #[test]
    fn test_format_file_size() {
        assert_eq!(format_file_size(500), "500 B");
        assert_eq!(format_file_size(1024), "1.0 KB");
        assert_eq!(format_file_size(1536), "1.5 KB");
        assert_eq!(format_file_size(1048576), "1.0 MB");
        assert_eq!(format_file_size(1073741824), "1.0 GB");
    }

    #[test]
    fn test_format_local_files_empty() {
        let result = format_local_files(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_format_local_files_single_file() {
        use nevoflux_protocol::LocalFileRef;
        let files = vec![LocalFileRef {
            path: "/home/user/test.rs".into(),
            is_directory: false,
            size: Some(2048),
            modified: None,
        }];
        let result = format_local_files(&files);
        assert!(result.contains("/home/user/test.rs"));
        assert!(result.contains("文件"));
        assert!(result.contains("2.0 KB"));
        assert!(result.contains("read_file"));
    }

    #[test]
    fn test_format_local_files_directory() {
        use nevoflux_protocol::LocalFileRef;
        let files = vec![LocalFileRef {
            path: "/home/user/project".into(),
            is_directory: true,
            size: None,
            modified: None,
        }];
        let result = format_local_files(&files);
        assert!(result.contains("/home/user/project"));
        assert!(result.contains("目录"));
        assert!(result.contains("list_directory"));
    }

    #[test]
    fn test_format_local_files_mixed() {
        use nevoflux_protocol::LocalFileRef;
        let files = vec![
            LocalFileRef {
                path: "/home/user/main.rs".into(),
                is_directory: false,
                size: Some(1024),
                modified: None,
            },
            LocalFileRef {
                path: "/home/user/src".into(),
                is_directory: true,
                size: None,
                modified: None,
            },
        ];
        let result = format_local_files(&files);
        assert!(result.contains("main.rs"));
        assert!(result.contains("/home/user/src"));
    }

    // ==================== Truncation Tests ====================

    #[test]
    fn test_calculate_messages_size() {
        let messages = vec![
            Message::system("System prompt"),
            Message::user("User message"),
        ];
        let size = calculate_messages_size(&messages);
        assert_eq!(size, "System prompt".len() + "User message".len());
    }

    #[test]
    fn test_truncate_tool_result_small_content() {
        // Small content should not be truncated
        let messages = vec![Message::system("System prompt")];
        let content = "Small content";
        let result = truncate_tool_result_if_needed(&messages, content);
        assert_eq!(result, content);
    }

    #[test]
    fn test_truncate_tool_result_large_content() {
        // Large content should be truncated
        let messages = vec![Message::system("System prompt")];
        // Create content larger than MAX_TOTAL_MESSAGE_SIZE
        let large_content = "x".repeat(400 * 1024); // 400KB
        let result = truncate_tool_result_if_needed(&messages, &large_content);

        // Should be truncated
        assert!(result.len() < large_content.len());
        assert!(result.contains("[Content truncated:"));
    }

    #[test]
    fn test_truncate_tool_result_respects_existing_messages() {
        // When messages already take up space, less space is available for tool result
        let large_system = "x".repeat(200 * 1024); // 200KB system message
        let messages = vec![Message::system(&large_system)];

        let content = "y".repeat(150 * 1024); // 150KB content
        let result = truncate_tool_result_if_needed(&messages, &content);

        // Should be truncated because total would exceed 300KB limit
        assert!(result.len() < content.len());
        assert!(result.contains("[Content truncated:"));
    }

    #[test]
    fn test_truncate_tool_result_minimum_size() {
        // Even when messages are very large, tool result should have minimum 10KB
        let huge_system = "x".repeat(290 * 1024); // 290KB - almost at limit
        let messages = vec![Message::system(&huge_system)];

        let content = "y".repeat(50 * 1024); // 50KB content
        let result = truncate_tool_result_if_needed(&messages, &content);

        // Should have at least 10KB (MIN_TOOL_RESULT_SIZE)
        assert!(result.len() >= 10 * 1024);
    }

    // =========================================================================
    // Elements caching tests
    // =========================================================================

    #[test]
    fn test_parse_elements_from_data_refs_format() {
        let data = serde_json::json!({
            "refs": {
                "e1": {"name": "Submit", "role": "button", "selectors": [{"type": "css", "strategy": "id", "value": "#submit"}], "rect": {"x": 10.0, "y": 20.0, "width": 100.0, "height": 30.0}},
                "e2": {"name": "Email", "role": "textbox", "selectors": [{"type": "css", "strategy": "css", "value": "input[name=email]"}]}
            }
        });
        let cache = parse_elements_from_data(&data).unwrap();
        assert_eq!(cache.elements.len(), 2);

        let e1 = cache.elements.iter().find(|e| e.id == "e1").unwrap();
        assert_eq!(e1.role, "button");
        assert_eq!(e1.name, "Submit");
        assert_eq!(e1.selector, "#submit");
        assert!(e1.rect.is_some());
        let rect = e1.rect.as_ref().unwrap();
        assert_eq!(rect.x, 10.0);
        assert_eq!(rect.width, 100.0);

        let e2 = cache.elements.iter().find(|e| e.id == "e2").unwrap();
        assert_eq!(e2.role, "textbox");
        assert_eq!(e2.selector, "input[name=email]");
        assert!(e2.rect.is_none());
    }

    #[test]
    fn test_parse_elements_from_data_elements_format() {
        let data = serde_json::json!({
            "elements": [
                {"id": "e1", "name": "Submit", "role": "button"},
                {"id": "e2", "name": "", "role": "generic"}
            ]
        });
        let cache = parse_elements_from_data(&data).unwrap();
        assert_eq!(cache.elements.len(), 2);
        assert_eq!(cache.elements[0].id, "e1");
        assert_eq!(cache.elements[1].role, "generic");
    }

    #[test]
    fn test_parse_elements_missing_refs() {
        let data = serde_json::json!({"other": "data"});
        assert!(parse_elements_from_data(&data).is_none());
    }

    #[test]
    fn test_parse_elements_empty_refs() {
        let data = serde_json::json!({"refs": {}});
        assert!(parse_elements_from_data(&data).is_none());
    }

    #[test]
    fn test_build_summary_interactive_elements() {
        let elements = vec![
            CachedElement {
                id: "e1".into(),
                role: "button".into(),
                name: "Submit".into(),
                selector: "#submit".into(),
                rect: Some(ElementRect {
                    x: 10.0,
                    y: 20.0,
                    width: 100.0,
                    height: 30.0,
                }),
                raw: serde_json::json!({}),
            },
            CachedElement {
                id: "e2".into(),
                role: "generic".into(),
                name: "Container".into(),
                selector: "div".into(),
                rect: None,
                raw: serde_json::json!({}),
            },
        ];
        let summary = build_elements_summary(&elements);
        let parsed: serde_json::Value = serde_json::from_str(&summary).unwrap();

        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["element_count"], 2);
        // Button is interactive and named, should appear in interactive_elements
        assert_eq!(parsed["interactive_elements"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["interactive_elements"][0]["id"], "e1");
        // Generic is not interactive, should not appear
        assert_eq!(parsed["unnamed_interactive_count"], 0);
    }

    #[test]
    fn test_build_summary_unnamed_elements() {
        let elements = vec![CachedElement {
            id: "e1".into(),
            role: "button".into(),
            name: "".into(),
            selector: "button.icon".into(),
            rect: Some(ElementRect {
                x: 50.0,
                y: 60.0,
                width: 32.0,
                height: 32.0,
            }),
            raw: serde_json::json!({}),
        }];
        let summary = build_elements_summary(&elements);
        let parsed: serde_json::Value = serde_json::from_str(&summary).unwrap();

        assert_eq!(parsed["interactive_elements"].as_array().unwrap().len(), 0);
        assert_eq!(parsed["unnamed_interactive_count"], 1);
        assert_eq!(parsed["unnamed_interactive_elements"][0]["id"], "e1");
        assert_eq!(parsed["unnamed_interactive_elements"][0]["name"], "");
    }

    #[test]
    fn test_build_summary_role_counts() {
        let elements = vec![
            CachedElement {
                id: "e1".into(),
                role: "button".into(),
                name: "A".into(),
                selector: "".into(),
                rect: None,
                raw: serde_json::json!({}),
            },
            CachedElement {
                id: "e2".into(),
                role: "button".into(),
                name: "B".into(),
                selector: "".into(),
                rect: None,
                raw: serde_json::json!({}),
            },
            CachedElement {
                id: "e3".into(),
                role: "link".into(),
                name: "C".into(),
                selector: "".into(),
                rect: None,
                raw: serde_json::json!({}),
            },
        ];
        let summary = build_elements_summary(&elements);
        let parsed: serde_json::Value = serde_json::from_str(&summary).unwrap();

        assert_eq!(parsed["roles"]["button"], 2);
        assert_eq!(parsed["roles"]["link"], 1);
    }

    #[test]
    fn test_find_elements_by_role() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        // Populate cache
        *agent.elements_cache.borrow_mut() = Some(ElementsCache {
            elements: vec![
                CachedElement {
                    id: "e1".into(),
                    role: "button".into(),
                    name: "Submit".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
                CachedElement {
                    id: "e2".into(),
                    role: "textbox".into(),
                    name: "Email".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
                CachedElement {
                    id: "e3".into(),
                    role: "button".into(),
                    name: "Cancel".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
            ],
        });

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_find_elements".into(),
            arguments: serde_json::json!({"role": "button"}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["count"], 2);
    }

    #[test]
    fn test_find_elements_by_name() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        *agent.elements_cache.borrow_mut() = Some(ElementsCache {
            elements: vec![
                CachedElement {
                    id: "e1".into(),
                    role: "button".into(),
                    name: "Submit Form".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
                CachedElement {
                    id: "e2".into(),
                    role: "button".into(),
                    name: "Cancel".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
            ],
        });

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_find_elements".into(),
            arguments: serde_json::json!({"name": "submit"}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["elements"][0]["id"], "e1");
    }

    #[test]
    fn test_find_elements_by_position() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        *agent.elements_cache.borrow_mut() = Some(ElementsCache {
            elements: vec![
                CachedElement {
                    id: "e1".into(),
                    role: "button".into(),
                    name: "Near".into(),
                    selector: "".into(),
                    rect: Some(ElementRect {
                        x: 100.0,
                        y: 100.0,
                        width: 20.0,
                        height: 20.0,
                    }),
                    raw: serde_json::json!({}),
                },
                CachedElement {
                    id: "e2".into(),
                    role: "button".into(),
                    name: "Far".into(),
                    selector: "".into(),
                    rect: Some(ElementRect {
                        x: 500.0,
                        y: 500.0,
                        width: 20.0,
                        height: 20.0,
                    }),
                    raw: serde_json::json!({}),
                },
            ],
        });

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_find_elements".into(),
            arguments: serde_json::json!({"near_x": 115, "near_y": 115, "radius": 50}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["elements"][0]["id"], "e1");
    }

    #[test]
    fn test_find_elements_unnamed_only() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        *agent.elements_cache.borrow_mut() = Some(ElementsCache {
            elements: vec![
                CachedElement {
                    id: "e1".into(),
                    role: "button".into(),
                    name: "Submit".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
                CachedElement {
                    id: "e2".into(),
                    role: "button".into(),
                    name: "".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
            ],
        });

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_find_elements".into(),
            arguments: serde_json::json!({"unnamed_only": true}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["elements"][0]["id"], "e2");
    }

    #[test]
    fn test_find_elements_combined_filters() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        *agent.elements_cache.borrow_mut() = Some(ElementsCache {
            elements: vec![
                CachedElement {
                    id: "e1".into(),
                    role: "button".into(),
                    name: "Submit".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
                CachedElement {
                    id: "e2".into(),
                    role: "link".into(),
                    name: "Submit Link".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
                CachedElement {
                    id: "e3".into(),
                    role: "button".into(),
                    name: "Cancel".into(),
                    selector: "".into(),
                    rect: None,
                    raw: serde_json::json!({}),
                },
            ],
        });

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_find_elements".into(),
            arguments: serde_json::json!({"role": "button", "name": "submit"}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["elements"][0]["id"], "e1");
    }

    #[test]
    fn test_element_info_found() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let raw = serde_json::json!({"name": "Submit", "role": "button", "extra_field": "value"});
        *agent.elements_cache.borrow_mut() = Some(ElementsCache {
            elements: vec![CachedElement {
                id: "e42".into(),
                role: "button".into(),
                name: "Submit".into(),
                selector: "".into(),
                rect: None,
                raw: raw.clone(),
            }],
        });

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_element_info".into(),
            arguments: serde_json::json!({"id": "e42"}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["element"]["extra_field"], "value");
    }

    #[test]
    fn test_element_info_not_found() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        *agent.elements_cache.borrow_mut() = Some(ElementsCache {
            elements: vec![CachedElement {
                id: "e1".into(),
                role: "button".into(),
                name: "X".into(),
                selector: "".into(),
                rect: None,
                raw: serde_json::json!({}),
            }],
        });

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_element_info".into(),
            arguments: serde_json::json!({"id": "e999"}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.content.contains("not found"));
    }

    #[test]
    fn test_cache_empty_error() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);
        // Don't populate cache

        let find_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_find_elements".into(),
            arguments: serde_json::json!({"role": "button"}),
            signature: None,
        };
        let result = agent.execute_tool(&find_call).unwrap();
        assert!(result.content.contains("No elements cached"));

        let info_call = ToolCall {
            id: "call-002".into(),
            call_id: None,
            name: "browser_element_info".into(),
            arguments: serde_json::json!({"id": "e1"}),
            signature: None,
        };
        let result = agent.execute_tool(&info_call).unwrap();
        assert!(result.content.contains("No elements cached"));
    }

    #[test]
    fn test_screenshot_cached_and_stripped() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_screenshot".into(),
            arguments: serde_json::json!({"full_page": false}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();

        // Content should NOT contain base64 data
        assert!(result.content.contains("screenshot_available"));
        assert!(!result.content.contains("iVBOR"));

        // Screenshot should be in cache
        let cached = agent.screenshot_cache.borrow();
        assert!(cached.is_some());
        assert!(cached.as_ref().unwrap().starts_with("iVBOR"));
    }

    #[test]
    fn test_browser_get_elements_caches() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "browser_get_elements".into(),
            arguments: serde_json::json!({}),
            signature: None,
        };
        let result = agent.execute_tool(&tool_call).unwrap();

        // Mock returns refs map format with selectors array
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["element_count"], 2);

        // Cache should be populated
        let cache = agent.elements_cache.borrow();
        assert!(cache.is_some());
        assert_eq!(cache.as_ref().unwrap().elements.len(), 2);
    }

    #[test]
    fn test_browser_tools_include_companion_tools() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let browser_tools = agent.get_browser_tools();
        assert!(browser_tools
            .iter()
            .any(|t| t.name == "browser_find_elements"));
        assert!(browser_tools
            .iter()
            .any(|t| t.name == "browser_element_info"));
    }

    #[test]
    fn test_switch_model_tool_registered_in_all_modes() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        for mode in [
            AgentMode::Chat,
            AgentMode::Browser,
            AgentMode::Agent,
            AgentMode::Code,
        ] {
            let tools = agent.get_tools_for_mode(mode);
            let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            assert!(
                tool_names.contains(&"switch_model"),
                "switch_model should be registered in {:?} mode",
                mode
            );
        }
    }

    #[test]
    fn test_switch_model_tool_schema() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tools = agent.get_tools_for_mode(AgentMode::Chat);
        let switch_model = tools.iter().find(|t| t.name == "switch_model").unwrap();

        // Verify it requires provider and model
        let required = switch_model.input_schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(required_names.contains(&"provider"));
        assert!(required_names.contains(&"model"));
    }

    #[test]
    fn test_switch_model_handler_empty_args() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "switch_model".into(),
            arguments: serde_json::json!({"provider": "", "model": ""}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.content.contains("error"));
        assert!(result.content.contains("required"));
    }

    #[test]
    fn test_execute_tool_subagent_spawn_with_tab_id() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tool_call = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Read page", "mode": "browser", "tab_id": 42}),
            signature: None,
        };

        let result = agent.execute_tool(&tool_call).unwrap();
        assert!(result.success);
        assert!(result.content.contains("Spawned sub-agent with ID:"));
    }

    #[test]
    fn test_execute_tool_subagent_wait_all() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        // Spawn two subagents
        let spawn1 = ToolCall {
            id: "call-001".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Task A"}),
            signature: None,
        };
        agent.execute_tool(&spawn1).unwrap();

        let spawn2 = ToolCall {
            id: "call-002".into(),
            call_id: None,
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Task B"}),
            signature: None,
        };
        agent.execute_tool(&spawn2).unwrap();

        // Wait for both
        let wait_all = ToolCall {
            id: "call-003".into(),
            call_id: None,
            name: "subagent_wait_all".into(),
            arguments: serde_json::json!({"ids": [1, 2]}),
            signature: None,
        };

        let result = agent.execute_tool(&wait_all).unwrap();
        assert!(result.success);
        assert!(result.content.contains("Task A"));
        assert!(result.content.contains("Task B"));
    }

    #[test]
    fn test_subagent_browser_read_tools() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let read_tools = agent.get_subagent_browser_read_tools();
        let chat_tools = agent.get_chat_tools();

        // Subagent browser read tools should be the same as chat tools
        // (which already include browser_get_content, browser_get_markdown, browser_screenshot)
        assert_eq!(read_tools.len(), chat_tools.len());

        // Verify read-only browser tools are present
        assert!(read_tools.iter().any(|t| t.name == "browser_get_content"));
        assert!(read_tools.iter().any(|t| t.name == "browser_get_markdown"));
        assert!(read_tools.iter().any(|t| t.name == "browser_screenshot"));

        // Verify interaction tools are NOT present
        assert!(!read_tools.iter().any(|t| t.name == "browser_click"));
        assert!(!read_tools.iter().any(|t| t.name == "browser_click_by_id"));
        assert!(!read_tools.iter().any(|t| t.name == "browser_type"));
        assert!(!read_tools.iter().any(|t| t.name == "browser_fill"));
        assert!(!read_tools.iter().any(|t| t.name == "browser_scroll"));
        assert!(!read_tools.iter().any(|t| t.name == "browser_navigate"));
        assert!(!read_tools.iter().any(|t| t.name == "browser_eval_js"));
    }

    #[test]
    fn test_subagent_tools_for_mode_browser() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tools = agent.get_subagent_tools_for_mode(AgentMode::Browser);
        // Should be read-only browser tools (same as chat tools)
        assert!(tools.iter().any(|t| t.name == "browser_get_markdown"));
        assert!(!tools.iter().any(|t| t.name == "browser_click"));
        assert!(!tools.iter().any(|t| t.name == "read")); // No file tools in browser mode
    }

    #[test]
    fn test_subagent_tools_for_mode_agent() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let tools = agent.get_subagent_tools_for_mode(AgentMode::Agent);
        // Should have read-only browser tools + file tools
        assert!(tools.iter().any(|t| t.name == "browser_get_markdown"));
        assert!(tools.iter().any(|t| t.name == "read"));
        assert!(tools.iter().any(|t| t.name == "write"));
        assert!(tools.iter().any(|t| t.name == "bash"));
        assert!(tools.iter().any(|t| t.name == "grep"));
        // But no browser interaction tools
        assert!(!tools.iter().any(|t| t.name == "browser_click"));
        assert!(!tools.iter().any(|t| t.name == "browser_navigate"));
    }

    #[test]
    fn test_agent_config_for_subagent_is_subagent() {
        let config = AgentConfig::for_subagent();
        assert!(config.is_subagent);
        assert!(!config.use_streaming);
        assert!(config.suppress_streaming);
    }

    #[test]
    fn test_agent_tools_include_subagent_wait_all() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let agent_tools = agent.get_agent_tools();
        assert!(agent_tools.iter().any(|t| t.name == "subagent_wait_all"));
    }

    #[test]
    fn test_subagent_spawn_schema_has_tab_id() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let agent_tools = agent.get_agent_tools();
        let spawn_tool = agent_tools
            .iter()
            .find(|t| t.name == "subagent_spawn")
            .unwrap();
        let props = spawn_tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("tab_id"));
    }
}
