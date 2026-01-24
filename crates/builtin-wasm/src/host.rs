//! Host function bindings.
//!
//! These are the external functions provided by the Wasmtime host.
//! When compiled for wasm32-wasi, these become actual imports.
//! For native testing, we provide mock implementations.

use crate::types::*;

/// Result type for host function calls.
pub type HostResult<T> = Result<T, HostError>;

/// Error from host function calls.
#[derive(Debug, Clone)]
pub struct HostError {
    /// Error code.
    pub code: i32,
    /// Error message.
    pub message: String,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Host error ({}): {}", self.code, self.message)
    }
}

impl std::error::Error for HostError {}

/// Host function interface.
///
/// This trait defines all host functions available to the Wasm guest.
/// The actual implementation is provided by the Wasmtime host at runtime.
pub trait HostFunctions {
    // =========================================================================
    // LLM Functions
    // =========================================================================

    /// Send a chat request to the LLM.
    fn llm_chat(&self, request: &LlmRequest) -> HostResult<LlmResponse>;

    /// Start a streaming chat request.
    fn llm_stream_start(&self, request: &LlmRequest) -> HostResult<u64>;

    /// Read the next chunk from a stream.
    fn llm_stream_next(&self, stream_id: u64) -> HostResult<Option<LlmChunk>>;

    /// Close a stream.
    fn llm_stream_close(&self, stream_id: u64) -> HostResult<()>;

    // =========================================================================
    // Memory Functions
    // =========================================================================

    /// Search memory.
    fn memory_search(&self, query: &str, limit: usize) -> HostResult<Vec<MemoryChunk>>;

    /// Create a memory chunk.
    fn memory_create(&self, content: &str, metadata: &serde_json::Value) -> HostResult<String>;

    /// Update a memory chunk.
    fn memory_update(&self, id: &str, content: &str) -> HostResult<()>;

    /// Delete a memory chunk.
    fn memory_delete(&self, id: &str) -> HostResult<()>;

    // =========================================================================
    // Skills Functions
    // =========================================================================

    /// List available skills (Level 1 loading).
    fn skill_list(&self) -> HostResult<Vec<SkillSummary>>;

    /// Load a skill's full content (Level 2 loading).
    fn skill_load(&self, name: &str) -> HostResult<String>;

    /// Read skill auxiliary files (Level 3 loading).
    fn skill_read(&self, name: &str, path: &str) -> HostResult<String>;

    /// Execute a skill script (Level 3 loading).
    fn skill_execute(
        &self,
        name: &str,
        script: &str,
        args: &serde_json::Value,
    ) -> HostResult<String>;

    // =========================================================================
    // Built-in Tools
    // =========================================================================

    /// Read a file.
    fn tool_read(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> HostResult<String>;

    /// Write a file.
    fn tool_write(&self, path: &str, content: &str) -> HostResult<()>;

    /// Edit a file (search and replace).
    fn tool_edit(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> HostResult<()>;

    /// Execute a bash command.
    fn tool_bash(&self, command: &str, timeout_ms: Option<u64>) -> HostResult<String>;

    /// Glob file patterns.
    fn tool_glob(&self, pattern: &str, path: Option<&str>) -> HostResult<Vec<String>>;

    /// Search file contents.
    fn tool_grep(
        &self,
        pattern: &str,
        path: Option<&str>,
        file_type: Option<&str>,
    ) -> HostResult<Vec<String>>;

    /// Web search.
    fn tool_web_search(&self, query: &str) -> HostResult<String>;

    /// Fetch a URL.
    fn tool_web_fetch(&self, url: &str, prompt: &str) -> HostResult<String>;

    /// Ask user a question.
    fn tool_ask_user(&self, question: &str, options: &[String]) -> HostResult<String>;

    // =========================================================================
    // Permission Functions
    // =========================================================================

    /// Request permission for an action.
    fn permission_request(
        &self,
        resource_type: &str,
        action: &str,
        resource: &str,
    ) -> HostResult<bool>;

    /// Check if permission is already granted.
    fn permission_check(
        &self,
        resource_type: &str,
        action: &str,
        resource: &str,
    ) -> HostResult<bool>;

    // =========================================================================
    // Built-in Proxy (for plugins to inherit built-in capabilities)
    // =========================================================================

    /// Invoke built-in chat mode.
    fn builtin_chat(&self, input: &AgentInput) -> HostResult<AgentOutput>;

    /// Invoke built-in browser mode.
    fn builtin_browser(&self, input: &AgentInput) -> HostResult<AgentOutput>;

    /// Invoke built-in agent mode.
    fn builtin_agent(&self, input: &AgentInput) -> HostResult<AgentOutput>;
}

/// Mock host functions for testing.
#[cfg(test)]
pub struct MockHostFunctions {
    /// Simulated LLM responses.
    pub llm_responses: std::cell::RefCell<Vec<LlmResponse>>,
    /// Simulated skills.
    pub skills: std::cell::RefCell<Vec<SkillSummary>>,
}

#[cfg(test)]
impl Default for MockHostFunctions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl MockHostFunctions {
    /// Create a new mock with default behavior.
    pub fn new() -> Self {
        Self {
            llm_responses: std::cell::RefCell::new(vec![]),
            skills: std::cell::RefCell::new(vec![]),
        }
    }

    /// Add an LLM response.
    pub fn add_llm_response(&self, response: LlmResponse) {
        self.llm_responses.borrow_mut().push(response);
    }

    /// Add a skill.
    pub fn add_skill(&self, skill: SkillSummary) {
        self.skills.borrow_mut().push(skill);
    }
}

#[cfg(test)]
impl HostFunctions for MockHostFunctions {
    fn llm_chat(&self, _request: &LlmRequest) -> HostResult<LlmResponse> {
        let mut responses = self.llm_responses.borrow_mut();
        if responses.is_empty() {
            Ok(LlmResponse {
                text: "Mock response".into(),
                tool_calls: vec![],
            })
        } else {
            Ok(responses.remove(0))
        }
    }

    fn llm_stream_start(&self, _request: &LlmRequest) -> HostResult<u64> {
        Ok(1) // Mock stream ID
    }

    fn llm_stream_next(&self, _stream_id: u64) -> HostResult<Option<LlmChunk>> {
        Ok(Some(LlmChunk {
            text: Some("Mock".into()),
            tool_calls: vec![],
            done: true,
        }))
    }

    fn llm_stream_close(&self, _stream_id: u64) -> HostResult<()> {
        Ok(())
    }

    fn memory_search(&self, _query: &str, _limit: usize) -> HostResult<Vec<MemoryChunk>> {
        Ok(vec![])
    }

    fn memory_create(&self, _content: &str, _metadata: &serde_json::Value) -> HostResult<String> {
        Ok("mem-001".into())
    }

    fn memory_update(&self, _id: &str, _content: &str) -> HostResult<()> {
        Ok(())
    }

    fn memory_delete(&self, _id: &str) -> HostResult<()> {
        Ok(())
    }

    fn skill_list(&self) -> HostResult<Vec<SkillSummary>> {
        Ok(self.skills.borrow().clone())
    }

    fn skill_load(&self, _name: &str) -> HostResult<String> {
        Ok("# Mock Skill\n\nContent here.".into())
    }

    fn skill_read(&self, _name: &str, _path: &str) -> HostResult<String> {
        Ok("File content".into())
    }

    fn skill_execute(
        &self,
        _name: &str,
        _script: &str,
        _args: &serde_json::Value,
    ) -> HostResult<String> {
        Ok("Execution result".into())
    }

    fn tool_read(
        &self,
        _path: &str,
        _offset: Option<u64>,
        _limit: Option<u64>,
    ) -> HostResult<String> {
        Ok("File content".into())
    }

    fn tool_write(&self, _path: &str, _content: &str) -> HostResult<()> {
        Ok(())
    }

    fn tool_edit(
        &self,
        _path: &str,
        _old_string: &str,
        _new_string: &str,
        _replace_all: bool,
    ) -> HostResult<()> {
        Ok(())
    }

    fn tool_bash(&self, _command: &str, _timeout_ms: Option<u64>) -> HostResult<String> {
        Ok("Command output".into())
    }

    fn tool_glob(&self, _pattern: &str, _path: Option<&str>) -> HostResult<Vec<String>> {
        Ok(vec!["file1.rs".into(), "file2.rs".into()])
    }

    fn tool_grep(
        &self,
        _pattern: &str,
        _path: Option<&str>,
        _file_type: Option<&str>,
    ) -> HostResult<Vec<String>> {
        Ok(vec!["match1".into()])
    }

    fn tool_web_search(&self, _query: &str) -> HostResult<String> {
        Ok("Search results".into())
    }

    fn tool_web_fetch(&self, _url: &str, _prompt: &str) -> HostResult<String> {
        Ok("Fetched content".into())
    }

    fn tool_ask_user(&self, _question: &str, _options: &[String]) -> HostResult<String> {
        Ok("User answer".into())
    }

    fn permission_request(
        &self,
        _resource_type: &str,
        _action: &str,
        _resource: &str,
    ) -> HostResult<bool> {
        Ok(true) // Always grant in mock
    }

    fn permission_check(
        &self,
        _resource_type: &str,
        _action: &str,
        _resource: &str,
    ) -> HostResult<bool> {
        Ok(true) // Always granted in mock
    }

    fn builtin_chat(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        Ok(AgentOutput {
            text: format!("Chat response to: {}", input.user_message),
            tool_calls: vec![],
            continue_loop: false,
        })
    }

    fn builtin_browser(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        Ok(AgentOutput {
            text: format!("Browser response to: {}", input.user_message),
            tool_calls: vec![],
            continue_loop: false,
        })
    }

    fn builtin_agent(&self, input: &AgentInput) -> HostResult<AgentOutput> {
        Ok(AgentOutput {
            text: format!("Agent response to: {}", input.user_message),
            tool_calls: vec![],
            continue_loop: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_error_display() {
        let err = HostError {
            code: 404,
            message: "Not found".into(),
        };
        assert!(err.to_string().contains("404"));
        assert!(err.to_string().contains("Not found"));
    }

    #[test]
    fn test_mock_host_functions_llm_chat() {
        let mock = MockHostFunctions::new();
        let request = LlmRequest {
            messages: vec![Message::user("Hello")],
            tools: vec![],
            stream: false,
        };
        let response = mock.llm_chat(&request).unwrap();
        assert_eq!(response.text, "Mock response");
    }

    #[test]
    fn test_mock_host_functions_llm_chat_with_response() {
        let mock = MockHostFunctions::new();
        mock.add_llm_response(LlmResponse {
            text: "Custom response".into(),
            tool_calls: vec![],
        });

        let request = LlmRequest {
            messages: vec![Message::user("Hello")],
            tools: vec![],
            stream: false,
        };
        let response = mock.llm_chat(&request).unwrap();
        assert_eq!(response.text, "Custom response");
    }

    #[test]
    fn test_mock_host_functions_streaming() {
        let mock = MockHostFunctions::new();
        let request = LlmRequest {
            messages: vec![Message::user("Hello")],
            tools: vec![],
            stream: true,
        };

        let stream_id = mock.llm_stream_start(&request).unwrap();
        assert_eq!(stream_id, 1);

        let chunk = mock.llm_stream_next(stream_id).unwrap().unwrap();
        assert!(chunk.done);

        assert!(mock.llm_stream_close(stream_id).is_ok());
    }

    #[test]
    fn test_mock_host_functions_memory() {
        let mock = MockHostFunctions::new();

        let results = mock.memory_search("test", 10).unwrap();
        assert!(results.is_empty());

        let id = mock
            .memory_create("content", &serde_json::json!({}))
            .unwrap();
        assert_eq!(id, "mem-001");

        assert!(mock.memory_update("mem-001", "new content").is_ok());
        assert!(mock.memory_delete("mem-001").is_ok());
    }

    #[test]
    fn test_mock_host_functions_skills() {
        let mock = MockHostFunctions::new();
        mock.add_skill(SkillSummary {
            name: "test-skill".into(),
            description: "A test".into(),
            tags: vec![],
        });

        let skills = mock.skill_list().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");

        let content = mock.skill_load("test-skill").unwrap();
        assert!(content.contains("Mock Skill"));
    }

    #[test]
    fn test_mock_host_functions_tools() {
        let mock = MockHostFunctions::new();

        assert!(mock.tool_read("/path", None, None).is_ok());
        assert!(mock.tool_write("/path", "content").is_ok());
        assert!(mock.tool_edit("/path", "old", "new", false).is_ok());
        assert!(mock.tool_bash("ls", None).is_ok());
        assert!(mock.tool_glob("*.rs", None).is_ok());
        assert!(mock.tool_grep("pattern", None, None).is_ok());
        assert!(mock.tool_web_search("query").is_ok());
        assert!(mock.tool_web_fetch("http://example.com", "prompt").is_ok());
        assert!(mock
            .tool_ask_user("Question?", &["A".into(), "B".into()])
            .is_ok());
    }

    #[test]
    fn test_mock_host_functions_permission() {
        let mock = MockHostFunctions::new();

        assert!(mock.permission_check("file", "read", "/home").unwrap());
        assert!(mock.permission_request("file", "write", "/home").unwrap());
    }

    #[test]
    fn test_mock_host_functions_builtin_proxy() {
        let mock = MockHostFunctions::new();
        let input = AgentInput {
            session_id: "sess-001".into(),
            mode: AgentMode::Chat,
            user_message: "Hello".into(),
            history: vec![],
        };

        let chat_output = mock.builtin_chat(&input).unwrap();
        assert!(chat_output.text.contains("Hello"));

        let browser_output = mock.builtin_browser(&input).unwrap();
        assert!(browser_output.text.contains("Hello"));

        let agent_output = mock.builtin_agent(&input).unwrap();
        assert!(agent_output.text.contains("Hello"));
    }
}
