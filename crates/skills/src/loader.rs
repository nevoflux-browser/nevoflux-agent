//! Skill loader for loading skills from the filesystem.

use crate::error::{Result, SkillsError};
use crate::parser::parse_skill_file;
use crate::types::{Skill, SkillSource};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Configuration for the skill loader.
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// User skills directory.
    pub user_dir: Option<PathBuf>,
    /// Builtin skills directory.
    pub builtin_dir: Option<PathBuf>,
    /// File extension for skill files.
    pub extension: String,
    /// Whether to load disabled skills.
    pub load_disabled: bool,
}

impl Default for LoaderConfig {
    fn default() -> Self {
        Self {
            user_dir: default_user_skills_dir(),
            builtin_dir: None,
            extension: "md".to_string(),
            load_disabled: false,
        }
    }
}

impl LoaderConfig {
    /// Create a new loader config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the user skills directory.
    pub fn with_user_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.user_dir = Some(dir.into());
        self
    }

    /// Set the builtin skills directory.
    pub fn with_builtin_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.builtin_dir = Some(dir.into());
        self
    }

    /// Set the file extension.
    pub fn with_extension(mut self, ext: impl Into<String>) -> Self {
        self.extension = ext.into();
        self
    }

    /// Set whether to load disabled skills.
    pub fn with_load_disabled(mut self, load: bool) -> Self {
        self.load_disabled = load;
        self
    }
}

/// Get the default user skills directory.
pub fn default_user_skills_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("com", "nevoflux", "nevoflux")
        .map(|dirs| dirs.config_dir().join("skills"))
}

/// Skill loader.
pub struct SkillLoader {
    config: LoaderConfig,
}

impl Default for SkillLoader {
    fn default() -> Self {
        Self::new(LoaderConfig::default())
    }
}

impl SkillLoader {
    /// Create a new skill loader.
    pub fn new(config: LoaderConfig) -> Self {
        Self { config }
    }

    /// Get the configuration.
    pub fn config(&self) -> &LoaderConfig {
        &self.config
    }

    /// Load all skills from configured directories.
    pub fn load_all(&self) -> Result<Vec<Skill>> {
        let mut skills = Vec::new();

        // Load builtin skills first
        if let Some(ref dir) = self.config.builtin_dir {
            if dir.exists() {
                let builtin = self.load_from_directory(dir, SkillSource::Builtin)?;
                skills.extend(builtin);
            }
        }

        // Load user skills (can override builtin)
        if let Some(ref dir) = self.config.user_dir {
            if dir.exists() {
                let user = self.load_from_directory(dir, SkillSource::User)?;
                skills.extend(user);
            }
        }

        Ok(skills)
    }

    /// Load skills from a specific directory.
    pub fn load_from_directory(&self, dir: &Path, source: SkillSource) -> Result<Vec<Skill>> {
        if !dir.exists() {
            return Ok(Vec::new());
        }

        if !dir.is_dir() {
            return Err(SkillsError::InvalidSkillFile(format!(
                "Not a directory: {}",
                dir.display()
            )));
        }

        let pattern = dir.join(format!("*.{}", self.config.extension));
        let pattern_str = pattern.to_string_lossy();

        let entries =
            glob::glob(&pattern_str).map_err(|e| SkillsError::GlobError(e.to_string()))?;

        let mut skills = Vec::new();

        for entry in entries {
            match entry {
                Ok(path) => {
                    match parse_skill_file(&path, source.clone()) {
                        Ok(skill) => {
                            // Skip disabled skills unless configured to load them
                            if !skill.is_enabled() && !self.config.load_disabled {
                                debug!("Skipping disabled skill: {}", skill.name());
                                continue;
                            }

                            debug!("Loaded skill: {} from {}", skill.name(), path.display());
                            skills.push(skill);
                        }
                        Err(e) => {
                            warn!("Failed to parse skill file {}: {}", path.display(), e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to read skill file: {}", e);
                }
            }
        }

        Ok(skills)
    }

    /// Load a single skill by name.
    pub fn load_skill(&self, name: &str) -> Result<Skill> {
        // Try user directory first
        if let Some(ref dir) = self.config.user_dir {
            let path = dir.join(format!("{}.{}", name, self.config.extension));
            if path.exists() {
                return parse_skill_file(&path, SkillSource::User);
            }
        }

        // Try builtin directory
        if let Some(ref dir) = self.config.builtin_dir {
            let path = dir.join(format!("{}.{}", name, self.config.extension));
            if path.exists() {
                return parse_skill_file(&path, SkillSource::Builtin);
            }
        }

        Err(SkillsError::NotFound(name.to_string()))
    }

    /// Check if a skill exists.
    pub fn skill_exists(&self, name: &str) -> bool {
        if let Some(ref dir) = self.config.user_dir {
            let path = dir.join(format!("{}.{}", name, self.config.extension));
            if path.exists() {
                return true;
            }
        }

        if let Some(ref dir) = self.config.builtin_dir {
            let path = dir.join(format!("{}.{}", name, self.config.extension));
            if path.exists() {
                return true;
            }
        }

        false
    }

    /// List skill names from configured directories.
    pub fn list_skill_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();

        let dirs: Vec<&PathBuf> = [&self.config.builtin_dir, &self.config.user_dir]
            .into_iter()
            .flatten()
            .collect();

        for dir in dirs {
            if !dir.exists() {
                continue;
            }

            let pattern = dir.join(format!("*.{}", self.config.extension));
            let pattern_str = pattern.to_string_lossy();

            let entries =
                glob::glob(&pattern_str).map_err(|e| SkillsError::GlobError(e.to_string()))?;

            for entry in entries.flatten() {
                if let Some(stem) = entry.file_stem() {
                    let name = stem.to_string_lossy().to_string();
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }

        names.sort();
        Ok(names)
    }
}

/// Async skill loader.
pub struct AsyncSkillLoader {
    config: LoaderConfig,
}

impl AsyncSkillLoader {
    /// Create a new async skill loader.
    pub fn new(config: LoaderConfig) -> Self {
        Self { config }
    }

    /// Load all skills asynchronously.
    pub async fn load_all(&self) -> Result<Vec<Skill>> {
        // For now, use the sync loader in a blocking task
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let loader = SkillLoader::new(config);
            loader.load_all()
        })
        .await
        .map_err(|e| SkillsError::Io(std::io::Error::other(e.to_string())))?
    }

    /// Load a single skill asynchronously.
    pub async fn load_skill(&self, name: &str) -> Result<Skill> {
        let config = self.config.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let loader = SkillLoader::new(config);
            loader.load_skill(&name)
        })
        .await
        .map_err(|e| SkillsError::Io(std::io::Error::other(e.to_string())))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_skill_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(format!("{}.md", name));
        let mut file = std::fs::File::create(path).unwrap();
        write!(file, "{}", content).unwrap();
    }

    fn sample_skill_content(name: &str, description: &str) -> String {
        format!(
            r#"---
name: {}
description: {}
---

# {}

Content for {}.
"#,
            name, description, name, name
        )
    }

    #[test]
    fn test_loader_config_default() {
        let config = LoaderConfig::default();
        assert!(config.user_dir.is_some());
        assert!(config.builtin_dir.is_none());
        assert_eq!(config.extension, "md");
        assert!(!config.load_disabled);
    }

    #[test]
    fn test_loader_config_builder() {
        let config = LoaderConfig::new()
            .with_user_dir("/custom/user")
            .with_builtin_dir("/custom/builtin")
            .with_extension("skill")
            .with_load_disabled(true);

        assert_eq!(config.user_dir, Some(PathBuf::from("/custom/user")));
        assert_eq!(config.builtin_dir, Some(PathBuf::from("/custom/builtin")));
        assert_eq!(config.extension, "skill");
        assert!(config.load_disabled);
    }

    #[test]
    fn test_skill_loader_new() {
        let config = LoaderConfig::new().with_user_dir("/test");
        let loader = SkillLoader::new(config);
        assert_eq!(loader.config().user_dir, Some(PathBuf::from("/test")));
    }

    #[test]
    fn test_skill_loader_load_from_directory() {
        let temp = TempDir::new().unwrap();

        create_skill_file(
            temp.path(),
            "skill1",
            &sample_skill_content("skill1", "First skill"),
        );
        create_skill_file(
            temp.path(),
            "skill2",
            &sample_skill_content("skill2", "Second skill"),
        );

        let config = LoaderConfig::new().with_user_dir(temp.path());
        let loader = SkillLoader::new(config);

        let skills = loader
            .load_from_directory(temp.path(), SkillSource::User)
            .unwrap();
        assert_eq!(skills.len(), 2);

        let names: Vec<_> = skills.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"skill1"));
        assert!(names.contains(&"skill2"));
    }

    #[test]
    fn test_skill_loader_load_all() {
        let temp_user = TempDir::new().unwrap();
        let temp_builtin = TempDir::new().unwrap();

        create_skill_file(
            temp_user.path(),
            "user-skill",
            &sample_skill_content("user-skill", "User skill"),
        );
        create_skill_file(
            temp_builtin.path(),
            "builtin-skill",
            &sample_skill_content("builtin-skill", "Builtin skill"),
        );

        let config = LoaderConfig::new()
            .with_user_dir(temp_user.path())
            .with_builtin_dir(temp_builtin.path());
        let loader = SkillLoader::new(config);

        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 2);

        // Check sources
        let user_skill = skills.iter().find(|s| s.name() == "user-skill").unwrap();
        assert_eq!(user_skill.source, SkillSource::User);

        let builtin_skill = skills.iter().find(|s| s.name() == "builtin-skill").unwrap();
        assert_eq!(builtin_skill.source, SkillSource::Builtin);
    }

    #[test]
    fn test_skill_loader_load_skill() {
        let temp = TempDir::new().unwrap();
        create_skill_file(
            temp.path(),
            "specific",
            &sample_skill_content("specific", "Specific skill"),
        );

        let config = LoaderConfig::new().with_user_dir(temp.path());
        let loader = SkillLoader::new(config);

        let skill = loader.load_skill("specific").unwrap();
        assert_eq!(skill.name(), "specific");
    }

    #[test]
    fn test_skill_loader_load_skill_not_found() {
        let temp = TempDir::new().unwrap();
        let config = LoaderConfig::new().with_user_dir(temp.path());
        let loader = SkillLoader::new(config);

        let result = loader.load_skill("nonexistent");
        assert!(matches!(result, Err(SkillsError::NotFound(_))));
    }

    #[test]
    fn test_skill_loader_skill_exists() {
        let temp = TempDir::new().unwrap();
        create_skill_file(temp.path(), "exists", "---\nname: exists\n---\nContent");

        let config = LoaderConfig::new().with_user_dir(temp.path());
        let loader = SkillLoader::new(config);

        assert!(loader.skill_exists("exists"));
        assert!(!loader.skill_exists("does-not-exist"));
    }

    #[test]
    fn test_skill_loader_list_skill_names() {
        let temp = TempDir::new().unwrap();
        create_skill_file(temp.path(), "alpha", "---\nname: alpha\n---\n");
        create_skill_file(temp.path(), "beta", "---\nname: beta\n---\n");
        create_skill_file(temp.path(), "gamma", "---\nname: gamma\n---\n");

        let config = LoaderConfig::new().with_user_dir(temp.path());
        let loader = SkillLoader::new(config);

        let names = loader.list_skill_names().unwrap();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_skill_loader_skip_disabled() {
        let temp = TempDir::new().unwrap();
        create_skill_file(
            temp.path(),
            "enabled",
            r#"---
name: enabled
enabled: true
---
Enabled content."#,
        );
        create_skill_file(
            temp.path(),
            "disabled",
            r#"---
name: disabled
enabled: false
---
Disabled content."#,
        );

        let config = LoaderConfig::new()
            .with_user_dir(temp.path())
            .with_load_disabled(false);
        let loader = SkillLoader::new(config);

        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name(), "enabled");
    }

    #[test]
    fn test_skill_loader_load_disabled() {
        let temp = TempDir::new().unwrap();
        create_skill_file(
            temp.path(),
            "disabled",
            r#"---
name: disabled
enabled: false
---
Disabled content."#,
        );

        let config = LoaderConfig::new()
            .with_user_dir(temp.path())
            .with_load_disabled(true);
        let loader = SkillLoader::new(config);

        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);
        assert!(!skills[0].is_enabled());
    }

    #[test]
    fn test_skill_loader_empty_directory() {
        let temp = TempDir::new().unwrap();
        let config = LoaderConfig::new().with_user_dir(temp.path());
        let loader = SkillLoader::new(config);

        let skills = loader.load_all().unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_skill_loader_nonexistent_directory() {
        let config = LoaderConfig::new().with_user_dir("/nonexistent/path");
        let loader = SkillLoader::new(config);

        let skills = loader.load_all().unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_skill_loader_user_overrides_builtin() {
        let temp_user = TempDir::new().unwrap();
        let temp_builtin = TempDir::new().unwrap();

        // Same name in both directories
        create_skill_file(
            temp_builtin.path(),
            "shared",
            &sample_skill_content("shared", "Builtin version"),
        );
        create_skill_file(
            temp_user.path(),
            "shared",
            &sample_skill_content("shared", "User version"),
        );

        let config = LoaderConfig::new()
            .with_user_dir(temp_user.path())
            .with_builtin_dir(temp_builtin.path());
        let loader = SkillLoader::new(config);

        // When loading by name, user takes precedence
        let skill = loader.load_skill("shared").unwrap();
        assert_eq!(skill.source, SkillSource::User);
        assert!(skill.description().contains("User version"));
    }

    #[tokio::test]
    async fn test_async_skill_loader_load_all() {
        let temp = TempDir::new().unwrap();
        create_skill_file(
            temp.path(),
            "async-skill",
            &sample_skill_content("async-skill", "Async loaded"),
        );

        let config = LoaderConfig::new().with_user_dir(temp.path());
        let loader = AsyncSkillLoader::new(config);

        let skills = loader.load_all().await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name(), "async-skill");
    }

    #[tokio::test]
    async fn test_async_skill_loader_load_skill() {
        let temp = TempDir::new().unwrap();
        create_skill_file(
            temp.path(),
            "async-single",
            &sample_skill_content("async-single", "Single async"),
        );

        let config = LoaderConfig::new().with_user_dir(temp.path());
        let loader = AsyncSkillLoader::new(config);

        let skill = loader.load_skill("async-single").await.unwrap();
        assert_eq!(skill.name(), "async-single");
    }

    #[test]
    fn test_default_user_skills_dir() {
        let dir = default_user_skills_dir();
        // Should return Some on most systems
        if let Some(path) = dir {
            assert!(path.to_string_lossy().contains("skills"));
        }
    }
}
