//! Agent loop implementation.
//!
//! This module contains the core agent logic that:
//! - Constructs prompts based on mode
//! - Calls the LLM
//! - Executes tool calls
//! - Manages the conversation loop

use crate::host::{HostFunctions, HostResult};
use crate::types::*;
use nevoflux_protocol::LocalFileRef;

/// Format local file references for injection into user message.
#[allow(dead_code)]
fn format_local_files(files: &[LocalFileRef]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let mut result = String::from("用户附加了以下本地文件/目录：\n");

    for file in files {
        let type_str = if file.is_directory { "目录" } else { "文件" };
        let size_str = file
            .size
            .map(format_file_size)
            .unwrap_or_default();

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
#[allow(dead_code)]
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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: MAX_ITERATIONS,
            use_streaming: true,
            suppress_streaming: false,
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
        }
    }

    /// Set whether to suppress streaming output.
    pub fn with_suppress_streaming(mut self, suppress: bool) -> Self {
        self.suppress_streaming = suppress;
        self
    }
}

/// The built-in agent.
pub struct Agent<H: HostFunctions> {
    /// Host functions interface.
    host: H,
    /// Configuration.
    config: AgentConfig,
}

impl<H: HostFunctions> Agent<H> {
    /// Create a new agent with the given host functions.
    pub fn new(host: H) -> Self {
        Self {
            host,
            config: AgentConfig::default(),
        }
    }

    /// Create a new agent with custom configuration.
    pub fn with_config(host: H, config: AgentConfig) -> Self {
        Self { host, config }
    }

    /// Run the agent for a single turn.
    pub fn run(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        // Use custom system prompt if provided, otherwise use mode-based prompt
        let system_prompt = match &input.custom_system_prompt {
            Some(custom) => custom.clone(),
            None => self.build_system_prompt_for_mode(input.mode),
        };

        let tools = self.get_tools_for_mode(input.mode);
        self.run_loop(input, &system_prompt, &tools)
    }

    /// Build system prompt for a specific mode.
    fn build_system_prompt_for_mode(&self, mode: AgentMode) -> String {
        match mode {
            AgentMode::Chat => self.build_chat_system_prompt(),
            AgentMode::Browser => self.build_browser_system_prompt(),
            AgentMode::Agent => self.build_agent_system_prompt(),
        }
    }

    /// Get tools for a specific mode.
    fn get_tools_for_mode(&self, mode: AgentMode) -> Vec<ToolDefinition> {
        match mode {
            AgentMode::Chat => self.get_chat_tools(),
            AgentMode::Browser => self.get_browser_tools(),
            AgentMode::Agent => self.get_agent_tools(),
        }
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

        // Create user message with attachments if present
        if input.attachments.is_empty() {
            messages.push(Message::user(&input.user_message));
        } else {
            messages.push(Message::user_with_attachments(
                &input.user_message,
                input.attachments.clone(),
            ));
        }

        let mut iterations = 0;
        let mut final_text = String::new();
        let mut all_tool_calls = Vec::new();

        loop {
            iterations += 1;
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

            // Execute tool calls
            messages.push(Message::assistant(&response.text));
            all_tool_calls.extend(response.tool_calls.clone());

            for tool_call in &response.tool_calls {
                let result = self.execute_tool(tool_call)?;
                messages.push(Message::tool(&tool_call.id, &result.content));

                // Check interrupt after each tool execution
                if self.host.is_interrupted()? {
                    break;
                }
            }

            // Check if we should exit the outer loop due to interrupt
            if self.host.is_interrupted()? {
                break;
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
        let mut accumulated_tool_calls: Vec<ToolCall> = Vec::new();

        // Read chunks until done
        loop {
            // Check for interrupt
            if self.host.is_interrupted()? {
                self.host.llm_stream_close(stream_id)?;
                break;
            }

            match self.host.llm_stream_next(stream_id)? {
                Some(chunk) => {
                    // Accumulate text
                    if let Some(ref text) = chunk.text {
                        if !text.is_empty() {
                            accumulated_text.push_str(text);

                            // Emit to sidebar
                            self.host.stream_emit(text)?;
                        }
                    }

                    // Accumulate tool calls
                    accumulated_tool_calls.extend(chunk.tool_calls);

                    if chunk.done {
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
            tool_calls: accumulated_tool_calls,
        })
    }

    /// Execute a single tool call.
    fn execute_tool(&self, tool_call: &ToolCall) -> HostResult<ToolResult> {
        let content = match tool_call.name.as_str() {
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
                self.host.tool_read(path, offset, limit)?
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
                self.host.tool_bash(command, timeout)?
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
                let matches = self.host.tool_grep(pattern, path, file_type)?;
                matches.join("\n")
            }
            "memory_search" => {
                let query = tool_call.arguments["query"].as_str().unwrap_or("");
                let limit = tool_call.arguments["limit"].as_u64().unwrap_or(10) as usize;
                let chunks = self.host.memory_search(query, limit)?;
                serde_json::to_string_pretty(&chunks).unwrap_or_default()
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
                let arguments = tool_call
                    .arguments
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                self.host.tool_call_dynamic(tool_name, &arguments)?
            }
            // Browser tools
            "browser_navigate" => {
                let url = tool_call.arguments["url"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_navigate(url, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_click" => {
                let selector = tool_call.arguments["selector"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_click(selector, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_click_by_id" => {
                let element_id = tool_call.arguments["element_id"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_click_by_id(element_id, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_type" => {
                let selector = tool_call.arguments["selector"].as_str().unwrap_or("");
                let text = tool_call.arguments["text"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_type(selector, text, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_type_by_id" => {
                let element_id = tool_call.arguments["element_id"].as_str().unwrap_or("");
                let text = tool_call.arguments["text"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_type_by_id(element_id, text, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_fill" => {
                let selector = tool_call.arguments["selector"].as_str().unwrap_or("");
                let value = tool_call.arguments["value"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_fill(selector, value, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_fill_by_id" => {
                let element_id = tool_call.arguments["element_id"].as_str().unwrap_or("");
                let value = tool_call.arguments["value"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_fill_by_id(element_id, value, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
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
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_eval_js" => {
                let script = tool_call.arguments["script"].as_str().unwrap_or("");
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_eval_js(script, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_scroll" => {
                let direction = tool_call.arguments["direction"].as_str().unwrap_or("down");
                let amount = tool_call.arguments["amount"].as_i64().unwrap_or(500) as i32;
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_scroll(direction, amount, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            "browser_wait_for" => {
                let selector = tool_call.arguments["selector"].as_str().unwrap_or("");
                let timeout_ms = tool_call.arguments["timeout_ms"].as_u64().unwrap_or(10000);
                let tab_id = tool_call.arguments["tab_id"].as_i64();
                let result = self.host.browser_wait_for(selector, timeout_ms, tab_id)?;
                serde_json::to_string(&result).unwrap_or_default()
            }
            // Subagent tools
            "subagent_spawn" => {
                let task = tool_call.arguments["task"].as_str().unwrap_or("");
                let mode = tool_call.arguments["mode"].as_str().unwrap_or("agent");
                let id = self.host.subagent_spawn(task, mode)?;
                format!("Spawned sub-agent with ID: {}", id)
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

        Ok(ToolResult {
            tool_call_id: tool_call.id.clone(),
            content,
            success: true,
        })
    }

    /// Build system prompt for chat mode.
    fn build_chat_system_prompt(&self) -> String {
        let base_prompt = r#"You are a helpful AI assistant integrated into a web browser.

You can:
- Answer questions and have conversations
- Search the web for current information
- Read and understand the current page content
- Ask the user clarifying questions

You cannot:
- Interact with the page (click, type, etc.)
- Access local files
- Execute commands

Be helpful, accurate, and concise."#;

        self.append_skills_section(base_prompt)
    }

    /// Build system prompt for browser mode.
    fn build_browser_system_prompt(&self) -> String {
        let base_prompt = r#"You are a helpful AI assistant with browser automation capabilities.

You can:
- Everything from chat mode
- Navigate to URLs using browser_navigate
- Click on elements using browser_click or browser_click_by_id
- Type text into inputs using browser_type or browser_type_by_id (simulates keystrokes)
- Fill form fields using browser_fill or browser_fill_by_id (sets value directly)
- Get page content as text/HTML using browser_get_content
- Get page content as markdown using browser_get_markdown
- Take screenshots using browser_screenshot
- Execute JavaScript using browser_eval_js
- Scroll the page using browser_scroll
- Wait for elements to appear using browser_wait_for

Use browser automation to help users accomplish tasks on web pages.
Always confirm before taking actions that might have side effects.
Prefer using *_by_id variants when element IDs are available for more reliable targeting."#;

        self.append_skills_section(base_prompt)
    }

    /// Build system prompt for agent mode.
    fn build_agent_system_prompt(&self) -> String {
        let base_prompt = r#"You are a powerful AI agent with full system access.

You can:
- Everything from browser mode
- Read, write, and edit local files
- Execute bash commands
- Use computer control (mouse, keyboard)
- Call MCP servers
- Spawn sub-agents for parallel work

Think step by step. Use tools to gather information before making changes.
Always verify your work after making modifications.
Ask for permission before destructive operations."#;

        self.append_skills_section(base_prompt)
    }

    /// Append available skills section to a base prompt.
    fn append_skills_section(&self, base_prompt: &str) -> String {
        match self.host.skill_list() {
            Ok(skills) if !skills.is_empty() => {
                let summaries = format_skill_summaries(&skills);
                format!(
                    "{}\n\n## Available Skills\n\n{}\n\nUse skill_load(name) to load a skill's full content.",
                    base_prompt, summaries
                )
            }
            _ => base_prompt.to_string(),
        }
    }

    /// Get available tools for chat mode.
    fn get_chat_tools(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "web_search".into(),
                description: "Search the web for information".into(),
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
                description: "Fetch and analyze content from a URL".into(),
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
        ]
    }

    /// Get available tools for browser mode.
    fn get_browser_tools(&self) -> Vec<ToolDefinition> {
        let mut tools = self.get_chat_tools();

        // Browser navigation
        tools.push(ToolDefinition {
            name: "browser_navigate".into(),
            description: "Navigate to a URL in the browser".into(),
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
            description: "Click on an element by its ID attribute".into(),
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
            description: "Type text into an element by ID (simulates keystrokes)".into(),
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
            description: "Fill an input element with a value by ID (sets value directly)".into(),
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

        // Get content
        tools.push(ToolDefinition {
            name: "browser_get_content".into(),
            description: "Get the current page content as text/HTML".into(),
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

        // Get markdown
        tools.push(ToolDefinition {
            name: "browser_get_markdown".into(),
            description: "Get the current page content as markdown".into(),
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

        // Screenshot
        tools.push(ToolDefinition {
            name: "browser_screenshot".into(),
            description: "Take a screenshot of the current page".into(),
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
                        "description": "Optional tab ID"
                    }
                }
            }),
        });

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
            description: "Scroll the page in a direction".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"],
                        "description": "Direction to scroll"
                    },
                    "amount": {
                        "type": "integer",
                        "description": "Amount to scroll in pixels (default: 500)",
                        "default": 500
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

        tools
    }

    /// Get available tools for agent mode.
    fn get_agent_tools(&self) -> Vec<ToolDefinition> {
        let mut tools = self.get_browser_tools();

        // Add file tools
        tools.push(ToolDefinition {
            name: "read".into(),
            description: "Read a file from the filesystem".into(),
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
        });

        tools.push(ToolDefinition {
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
        });

        tools.push(ToolDefinition {
            name: "bash".into(),
            description: "Execute a bash command".into(),
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
        });

        tools.push(ToolDefinition {
            name: "grep".into(),
            description: "Search file contents".into(),
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
                        "description": "File type filter (e.g., 'rs', 'py')"
                    }
                },
                "required": ["pattern"]
            }),
        });

        // Dynamic tool discovery
        tools.push(ToolDefinition {
            name: "tool_search".into(),
            description: "Search for available tools by keyword. Use this when you need \
                          capabilities not provided by the built-in tools."
                .into(),
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
            description: "Call a tool discovered via tool_search. Provide the exact tool name \
                          and arguments matching the tool's input_schema."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "The exact name of the tool to call"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments to pass to the tool"
                    }
                },
                "required": ["tool_name", "arguments"]
            }),
        });

        // Subagent tools for parallel work
        tools.push(ToolDefinition {
            name: "subagent_spawn".into(),
            description: "Spawn a sub-agent to execute a task in parallel. The sub-agent runs \
                          independently and can be monitored or waited on."
                .into(),
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

/// Format skill summaries for system prompt injection.
fn format_skill_summaries(skills: &[SkillSummary]) -> String {
    skills
        .iter()
        .map(|s| format!("- **{}**: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n")
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
        };

        // Should run successfully with custom prompt
        let output = agent.run(&input).unwrap();
        assert!(!output.continue_loop);
    }

    #[test]
    fn test_build_system_prompt_for_mode() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let chat_prompt = agent.build_system_prompt_for_mode(AgentMode::Chat);
        assert!(chat_prompt.contains("helpful AI assistant"));

        let browser_prompt = agent.build_system_prompt_for_mode(AgentMode::Browser);
        assert!(browser_prompt.contains("browser automation"));

        let agent_prompt = agent.build_system_prompt_for_mode(AgentMode::Agent);
        assert!(agent_prompt.contains("full system access"));
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
                name: "web_search".into(),
                arguments: serde_json::json!({"query": "rust programming"}),
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
        };

        let output = agent.run(&input).unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert!(output.text.contains("Rust"));
    }

    #[test]
    fn test_agent_system_prompts() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let chat_prompt = agent.build_chat_system_prompt();
        assert!(chat_prompt.contains("helpful AI assistant"));
        assert!(chat_prompt.contains("web browser"));

        let browser_prompt = agent.build_browser_system_prompt();
        assert!(browser_prompt.contains("browser automation"));

        let agent_prompt = agent.build_agent_system_prompt();
        assert!(agent_prompt.contains("full system access"));
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
    fn test_system_prompts_with_skills() {
        let mock = MockHostFunctions::new();
        mock.add_skill(SkillSummary {
            name: "code-review".into(),
            description: "Review code for issues".into(),
            tags: vec![],
        });
        mock.add_skill(SkillSummary {
            name: "tdd".into(),
            description: "Test-driven development".into(),
            tags: vec![],
        });

        let agent = Agent::new(mock);

        // Chat prompt should include skills section
        let chat_prompt = agent.build_chat_system_prompt();
        assert!(chat_prompt.contains("## Available Skills"));
        assert!(chat_prompt.contains("**code-review**"));
        assert!(chat_prompt.contains("**tdd**"));
        assert!(chat_prompt.contains("Use skill_load(name)"));

        // Browser prompt should include skills section
        let browser_prompt = agent.build_browser_system_prompt();
        assert!(browser_prompt.contains("## Available Skills"));

        // Agent prompt should include skills section
        let agent_prompt = agent.build_agent_system_prompt();
        assert!(agent_prompt.contains("## Available Skills"));
    }

    #[test]
    fn test_system_prompts_without_skills() {
        let mock = MockHostFunctions::new();
        // Don't add any skills
        let agent = Agent::new(mock);

        // Prompts should not have skills section when no skills available
        let chat_prompt = agent.build_chat_system_prompt();
        assert!(!chat_prompt.contains("## Available Skills"));

        let browser_prompt = agent.build_browser_system_prompt();
        assert!(!browser_prompt.contains("## Available Skills"));

        let agent_prompt = agent.build_agent_system_prompt();
        assert!(!agent_prompt.contains("## Available Skills"));
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
            name: "web_search".into(),
            arguments: serde_json::json!({"query": "test"}),
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
            name: "unknown_tool".into(),
            arguments: serde_json::json!({}),
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
            name: "tool_search".into(),
            arguments: serde_json::json!({"query": "file", "max_results": 5}),
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
            name: "tool_call_dynamic".into(),
            arguments: serde_json::json!({
                "tool_name": "read_file",
                "arguments": {"path": "/test.txt"}
            }),
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
            name: "browser_navigate".into(),
            arguments: serde_json::json!({"url": "https://example.com"}),
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
            name: "browser_click".into(),
            arguments: serde_json::json!({"selector": "#submit-btn"}),
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
            name: "browser_type".into(),
            arguments: serde_json::json!({"selector": "#input", "text": "Hello World"}),
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
            name: "browser_screenshot".into(),
            arguments: serde_json::json!({"full_page": true}),
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
            name: "browser_scroll".into(),
            arguments: serde_json::json!({"direction": "down", "amount": 500}),
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
            name: "browser_wait_for".into(),
            arguments: serde_json::json!({"selector": "#loading", "timeout_ms": 5000}),
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
        // Browser tools = chat tools + 13 browser-specific tools
        assert_eq!(browser_tools.len(), chat_tools.len() + 13);
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
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Search for files", "mode": "agent"}),
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
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Do something"}),
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
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Test task"}),
        };
        agent.execute_tool(&spawn_call).unwrap();

        // Then check its status
        let status_call = ToolCall {
            id: "call-002".into(),
            name: "subagent_status".into(),
            arguments: serde_json::json!({"id": 1}),
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
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Test task"}),
        };
        agent.execute_tool(&spawn_call).unwrap();

        // Then wait for it
        let wait_call = ToolCall {
            id: "call-002".into(),
            name: "subagent_wait".into(),
            arguments: serde_json::json!({"id": 1}),
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
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Long running task"}),
        };
        agent.execute_tool(&spawn_call).unwrap();

        // Then kill it
        let kill_call = ToolCall {
            id: "call-002".into(),
            name: "subagent_kill".into(),
            arguments: serde_json::json!({"id": 1}),
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
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Task 1", "mode": "agent"}),
        };
        agent.execute_tool(&spawn_call1).unwrap();

        let spawn_call2 = ToolCall {
            id: "call-002".into(),
            name: "subagent_spawn".into(),
            arguments: serde_json::json!({"task": "Task 2", "mode": "browser"}),
        };
        agent.execute_tool(&spawn_call2).unwrap();

        // Then list them
        let list_call = ToolCall {
            id: "call-003".into(),
            name: "subagent_list".into(),
            arguments: serde_json::json!({}),
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
}
