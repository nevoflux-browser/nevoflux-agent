//! ClaudeCodeClient implementation for Claude Code CLI.

use super::completion::ClaudeCodeCompletionModel;

/// Client for interacting with Claude models via the Claude Code CLI.
///
/// The CLI manages its own authentication, so no API key is required.
///
/// # Example
/// ```ignore
/// use nevoflux_llm::providers::claude_code::ClaudeCodeClient;
///
/// let client = ClaudeCodeClient::new("claude");
/// let model = client.completion_model("sonnet");
/// ```
#[derive(Clone)]
pub struct ClaudeCodeClient {
    command: String,
    api_key: Option<String>,
    working_dir: Option<String>,
    add_dirs: Vec<String>,
}

impl ClaudeCodeClient {
    /// Create a new ClaudeCodeClient with the given command path.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            api_key: None,
            working_dir: None,
            add_dirs: Vec::new(),
        }
    }

    /// Set an API key to pass to the CLI via environment variable.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Set the working directory for the CLI subprocess.
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Set additional directories to pass via `--add-dir`.
    pub fn with_add_dirs(mut self, dirs: Vec<String>) -> Self {
        self.add_dirs = dirs;
        self
    }

    /// Get the command path.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Get the API key (for internal use).
    pub(crate) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Get the working directory.
    pub(crate) fn working_dir(&self) -> Option<&str> {
        self.working_dir.as_deref()
    }

    /// Get the additional directories.
    pub(crate) fn add_dirs(&self) -> &[String] {
        &self.add_dirs
    }

    /// Create a completion model for the specified model name.
    ///
    /// # Example
    /// ```ignore
    /// use nevoflux_llm::providers::claude_code::ClaudeCodeClient;
    ///
    /// let client = ClaudeCodeClient::new("claude");
    /// let model = client.completion_model("sonnet");
    /// ```
    pub fn completion_model(&self, model: impl Into<String>) -> ClaudeCodeCompletionModel {
        ClaudeCodeCompletionModel::new(self.clone(), model)
    }
}

impl std::fmt::Debug for ClaudeCodeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeClient")
            .field("command", &self.command)
            .field("api_key", &self.api_key.as_ref().map(|_| "<REDACTED>"))
            .field("working_dir", &self.working_dir)
            .field("add_dirs", &self.add_dirs)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_new() {
        let client = ClaudeCodeClient::new("claude");
        assert_eq!(client.command(), "claude");
    }

    #[test]
    fn test_client_with_api_key() {
        let client = ClaudeCodeClient::new("claude").with_api_key("sk-test");
        assert_eq!(client.api_key(), Some("sk-test"));
    }

    #[test]
    fn test_client_debug_redacts_api_key() {
        let client = ClaudeCodeClient::new("claude").with_api_key("super-secret-key");
        let debug_output = format!("{:?}", client);
        assert!(!debug_output.contains("super-secret-key"));
        assert!(debug_output.contains("<REDACTED>"));
    }

    #[test]
    fn test_client_is_clone() {
        let client = ClaudeCodeClient::new("claude");
        let _cloned = client.clone();
    }

    #[test]
    fn test_completion_model_creation() {
        let client = ClaudeCodeClient::new("claude");
        let model = client.completion_model("sonnet");
        assert_eq!(model.model(), "sonnet");
    }

    #[test]
    fn test_client_no_api_key_by_default() {
        let client = ClaudeCodeClient::new("claude");
        assert!(client.api_key().is_none());
    }

    #[test]
    fn test_client_with_working_dir() {
        let client = ClaudeCodeClient::new("claude").with_working_dir("/tmp/workspace");
        assert_eq!(client.working_dir(), Some("/tmp/workspace"));
    }

    #[test]
    fn test_client_with_add_dirs() {
        let client = ClaudeCodeClient::new("claude").with_add_dirs(vec![
            "/home/user/project".to_string(),
            "/tmp/extra".to_string(),
        ]);
        assert_eq!(client.add_dirs().len(), 2);
        assert_eq!(client.add_dirs()[0], "/home/user/project");
        assert_eq!(client.add_dirs()[1], "/tmp/extra");
    }

    #[test]
    fn test_client_no_working_dir_by_default() {
        let client = ClaudeCodeClient::new("claude");
        assert!(client.working_dir().is_none());
        assert!(client.add_dirs().is_empty());
    }

    #[test]
    fn test_client_debug_includes_working_dir() {
        let client = ClaudeCodeClient::new("claude").with_working_dir("/tmp/test");
        let debug = format!("{:?}", client);
        assert!(debug.contains("/tmp/test"));
    }
}
