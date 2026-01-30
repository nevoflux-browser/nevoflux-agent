//! Skill loader for loading skills from the filesystem.

use crate::error::{Result, SkillsError};
use crate::parser::parse_skill_file;
use crate::types::{Skill, SkillSource};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Configuration for the skill loader.
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// User skills directories (multiple sources supported).
    pub user_dirs: Vec<PathBuf>,
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
            user_dirs: default_user_skills_dirs(),
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

    /// Set a single user skills directory (replaces all existing).
    pub fn with_user_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.user_dirs = vec![dir.into()];
        self
    }

    /// Add a user skills directory.
    pub fn with_additional_user_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.user_dirs.push(dir.into());
        self
    }

    /// Set multiple user skills directories (replaces all existing).
    pub fn with_user_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.user_dirs = dirs;
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

    /// For backward compatibility: get the first user directory.
    #[deprecated(note = "Use user_dirs instead")]
    pub fn user_dir(&self) -> Option<&PathBuf> {
        self.user_dirs.first()
    }
}

/// Get all default user skills directories.
///
/// Searches multiple common locations for skills:
/// - `~/.config/nevoflux/skills/` (NevoFlux native)
/// - `~/.claude/skills/` (Claude Code compatible)
/// - `~/.gemini/skills/` (Gemini compatible)
/// - `~/.config/opencode/skills/` (OpenCode compatible)
/// - `~/.config/goose/skills/` (Goose compatible)
pub fn default_user_skills_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // NevoFlux native directory
    if let Some(project_dirs) = directories::ProjectDirs::from("com", "nevoflux", "nevoflux") {
        dirs.push(project_dirs.config_dir().join("skills"));
    }

    // Home directory based paths
    if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) {
        // Claude Code compatible
        dirs.push(home.join(".claude").join("skills"));

        // Gemini compatible
        dirs.push(home.join(".gemini").join("skills"));

        // OpenCode compatible
        dirs.push(home.join(".config").join("opencode").join("skills"));

        // Goose compatible
        dirs.push(home.join(".config").join("goose").join("skills"));
    }

    dirs
}

/// Get the default user skills directory (legacy, returns first directory).
#[deprecated(note = "Use default_user_skills_dirs() instead")]
pub fn default_user_skills_dir() -> Option<PathBuf> {
    default_user_skills_dirs().into_iter().next()
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
    ///
    /// Skills are loaded in order: builtin first, then user directories.
    /// Later directories override earlier ones (same name = replace).
    pub fn load_all(&self) -> Result<Vec<Skill>> {
        use std::collections::HashMap;

        let mut skills_map: HashMap<String, Skill> = HashMap::new();

        // Load builtin skills first
        if let Some(ref dir) = self.config.builtin_dir {
            if dir.exists() {
                for skill in self.load_from_directory(dir, SkillSource::Builtin)? {
                    skills_map.insert(skill.name().to_string(), skill);
                }
            }
        }

        // Load user skills from all configured directories (can override builtin)
        // Later directories take precedence over earlier ones
        for dir in &self.config.user_dirs {
            if dir.exists() {
                for skill in self.load_from_directory(dir, SkillSource::User)? {
                    skills_map.insert(skill.name().to_string(), skill);
                }
            }
        }

        Ok(skills_map.into_values().collect())
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
    ///
    /// Searches user directories first (in reverse order, so later directories
    /// take precedence), then falls back to builtin directory.
    pub fn load_skill(&self, name: &str) -> Result<Skill> {
        // Try user directories in reverse order (later dirs have higher priority)
        for dir in self.config.user_dirs.iter().rev() {
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
        // Check user directories
        for dir in &self.config.user_dirs {
            let path = dir.join(format!("{}.{}", name, self.config.extension));
            if path.exists() {
                return true;
            }
        }

        // Check builtin directory
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

        // Collect all directories to search
        let mut dirs: Vec<&PathBuf> = Vec::new();
        if let Some(ref builtin) = self.config.builtin_dir {
            dirs.push(builtin);
        }
        dirs.extend(self.config.user_dirs.iter());

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
        assert!(!config.user_dirs.is_empty());
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

        assert_eq!(config.user_dirs, vec![PathBuf::from("/custom/user")]);
        assert_eq!(config.builtin_dir, Some(PathBuf::from("/custom/builtin")));
        assert_eq!(config.extension, "skill");
        assert!(config.load_disabled);
    }

    #[test]
    fn test_loader_config_multiple_user_dirs() {
        let config = LoaderConfig::new()
            .with_user_dirs(vec![PathBuf::from("/dir1"), PathBuf::from("/dir2")])
            .with_additional_user_dir("/dir3");

        assert_eq!(config.user_dirs.len(), 3);
        assert_eq!(config.user_dirs[0], PathBuf::from("/dir1"));
        assert_eq!(config.user_dirs[1], PathBuf::from("/dir2"));
        assert_eq!(config.user_dirs[2], PathBuf::from("/dir3"));
    }

    #[test]
    fn test_skill_loader_new() {
        let config = LoaderConfig::new().with_user_dir("/test");
        let loader = SkillLoader::new(config);
        assert_eq!(loader.config().user_dirs, vec![PathBuf::from("/test")]);
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
    fn test_default_user_skills_dirs() {
        let dirs = default_user_skills_dirs();
        // Should return multiple directories on most systems
        assert!(!dirs.is_empty());

        // All paths should contain "skills"
        for dir in &dirs {
            assert!(dir.to_string_lossy().contains("skills"));
        }

        // At least check that we have multiple directories (claude, gemini, opencode, goose)
        assert!(
            dirs.len() >= 4,
            "Expected at least 4 directories, got {}",
            dirs.len()
        );
    }

    #[test]
    fn test_load_from_multiple_user_dirs() {
        let temp1 = TempDir::new().unwrap();
        let temp2 = TempDir::new().unwrap();

        // Create skill in first directory
        create_skill_file(
            temp1.path(),
            "skill-from-dir1",
            &sample_skill_content("skill-from-dir1", "From directory 1"),
        );

        // Create skill in second directory
        create_skill_file(
            temp2.path(),
            "skill-from-dir2",
            &sample_skill_content("skill-from-dir2", "From directory 2"),
        );

        // Create skill with same name in both (dir2 should win)
        create_skill_file(
            temp1.path(),
            "shared-skill",
            &sample_skill_content("shared-skill", "Version from dir1"),
        );
        create_skill_file(
            temp2.path(),
            "shared-skill",
            &sample_skill_content("shared-skill", "Version from dir2"),
        );

        let config = LoaderConfig::new()
            .with_user_dirs(vec![temp1.path().to_path_buf(), temp2.path().to_path_buf()]);
        let loader = SkillLoader::new(config);

        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 3); // skill-from-dir1, skill-from-dir2, shared-skill

        // shared-skill should be from dir2 (later directory wins)
        let shared = skills.iter().find(|s| s.name() == "shared-skill").unwrap();
        assert!(shared.description().contains("dir2"));
    }
}
