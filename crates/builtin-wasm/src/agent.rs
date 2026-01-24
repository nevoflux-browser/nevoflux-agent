//! Agent loop implementation.
//!
//! This module contains the core agent logic that:
//! - Constructs prompts based on mode
//! - Calls the LLM
//! - Executes tool calls
//! - Manages the conversation loop

use crate::host::{HostFunctions, HostResult};
use crate::types::*;

/// Maximum iterations in the agent loop to prevent infinite loops.
const MAX_ITERATIONS: usize = 100;

/// Agent configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum iterations before stopping.
    pub max_iterations: usize,
    /// Whether to use streaming.
    pub use_streaming: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: MAX_ITERATIONS,
            use_streaming: true,
        }
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
        match input.mode {
            AgentMode::Chat => self.run_chat(input),
            AgentMode::Browser => self.run_browser(input),
            AgentMode::Agent => self.run_agent(input),
        }
    }

    /// Run in chat mode.
    fn run_chat(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        let system_prompt = self.build_chat_system_prompt();
        let tools = self.get_chat_tools();

        self.run_loop(input, &system_prompt, &tools)
    }

    /// Run in browser mode.
    fn run_browser(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        let system_prompt = self.build_browser_system_prompt();
        let tools = self.get_browser_tools();

        self.run_loop(input, &system_prompt, &tools)
    }

    /// Run in agent mode.
    fn run_agent(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        let system_prompt = self.build_agent_system_prompt();
        let tools = self.get_agent_tools();

        self.run_loop(input, &system_prompt, &tools)
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
        messages.push(Message::user(&input.user_message));

        let mut iterations = 0;
        let mut final_text = String::new();
        let mut all_tool_calls = Vec::new();

        loop {
            iterations += 1;
            if iterations > self.config.max_iterations {
                break;
            }

            // Call LLM
            let request = LlmRequest {
                messages: messages.clone(),
                tools: tools.to_vec(),
                stream: false,
            };

            let response = self.host.llm_chat(&request)?;

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
            }
        }

        Ok(AgentOutput {
            text: final_text,
            tool_calls: all_tool_calls,
            continue_loop: false,
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
            "skill_list" => {
                let skills = self.host.skill_list()?;
                serde_json::to_string_pretty(&skills).unwrap_or_default()
            }
            "skill_load" => {
                let name = tool_call.arguments["name"].as_str().unwrap_or("");
                self.host.skill_load(name)?
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
        r#"You are a helpful AI assistant integrated into a web browser.

You can:
- Answer questions and have conversations
- Search the web for current information
- Read and understand the current page content
- Ask the user clarifying questions

You cannot:
- Interact with the page (click, type, etc.)
- Access local files
- Execute commands

Be helpful, accurate, and concise."#
            .to_string()
    }

    /// Build system prompt for browser mode.
    fn build_browser_system_prompt(&self) -> String {
        r#"You are a helpful AI assistant with browser automation capabilities.

You can:
- Everything from chat mode
- Click on elements
- Type text into inputs
- Navigate between pages
- Take screenshots

Use browser automation to help users accomplish tasks on web pages.
Always confirm before taking actions that might have side effects."#
            .to_string()
    }

    /// Build system prompt for agent mode.
    fn build_agent_system_prompt(&self) -> String {
        r#"You are a powerful AI agent with full system access.

You can:
- Everything from browser mode
- Read, write, and edit local files
- Execute bash commands
- Use computer control (mouse, keyboard)
- Call MCP servers
- Spawn sub-agents for parallel work

Think step by step. Use tools to gather information before making changes.
Always verify your work after making modifications.
Ask for permission before destructive operations."#
            .to_string()
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
                name: "skill_list".into(),
                description: "List available skills".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
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
        // Browser tools would be added here (click, type, navigate, etc.)
        // These are provided by the browser extension via Host Functions
        self.get_chat_tools()
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

        tools
    }
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
        };
        let agent = Agent::with_config(mock, config);
        assert_eq!(agent.config.max_iterations, 50);
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

        let agent = Agent::new(mock);
        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Chat,
            user_message: "Tell me about Rust".into(),
            history: vec![],
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
    fn test_agent_get_tools() {
        let mock = MockHostFunctions::new();
        let agent = Agent::new(mock);

        let chat_tools = agent.get_chat_tools();
        assert!(chat_tools.iter().any(|t| t.name == "web_search"));
        assert!(chat_tools.iter().any(|t| t.name == "ask_user"));

        let browser_tools = agent.get_browser_tools();
        assert!(browser_tools.len() >= chat_tools.len());

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
}
