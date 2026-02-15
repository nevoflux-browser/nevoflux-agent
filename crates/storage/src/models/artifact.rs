//! Artifact data model.

use std::collections::HashMap;

/// A persisted artifact record.
#[derive(Debug, Clone)]
pub struct ArtifactRecord {
    pub id: String,
    pub session_id: String,
    pub title: String,
    pub description: Option<String>,
    pub content_type: String,
    pub content: String,
    pub files: Option<HashMap<String, String>>,
    pub entry: Option<String>,
    pub created_at: i64,
}

/// Parameters for creating/upserting an artifact.
pub struct CreateArtifactParams {
    pub id: String,
    pub session_id: String,
    pub title: String,
    pub description: Option<String>,
    pub content_type: String,
    pub content: String,
    pub files: Option<HashMap<String, String>>,
    pub entry: Option<String>,
}

impl CreateArtifactParams {
    /// Create new params with required fields.
    pub fn new(id: &str, session_id: &str, title: &str, content_type: &str) -> Self {
        Self {
            id: id.to_string(),
            session_id: session_id.to_string(),
            title: title.to_string(),
            description: None,
            content_type: content_type.to_string(),
            content: String::new(),
            files: None,
            entry: None,
        }
    }

    /// Set the description.
    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Set the content.
    pub fn with_content(mut self, content: &str) -> Self {
        self.content = content.to_string();
        self
    }

    /// Set the files map.
    pub fn with_files(mut self, files: HashMap<String, String>) -> Self {
        self.files = Some(files);
        self
    }

    /// Set the entry point.
    pub fn with_entry(mut self, entry: &str) -> Self {
        self.entry = Some(entry.to_string());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_artifact_params_builder() {
        let params = CreateArtifactParams::new("art-1", "sess-1", "My Page", "text/html")
            .with_description("A simple HTML page")
            .with_content("<h1>Hello</h1>");

        assert_eq!(params.id, "art-1");
        assert_eq!(params.session_id, "sess-1");
        assert_eq!(params.title, "My Page");
        assert_eq!(params.description, Some("A simple HTML page".to_string()));
        assert_eq!(params.content_type, "text/html");
        assert_eq!(params.content, "<h1>Hello</h1>");
        assert!(params.files.is_none());
        assert!(params.entry.is_none());
    }

    #[test]
    fn test_create_artifact_params_with_files() {
        let mut files = HashMap::new();
        files.insert("src/App.jsx".to_string(), "export default App;".to_string());

        let params = CreateArtifactParams::new("art-2", "sess-1", "React App", "project")
            .with_files(files)
            .with_entry("src/App.jsx");

        assert!(params.files.is_some());
        assert_eq!(params.entry, Some("src/App.jsx".to_string()));
    }
}
