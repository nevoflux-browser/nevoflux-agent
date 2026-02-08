//! GeminiCliClient implementation for Gemini CLI.

use super::completion::GeminiCliCompletionModel;

/// Known Gemini CLI model names.
pub const GEMINI_CLI_KNOWN_MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
];

/// Client for interacting with Gemini models via the Gemini CLI.
///
/// The CLI manages its own authentication, so no API key is required
/// by default. An optional API key can be passed via the
/// `GEMINI_API_KEY` environment variable.
///
/// # Example
/// ```ignore
/// use nevoflux_llm::providers::gemini_cli::GeminiCliClient;
///
/// let client = GeminiCliClient::new("gemini");
/// let model = client.completion_model("gemini-2.5-pro");
/// ```
#[derive(Clone)]
pub struct GeminiCliClient {
    command: String,
    api_key: Option<String>,
    working_dir: Option<String>,
    add_dirs: Vec<String>,
}

impl GeminiCliClient {
    /// Create a new GeminiCliClient with the given command path.
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
    /// use nevoflux_llm::providers::gemini_cli::GeminiCliClient;
    ///
    /// let client = GeminiCliClient::new("gemini");
    /// let model = client.completion_model("gemini-2.5-pro");
    /// ```
    pub fn completion_model(&self, model: impl Into<String>) -> GeminiCliCompletionModel {
        GeminiCliCompletionModel::new(self.clone(), model)
    }
}

impl std::fmt::Debug for GeminiCliClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiCliClient")
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
        let client = GeminiCliClient::new("gemini");
        assert_eq!(client.command(), "gemini");
    }

    #[test]
    fn test_client_with_api_key() {
        let client = GeminiCliClient::new("gemini").with_api_key("test-key");
        assert_eq!(client.api_key(), Some("test-key"));
    }

    #[test]
    fn test_client_debug_redacts_api_key() {
        let client = GeminiCliClient::new("gemini").with_api_key("super-secret-key");
        let debug_output = format!("{:?}", client);
        assert!(!debug_output.contains("super-secret-key"));
        assert!(debug_output.contains("<REDACTED>"));
    }

    #[test]
    fn test_client_is_clone() {
        let client = GeminiCliClient::new("gemini");
        let _cloned = client.clone();
    }

    #[test]
    fn test_completion_model_creation() {
        let client = GeminiCliClient::new("gemini");
        let model = client.completion_model("gemini-2.5-pro");
        assert_eq!(model.model(), "gemini-2.5-pro");
    }

    #[test]
    fn test_client_no_api_key_by_default() {
        let client = GeminiCliClient::new("gemini");
        assert!(client.api_key().is_none());
    }

    #[test]
    fn test_client_with_working_dir() {
        let client = GeminiCliClient::new("gemini").with_working_dir("/tmp/workspace");
        assert_eq!(client.working_dir(), Some("/tmp/workspace"));
    }

    #[test]
    fn test_client_no_working_dir_by_default() {
        let client = GeminiCliClient::new("gemini");
        assert!(client.working_dir().is_none());
        assert!(client.add_dirs().is_empty());
    }

    #[test]
    fn test_client_with_add_dirs() {
        let client = GeminiCliClient::new("gemini").with_add_dirs(vec![
            "/home/user/project".to_string(),
            "/tmp/extra".to_string(),
        ]);
        assert_eq!(client.add_dirs().len(), 2);
        assert_eq!(client.add_dirs()[0], "/home/user/project");
        assert_eq!(client.add_dirs()[1], "/tmp/extra");
    }

    #[test]
    fn test_client_debug_includes_working_dir() {
        let client = GeminiCliClient::new("gemini").with_working_dir("/tmp/test");
        let debug = format!("{:?}", client);
        assert!(debug.contains("/tmp/test"));
    }

    #[test]
    fn test_known_models() {
        assert!(GEMINI_CLI_KNOWN_MODELS.contains(&"gemini-2.5-pro"));
        assert!(GEMINI_CLI_KNOWN_MODELS.contains(&"gemini-2.5-flash"));
        assert!(GEMINI_CLI_KNOWN_MODELS.contains(&"gemini-2.5-flash-lite"));
    }
}
