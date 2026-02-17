use std::path::{Path, PathBuf};

use chrono::Utc;

use super::templates;
use crate::error::Result;

/// Cached in-memory representation of the five soul documents.
pub struct FiveDocCache {
    pub identity_raw: String,
    pub soul_raw: String,
    pub user_raw: String,
    pub tools_raw: String,
    pub agents_raw: String,
    pub last_parsed_at: chrono::DateTime<Utc>,
}

/// Manages the soul directory and its five core documents.
///
/// Responsible for initializing a new soul directory with default templates,
/// loading existing documents into an in-memory cache, and providing
/// access to the cached content.
pub struct SoulManager {
    soul_dir: PathBuf,
    cache: FiveDocCache,
}

impl SoulManager {
    /// Initialize a new soul directory with default templates.
    ///
    /// Creates the directory structure and writes default content for any
    /// documents that do not already exist. After writing, loads all
    /// documents into the cache.
    pub async fn init(soul_dir: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(soul_dir).await?;
        tokio::fs::create_dir_all(soul_dir.join(".changelog")).await?;
        tokio::fs::create_dir_all(soul_dir.join(".snapshots")).await?;

        let files = [
            ("IDENTITY.md", templates::default_identity()),
            ("SOUL.md", templates::default_soul()),
            ("USER.md", templates::default_user()),
            ("TOOLS.md", templates::default_tools()),
            ("AGENTS.md", templates::default_agents()),
        ];

        for (name, content) in &files {
            let path = soul_dir.join(name);
            match tokio::fs::metadata(&path).await {
                Ok(_) => { /* file exists, skip */ }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tokio::fs::write(&path, content).await?;
                }
                Err(e) => return Err(e.into()),
            }
        }

        Self::load(soul_dir).await
    }

    /// Load existing soul directory into cache.
    ///
    /// Reads all five documents from disk and stores their content
    /// in the in-memory cache.
    pub async fn load(soul_dir: &Path) -> Result<Self> {
        let identity_raw = tokio::fs::read_to_string(soul_dir.join("IDENTITY.md")).await?;
        let soul_raw = tokio::fs::read_to_string(soul_dir.join("SOUL.md")).await?;
        let user_raw = tokio::fs::read_to_string(soul_dir.join("USER.md")).await?;
        let tools_raw = tokio::fs::read_to_string(soul_dir.join("TOOLS.md")).await?;
        let agents_raw = tokio::fs::read_to_string(soul_dir.join("AGENTS.md")).await?;

        let cache = FiveDocCache {
            identity_raw,
            soul_raw,
            user_raw,
            tools_raw,
            agents_raw,
            last_parsed_at: Utc::now(),
        };

        Ok(Self {
            soul_dir: soul_dir.to_path_buf(),
            cache,
        })
    }

    /// Returns a reference to the cached five-document content.
    pub fn cache(&self) -> &FiveDocCache {
        &self.cache
    }

    /// Returns the path to the soul directory.
    pub fn soul_dir(&self) -> &Path {
        &self.soul_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn manager_initializes_directory_with_defaults() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");

        let manager = SoulManager::init(&soul_dir).await.unwrap();

        assert!(soul_dir.join("IDENTITY.md").exists());
        assert!(soul_dir.join("SOUL.md").exists());
        assert!(soul_dir.join("USER.md").exists());
        assert!(soul_dir.join("TOOLS.md").exists());
        assert!(soul_dir.join("AGENTS.md").exists());
        assert!(soul_dir.join(".changelog").is_dir());
        assert!(soul_dir.join(".snapshots").is_dir());

        // Verify the manager's soul_dir is set correctly
        assert_eq!(manager.soul_dir(), soul_dir);
    }

    #[tokio::test]
    async fn manager_loads_existing_files() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");

        // Initialize first
        let _manager = SoulManager::init(&soul_dir).await.unwrap();

        // Load again
        let manager = SoulManager::load(&soul_dir).await.unwrap();
        assert!(manager.cache().identity_raw.contains("NevoFlux Identity"));
        assert!(manager.cache().soul_raw.contains("Safety Boundaries"));
    }

    #[tokio::test]
    async fn init_does_not_overwrite_existing_files() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");

        // Initialize first
        let _manager = SoulManager::init(&soul_dir).await.unwrap();

        // Modify a file
        let custom_content = "# Custom Identity\n\nMy custom identity.";
        tokio::fs::write(soul_dir.join("IDENTITY.md"), custom_content)
            .await
            .unwrap();

        // Re-initialize — should not overwrite
        let manager = SoulManager::init(&soul_dir).await.unwrap();
        assert_eq!(manager.cache().identity_raw, custom_content);
    }

    #[tokio::test]
    async fn load_fails_on_missing_directory() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("nonexistent");

        let result = SoulManager::load(&soul_dir).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cache_contains_all_template_content() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");

        let manager = SoulManager::init(&soul_dir).await.unwrap();

        // Verify each cached document contains expected content
        assert!(manager.cache().identity_raw.contains("NevoFlux Identity"));
        assert!(manager.cache().soul_raw.contains("Core Values"));
        assert!(manager.cache().user_raw.contains("NevoFlux User Profile"));
        assert!(manager.cache().tools_raw.contains("NevoFlux Tools"));
        assert!(manager.cache().agents_raw.contains("NevoFlux Agents"));
    }
}
