//! Mock LLM provider for testing.

use std::sync::Arc;

/// A mock LLM provider for testing.
///
/// Allows configuring fixed responses, streaming chunks, and error conditions
/// for testing without making real API calls.
#[derive(Debug, Clone)]
pub struct MockLlmProvider {
    response: MockResponse,
    configured: bool,
}

#[derive(Debug, Clone)]
enum MockResponse {
    Fixed(String),
    Stream(Vec<String>),
    Error(String),
}

impl Default for MockLlmProvider {
    fn default() -> Self {
        Self {
            response: MockResponse::Fixed("Mock response".into()),
            configured: false,
        }
    }
}

impl MockLlmProvider {
    /// Create a mock provider that returns a fixed response.
    pub fn with_fixed_response(text: impl Into<String>) -> Self {
        Self {
            response: MockResponse::Fixed(text.into()),
            configured: true,
        }
    }

    /// Create a mock provider that streams multiple chunks.
    pub fn with_stream_chunks(chunks: Vec<impl Into<String>>) -> Self {
        Self {
            response: MockResponse::Stream(chunks.into_iter().map(Into::into).collect()),
            configured: true,
        }
    }

    /// Create a mock provider that returns an error.
    pub fn with_error(message: impl Into<String>) -> Self {
        Self {
            response: MockResponse::Error(message.into()),
            configured: true,
        }
    }

    /// Check if the mock is configured.
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// Get the configured response text (for fixed responses).
    pub fn response_text(&self) -> Option<&str> {
        match &self.response {
            MockResponse::Fixed(text) => Some(text),
            _ => None,
        }
    }

    /// Get the configured stream chunks.
    pub fn stream_chunks(&self) -> Option<&[String]> {
        match &self.response {
            MockResponse::Stream(chunks) => Some(chunks),
            _ => None,
        }
    }

    /// Get the configured error message.
    pub fn error_message(&self) -> Option<&str> {
        match &self.response {
            MockResponse::Error(msg) => Some(msg),
            _ => None,
        }
    }

    /// Check if this mock will return an error.
    pub fn will_error(&self) -> bool {
        matches!(self.response, MockResponse::Error(_))
    }

    /// Wrap in an Arc for shared use.
    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_llm_provider_default() {
        let mock = MockLlmProvider::default();
        assert!(!mock.is_configured());
    }

    #[test]
    fn test_mock_llm_provider_fixed_response() {
        let mock = MockLlmProvider::with_fixed_response("Hello, world!");

        assert!(mock.is_configured());
        assert_eq!(mock.response_text(), Some("Hello, world!"));
        assert!(!mock.will_error());
    }

    #[test]
    fn test_mock_llm_provider_stream_chunks() {
        let chunks = vec!["Hello", " ", "world", "!"];
        let mock = MockLlmProvider::with_stream_chunks(chunks);

        assert!(mock.is_configured());
        assert_eq!(
            mock.stream_chunks(),
            Some(["Hello", " ", "world", "!"].map(String::from).as_slice())
        );
    }

    #[test]
    fn test_mock_llm_provider_error() {
        let mock = MockLlmProvider::with_error("API rate limit exceeded");

        assert!(mock.is_configured());
        assert!(mock.will_error());
        assert_eq!(mock.error_message(), Some("API rate limit exceeded"));
    }

    #[test]
    fn test_mock_llm_provider_into_arc() {
        let mock = MockLlmProvider::with_fixed_response("Test");
        let arc_mock = mock.into_arc();

        assert!(arc_mock.is_configured());
    }
}
