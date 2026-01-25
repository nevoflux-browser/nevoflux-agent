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
        assert_eq!(names.len(), 3);
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
