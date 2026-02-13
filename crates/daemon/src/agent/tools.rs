//! Tool execution infrastructure for the agent.
//!
//! This module provides the tool registry and built-in tools for the agent.
//! Tools are executed by the agent runner when the Wasm agent requests them.

use crate::agent::abi::{PendingToolCall, ToolResult};
use crate::error::{DaemonError, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;

// ============================================================================
// Tool Executor Trait
// ============================================================================

/// Trait for tool executors.
///
/// Implementations of this trait provide the actual execution logic for tools.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute the tool with the given arguments.
    ///
    /// # Arguments
    /// * `name` - The name of the tool being executed
    /// * `arguments` - The arguments passed to the tool as JSON
    ///
    /// # Returns
    /// The result of the tool execution as a string, or an error.
    async fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<String>;
}

// ============================================================================
// Tool Registry
// ============================================================================

/// Registry for tool executors.
///
/// The registry maps tool names to their executor implementations.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn ToolExecutor>>,
}

impl ToolRegistry {
    /// Create a new tool registry with built-in tools registered.
    pub fn new() -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
        };

        // Register built-in tools
        registry.register("read_file", Box::new(ReadFileTool));
        registry.register("write_file", Box::new(WriteFileTool));
        registry.register("list_files", Box::new(ListFilesTool));

        registry
    }

    /// Create an empty tool registry without built-in tools.
    pub fn empty() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool executor.
    ///
    /// # Arguments
    /// * `name` - The name of the tool
    /// * `executor` - The executor implementation
    pub fn register(&mut self, name: &str, executor: Box<dyn ToolExecutor>) {
        self.tools.insert(name.to_string(), executor);
    }

    /// Check if a tool is registered.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Get the list of registered tool names.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Get sorted tool names for deterministic output.
    fn tool_names_sorted(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.tools.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Get parameter hints and docs for known built-in tools.
    /// Returns (params, description) based on tool name conventions.
    fn tool_signature_hint(name: &str) -> (&'static str, &'static str) {
        match name {
            "read_file" => (
                "path: str",
                "Read the contents of a file. Returns the file content as a string.",
            ),
            "write_file" => (
                "path: str, content: str",
                "Write content to a file. Returns success message.",
            ),
            "list_files" => (
                "path: str",
                "List directory contents. Returns newline-separated entries.",
            ),
            "web_search" => ("query: str", "Search the web. Returns search results."),
            "fetch_page" => ("url: str", "Fetch a web page. Returns the page content."),
            "canvas_render" => (
                "files: dict, entry: str",
                "Render a multi-file project in the canvas.",
            ),
            "get_code_mode_context" => (
                "",
                "Get Code Mode context with available tool function signatures.",
            ),
            _ => ("**kwargs", "Execute this tool with keyword arguments."),
        }
    }

    /// Generate Python function signatures for all registered tools.
    /// Since ToolExecutor doesn't expose schemas, this generates stubs based on tool names only.
    /// Used by get_code_mode_context() for on-demand tool discovery.
    pub fn to_python_stubs(&self) -> String {
        let mut stubs =
            String::from("# Available tool functions (pre-injected, no imports needed)\n\n");

        for name in self.tool_names_sorted() {
            let (params, doc) = Self::tool_signature_hint(name);
            stubs.push_str(&format!("def {}({}):\n", name, params));
            stubs.push_str(&format!("    \"\"\"{}\"\"\"\n", doc));
            stubs.push_str("    ...\n\n");
        }

        stubs
    }

    /// Generate compact tool category summary for system prompt (~200 tokens).
    pub fn tool_categories_summary(&self) -> String {
        let mut categories: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();

        for name in self.tool_names_sorted() {
            let category = if name.starts_with("web_") || name.starts_with("fetch_") {
                "Search & Web"
            } else if name.starts_with("read_")
                || name.starts_with("write_")
                || name.starts_with("list_")
            {
                "Files"
            } else if name.starts_with("canvas_") || name.starts_with("browser_") {
                "Browser & Canvas"
            } else {
                "Other"
            };
            categories.entry(category).or_default().push(name);
        }

        let mut summary = String::from("Available tool categories:\n");
        for (category, tools) in &categories {
            summary.push_str(&format!("- {}: {}\n", category, tools.join(", ")));
        }
        summary
    }

    /// Generate the Code Mode system prompt with Monty constraints and tool info.
    pub fn code_mode_system_prompt(&self) -> String {
        let mut prompt = String::new();
        prompt.push_str("You are in Code Mode. Write a Python script to accomplish the task.\n\n");
        prompt.push_str("IMPORTANT: Use ONLY these supported constructs:\n");
        prompt.push_str("- Statements: variable, def, if/elif/else, for/while, break, continue, try/except/finally, return, pass, del, assert, raise\n");
        prompt.push_str("- Expressions: arithmetic, comparison, boolean, f-string, lambda, comprehensions, ternary, slice, unpack, walrus (:=)\n");
        prompt.push_str("- Types: int, float, str, bool, list, dict, set, tuple, None, bytes\n");
        prompt.push_str("- Built-ins: len, range, sorted, enumerate, zip, map, filter, sum, min, max, abs, round, isinstance, type, print\n\n");
        prompt.push_str(
            "DO NOT use: class, match/case, import, with, async/await, yield, decorators\n\n",
        );
        prompt.push_str("Pattern corrections:\n");
        prompt.push_str(
            "- Instead of class: use dict + factory function: def make_item(x): return {\"x\": x}\n",
        );
        prompt.push_str("- Instead of match: use if/elif/else\n");
        prompt.push_str("- Instead of import: tools are pre-injected as functions\n");
        prompt.push_str("- Instead of with: use try/finally or call tool directly\n\n");
        prompt.push_str(&self.tool_categories_summary());
        prompt.push_str("\nCall get_code_mode_context() to see full function signatures.\n");
        prompt.push_str("\nReturn only the Python code in a ```python block.\n");
        prompt
    }

    /// Register the get_code_mode_context tool that returns Python stubs.
    /// Call this after all other tools are registered.
    pub fn register_code_mode_context_tool(&mut self) {
        let stubs = self.to_python_stubs();
        self.register(
            "get_code_mode_context",
            Box::new(GetCodeModeContextTool::new(stubs)),
        );
    }

    /// Execute a tool call and return the result.
    ///
    /// # Arguments
    /// * `call` - The pending tool call to execute
    ///
    /// # Returns
    /// A `ToolResult` containing the execution result or error.
    pub async fn execute(&self, call: &PendingToolCall) -> ToolResult {
        match self.tools.get(&call.name) {
            Some(executor) => match executor.execute(&call.name, &call.arguments).await {
                Ok(content) => ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: Some(content),
                    error: None,
                },
                Err(e) => ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: None,
                    error: Some(e.to_string()),
                },
            },
            None => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content: None,
                error: Some(format!("Unknown tool: {}", call.name)),
            },
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Built-in Tools
// ============================================================================

/// Tool for reading file contents.
pub struct ReadFileTool;

#[async_trait]
impl ToolExecutor for ReadFileTool {
    async fn execute(&self, _name: &str, arguments: &serde_json::Value) -> Result<String> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError::InternalError("Missing 'path' argument".to_string()))?;

        let content = tokio::fs::read_to_string(path).await.map_err(|e| {
            DaemonError::IoError(std::io::Error::new(
                e.kind(),
                format!("Failed to read file '{}': {}", path, e),
            ))
        })?;

        Ok(content)
    }
}

/// Tool for writing content to files.
pub struct WriteFileTool;

#[async_trait]
impl ToolExecutor for WriteFileTool {
    async fn execute(&self, _name: &str, arguments: &serde_json::Value) -> Result<String> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError::InternalError("Missing 'path' argument".to_string()))?;

        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError::InternalError("Missing 'content' argument".to_string()))?;

        tokio::fs::write(path, content).await.map_err(|e| {
            DaemonError::IoError(std::io::Error::new(
                e.kind(),
                format!("Failed to write file '{}': {}", path, e),
            ))
        })?;

        Ok(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path
        ))
    }
}

/// Tool for listing directory contents.
pub struct ListFilesTool;

#[async_trait]
impl ToolExecutor for ListFilesTool {
    async fn execute(&self, _name: &str, arguments: &serde_json::Value) -> Result<String> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError::InternalError("Missing 'path' argument".to_string()))?;

        let path = Path::new(path);
        if !path.is_dir() {
            return Err(DaemonError::InternalError(format!(
                "Path '{}' is not a directory",
                path.display()
            )));
        }

        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(path).await.map_err(|e| {
            DaemonError::IoError(std::io::Error::new(
                e.kind(),
                format!("Failed to read directory '{}': {}", path.display(), e),
            ))
        })?;

        while let Some(entry) = dir.next_entry().await.map_err(|e| {
            DaemonError::IoError(std::io::Error::new(
                e.kind(),
                format!("Failed to read directory entry: {}", e),
            ))
        })? {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let file_type = entry.file_type().await.ok();
            let type_indicator = if file_type.map(|t| t.is_dir()).unwrap_or(false) {
                "/"
            } else {
                ""
            };
            entries.push(format!("{}{}", file_name, type_indicator));
        }

        entries.sort();
        Ok(entries.join("\n"))
    }
}

// ============================================================================
// Code Mode Context Tool
// ============================================================================

/// Tool that returns Code Mode context (Python function signatures for available tools).
pub struct GetCodeModeContextTool {
    stubs: String,
}

impl GetCodeModeContextTool {
    /// Create a new Code Mode context tool with pre-generated Python stubs.
    pub fn new(stubs: String) -> Self {
        Self { stubs }
    }
}

#[async_trait]
impl ToolExecutor for GetCodeModeContextTool {
    async fn execute(&self, _name: &str, _arguments: &serde_json::Value) -> Result<String> {
        Ok(self.stubs.clone())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_registry_creation() {
        let registry = ToolRegistry::new();

        // Check that built-in tools are registered
        assert!(registry.has_tool("read_file"));
        assert!(registry.has_tool("write_file"));
        assert!(registry.has_tool("list_files"));

        // Check that unknown tools are not registered
        assert!(!registry.has_tool("unknown_tool"));
    }

    #[test]
    fn test_registry_tool_names() {
        let registry = ToolRegistry::new();
        let names = registry.tool_names();

        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"list_files"));
        // 3 built-in tools: read_file, write_file, list_files
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn test_python_stubs_generation() {
        let registry = ToolRegistry::new();
        let stubs = registry.to_python_stubs();
        assert!(stubs.contains("def list_files("));
        assert!(stubs.contains("def read_file("));
        assert!(stubs.contains("def write_file("));
        assert!(stubs.contains("# Available tool functions"));
    }

    #[test]
    fn test_tool_categories_summary() {
        let registry = ToolRegistry::new();
        let summary = registry.tool_categories_summary();
        assert!(summary.contains("Files"));
        assert!(summary.contains("read_file"));
    }

    #[test]
    fn test_code_mode_system_prompt() {
        let registry = ToolRegistry::new();
        let prompt = registry.code_mode_system_prompt();
        assert!(prompt.contains("Code Mode"));
        assert!(prompt.contains("DO NOT use"));
        assert!(prompt.contains("class"));
        assert!(prompt.contains("```python"));
    }

    #[test]
    fn test_register_code_mode_context_tool() {
        let mut registry = ToolRegistry::new();
        assert!(!registry.has_tool("get_code_mode_context"));

        registry.register_code_mode_context_tool();
        assert!(registry.has_tool("get_code_mode_context"));
        assert_eq!(registry.tool_names().len(), 4);
    }

    #[tokio::test]
    async fn test_code_mode_context_tool_execution() {
        let mut registry = ToolRegistry::new();
        registry.register_code_mode_context_tool();

        let call = PendingToolCall {
            id: "call-ctx".to_string(),
            name: "get_code_mode_context".to_string(),
            arguments: serde_json::json!({}),
        };

        let result = registry.execute(&call).await;
        assert!(result.error.is_none());
        let content = result.content.unwrap();
        assert!(content.contains("def read_file("));
        assert!(content.contains("def write_file("));
        assert!(content.contains("# Available tool functions"));
    }

    #[test]
    fn test_empty_registry() {
        let registry = ToolRegistry::empty();
        assert!(!registry.has_tool("read_file"));
        assert!(registry.tool_names().is_empty());
    }

    #[test]
    fn test_register_custom_tool() {
        struct CustomTool;

        #[async_trait]
        impl ToolExecutor for CustomTool {
            async fn execute(&self, _name: &str, _arguments: &serde_json::Value) -> Result<String> {
                Ok("custom result".to_string())
            }
        }

        let mut registry = ToolRegistry::empty();
        registry.register("custom", Box::new(CustomTool));

        assert!(registry.has_tool("custom"));
    }

    #[tokio::test]
    async fn test_unknown_tool_handling() {
        let registry = ToolRegistry::new();
        let call = PendingToolCall {
            id: "call-001".to_string(),
            name: "nonexistent_tool".to_string(),
            arguments: serde_json::json!({}),
        };

        let result = registry.execute(&call).await;

        assert_eq!(result.call_id, "call-001");
        assert_eq!(result.name, "nonexistent_tool");
        assert!(result.content.is_none());
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("Unknown tool"));
    }

    #[tokio::test]
    async fn test_list_files_execution() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Create some test files
        std::fs::write(dir_path.join("file1.txt"), "content1").unwrap();
        std::fs::write(dir_path.join("file2.txt"), "content2").unwrap();
        std::fs::create_dir(dir_path.join("subdir")).unwrap();

        let registry = ToolRegistry::new();
        let call = PendingToolCall {
            id: "call-002".to_string(),
            name: "list_files".to_string(),
            arguments: serde_json::json!({
                "path": dir_path.to_str().unwrap()
            }),
        };

        let result = registry.execute(&call).await;

        assert_eq!(result.call_id, "call-002");
        assert_eq!(result.name, "list_files");
        assert!(result.error.is_none());
        assert!(result.content.is_some());

        let content = result.content.unwrap();
        assert!(content.contains("file1.txt"));
        assert!(content.contains("file2.txt"));
        assert!(content.contains("subdir/"));
    }

    #[tokio::test]
    async fn test_read_file_execution() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "Hello, World!").unwrap();

        let registry = ToolRegistry::new();
        let call = PendingToolCall {
            id: "call-003".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({
                "path": file_path.to_str().unwrap()
            }),
        };

        let result = registry.execute(&call).await;

        assert!(result.error.is_none());
        assert_eq!(result.content, Some("Hello, World!".to_string()));
    }

    #[tokio::test]
    async fn test_read_file_missing_path() {
        let registry = ToolRegistry::new();
        let call = PendingToolCall {
            id: "call-004".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({}),
        };

        let result = registry.execute(&call).await;

        assert!(result.content.is_none());
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("Missing 'path' argument"));
    }

    #[tokio::test]
    async fn test_read_file_nonexistent() {
        let registry = ToolRegistry::new();
        let call = PendingToolCall {
            id: "call-005".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({
                "path": "/nonexistent/path/to/file.txt"
            }),
        };

        let result = registry.execute(&call).await;

        assert!(result.content.is_none());
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_write_file_execution() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("output.txt");

        let registry = ToolRegistry::new();
        let call = PendingToolCall {
            id: "call-006".to_string(),
            name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": file_path.to_str().unwrap(),
                "content": "Test content"
            }),
        };

        let result = registry.execute(&call).await;

        assert!(result.error.is_none());
        assert!(result.content.is_some());
        assert!(result.content.unwrap().contains("Successfully wrote"));

        // Verify file was written
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "Test content");
    }

    #[tokio::test]
    async fn test_write_file_missing_content() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("output.txt");

        let registry = ToolRegistry::new();
        let call = PendingToolCall {
            id: "call-007".to_string(),
            name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": file_path.to_str().unwrap()
            }),
        };

        let result = registry.execute(&call).await;

        assert!(result.content.is_none());
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("Missing 'content' argument"));
    }

    #[tokio::test]
    async fn test_list_files_not_directory() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("file.txt");
        std::fs::write(&file_path, "content").unwrap();

        let registry = ToolRegistry::new();
        let call = PendingToolCall {
            id: "call-008".to_string(),
            name: "list_files".to_string(),
            arguments: serde_json::json!({
                "path": file_path.to_str().unwrap()
            }),
        };

        let result = registry.execute(&call).await;

        assert!(result.content.is_none());
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("is not a directory"));
    }

    #[test]
    fn test_registry_default() {
        let registry = ToolRegistry::default();
        assert!(registry.has_tool("read_file"));
        assert!(registry.has_tool("write_file"));
        assert!(registry.has_tool("list_files"));
    }
}
