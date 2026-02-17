use std::path::{Path, PathBuf};

use chrono::Utc;

use super::protection::{self, ChangePermission};
use super::templates;
use crate::error::{DaemonError, Result};

/// Describes a change to be applied to a soul document.
#[derive(Debug, Clone, Default)]
pub struct SoulChange {
    /// Target file, e.g., "TOOLS.md".
    pub target_file: String,
    /// Section heading to target, e.g., "Site Adaptation Graph".
    pub section: String,
    /// Type of change: "add", "modify", or "remove".
    pub change_type: String,
    /// The content to add or replace.
    pub new_content: String,
    /// Why this change is being made.
    pub reason: String,
    /// Source of the change: "system" or "manual".
    pub source_type: String,
    /// Confidence score from 0.0 to 1.0.
    pub confidence: f64,
}

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

    /// The five allowed soul document filenames.
    const ALLOWED_FILES: [&'static str; 5] = [
        "IDENTITY.md",
        "SOUL.md",
        "USER.md",
        "TOOLS.md",
        "AGENTS.md",
    ];

    /// Apply a change to one of the soul documents.
    ///
    /// 1. Checks permission via the protection module — rejects if `Forbidden`.
    /// 2. Reads the target file from disk.
    /// 3. For "add": finds the target section and appends content at the end of it.
    /// 4. Performs atomic write (write to `.tmp`, then rename).
    /// 5. Appends an entry to the daily changelog.
    /// 6. Reloads the cache.
    pub async fn apply_change(&mut self, change: SoulChange) -> Result<()> {
        // 0. Validate target file is one of the five allowed documents
        if !Self::ALLOWED_FILES.contains(&change.target_file.as_str()) {
            return Err(DaemonError::InvalidRequest(format!(
                "unknown target file: {}",
                change.target_file
            )));
        }

        // 1. Check permission
        let permission = protection::check_permission(&change.target_file, &change.section);
        if permission == ChangePermission::Forbidden {
            return Err(DaemonError::PermissionDenied(format!(
                "cannot modify {} / {}: change is forbidden",
                change.target_file, change.section
            )));
        }

        // 2. Read target file
        let file_path = self.soul_dir.join(&change.target_file);
        let content = tokio::fs::read_to_string(&file_path).await?;

        // 3. Apply the change based on change_type
        let updated = match change.change_type.as_str() {
            "add" => Self::apply_add(&content, &change.section, &change.new_content)?,
            "modify" => {
                // TODO: implement modify
                return Err(DaemonError::InternalError(
                    "change_type 'modify' is not yet implemented".into(),
                ));
            }
            "remove" => {
                // TODO: implement remove
                return Err(DaemonError::InternalError(
                    "change_type 'remove' is not yet implemented".into(),
                ));
            }
            other => {
                return Err(DaemonError::InvalidRequest(format!(
                    "unknown change_type: {}",
                    other
                )));
            }
        };

        // 4. Atomic write: write to .tmp then rename
        let tmp_path = file_path.with_extension("md.tmp");
        tokio::fs::write(&tmp_path, &updated).await?;
        tokio::fs::rename(&tmp_path, &file_path).await?;

        // 5. Append to changelog
        self.append_changelog(&change).await?;

        // 6. Reload cache
        let reloaded = Self::load(&self.soul_dir).await?;
        self.cache = reloaded.cache;

        Ok(())
    }

    /// Find the target section and append new content at its end.
    ///
    /// A section is identified by a `## heading` line. Its content extends until
    /// the next `## ` heading or end of file. The new content is inserted just
    /// before that boundary.
    fn apply_add(content: &str, section: &str, new_content: &str) -> Result<String> {
        let section_header = format!("## {}", section);
        let lines: Vec<&str> = content.lines().collect();

        // Find the section header line
        let section_start = lines
            .iter()
            .position(|line| *line == section_header.as_str())
            .ok_or_else(|| {
                DaemonError::InvalidRequest(format!("section '{}' not found in document", section))
            })?;

        // Find the end of the section: next `## ` heading or end of file
        let section_end = lines
            .iter()
            .enumerate()
            .skip(section_start + 1)
            .find(|(_, line)| line.starts_with("## "))
            .map(|(i, _)| i)
            .unwrap_or(lines.len());

        // Build the updated content
        let mut result = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i == section_end {
                // Insert new content before the next section header
                result.push_str(new_content);
                if !new_content.ends_with('\n') {
                    result.push('\n');
                }
                result.push('\n');
            }
            result.push_str(line);
            result.push('\n');
        }

        // If section_end == lines.len(), we need to append at the end
        if section_end == lines.len() {
            result.push_str(new_content);
            if !new_content.ends_with('\n') {
                result.push('\n');
            }
        }

        Ok(result)
    }

    /// Create a snapshot of all five soul documents.
    ///
    /// Copies every allowed file into `.snapshots/{YYYYMMDD-HHmmSS}/` and
    /// returns the path to the snapshot directory.
    pub async fn create_snapshot(&self) -> Result<PathBuf> {
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let snapshot_dir = self.soul_dir.join(".snapshots").join(&timestamp);
        tokio::fs::create_dir_all(&snapshot_dir).await?;

        for name in &Self::ALLOWED_FILES {
            let src = self.soul_dir.join(name);
            let dst = snapshot_dir.join(name);
            tokio::fs::copy(&src, &dst).await?;
        }

        Ok(snapshot_dir)
    }

    /// Rollback the soul directory to a previous snapshot.
    ///
    /// Restores all five document files from the given snapshot directory
    /// back into the soul directory, then reloads the in-memory cache.
    pub async fn rollback(&mut self, snapshot_path: &Path) -> Result<()> {
        // Validate snapshot path is within the snapshots directory
        let snapshots_root = self.soul_dir.join(".snapshots");
        if !snapshot_path.starts_with(&snapshots_root) {
            return Err(DaemonError::InvalidRequest(format!(
                "snapshot path is not within the snapshots directory: {}",
                snapshot_path.display()
            )));
        }

        // Pre-check all source files exist before touching the destination
        for name in &Self::ALLOWED_FILES {
            let src = snapshot_path.join(name);
            tokio::fs::metadata(&src).await.map_err(|_| {
                DaemonError::InvalidRequest(format!(
                    "snapshot is incomplete — missing file: {}",
                    name
                ))
            })?;
        }

        for name in &Self::ALLOWED_FILES {
            let src = snapshot_path.join(name);
            let dst = self.soul_dir.join(name);
            tokio::fs::copy(&src, &dst).await?;
        }

        // Reload cache from the restored files
        let reloaded = Self::load(&self.soul_dir).await?;
        self.cache = reloaded.cache;

        Ok(())
    }

    /// Clean up old snapshots, keeping only the most recent `keep` snapshots.
    ///
    /// Lists all entries in `.snapshots/`, sorts by name (which is a
    /// timestamp and therefore in chronological order), and removes all
    /// but the last `keep` entries.
    pub async fn cleanup_snapshots(&self, keep: usize) -> Result<()> {
        let snapshots_dir = self.soul_dir.join(".snapshots");
        let mut entries: Vec<String> = Vec::new();

        let mut read_dir = tokio::fs::read_dir(&snapshots_dir).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    entries.push(name.to_string());
                }
            }
        }

        entries.sort();

        if entries.len() <= keep {
            return Ok(());
        }

        let to_remove = entries.len() - keep;
        for name in &entries[..to_remove] {
            let path = snapshots_dir.join(name);
            tokio::fs::remove_dir_all(&path).await?;
        }

        Ok(())
    }

    /// Append a changelog entry for a change to `.changelog/YYYY-MM-DD.md`.
    async fn append_changelog(&self, change: &SoulChange) -> Result<()> {
        let now = Utc::now();
        let date_str = now.format("%Y-%m-%d").to_string();
        let time_str = now.format("%H:%M:%S").to_string();

        let changelog_dir = self.soul_dir.join(".changelog");
        tokio::fs::create_dir_all(&changelog_dir).await?;

        let changelog_path = changelog_dir.join(format!("{}.md", date_str));

        let entry = format!(
            "## {} \u{2014} {} / {}\n- {}: {}\n- confidence: {}\n- source: {}\n\n",
            time_str,
            change.target_file,
            change.section,
            change.change_type,
            change.reason,
            change.confidence,
            change.source_type,
        );

        // Append to file (create if not exists)
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&changelog_path)
            .await?;
        file.write_all(entry.as_bytes()).await?;

        Ok(())
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
    async fn manager_applies_change_to_tools_md() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        let change = SoulChange {
            target_file: "TOOLS.md".into(),
            section: "Site Adaptation Graph".into(),
            change_type: "add".into(),
            new_content: "### newsite.com\n- **Trust level**: normal\n".into(),
            reason: "Test change".into(),
            source_type: "system".into(),
            confidence: 0.9,
            ..Default::default()
        };

        manager.apply_change(change).await.unwrap();

        // Verify file was updated
        let content = tokio::fs::read_to_string(soul_dir.join("TOOLS.md"))
            .await
            .unwrap();
        assert!(content.contains("newsite.com"));

        // Verify changelog was written
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let changelog_path = soul_dir.join(".changelog").join(format!("{}.md", today));
        assert!(changelog_path.exists());
    }

    #[tokio::test]
    async fn manager_rejects_forbidden_change() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        let change = SoulChange {
            target_file: "SOUL.md".into(),
            section: "Safety Boundaries".into(),
            change_type: "modify".into(),
            new_content: "removed all boundaries".into(),
            reason: "Bad idea".into(),
            source_type: "system".into(),
            confidence: 1.0,
            ..Default::default()
        };

        let result = manager.apply_change(change).await;
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

    #[tokio::test]
    async fn snapshot_and_rollback() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let mut manager = SoulManager::init(&soul_dir).await.unwrap();

        // Create snapshot of initial state
        let snapshot_path = manager.create_snapshot().await.unwrap();
        assert!(snapshot_path.exists());

        // Modify TOOLS.md
        let change = SoulChange {
            target_file: "TOOLS.md".into(),
            section: "Site Adaptation Graph".into(),
            change_type: "add".into(),
            new_content: "### modified.com\n".into(),
            reason: "test".into(),
            source_type: "system".into(),
            ..Default::default()
        };
        manager.apply_change(change).await.unwrap();
        assert!(manager.cache().tools_raw.contains("modified.com"));

        // Rollback
        manager.rollback(&snapshot_path).await.unwrap();
        assert!(!manager.cache().tools_raw.contains("modified.com"));
    }

    #[tokio::test]
    async fn snapshot_copies_all_five_files() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let manager = SoulManager::init(&soul_dir).await.unwrap();

        let snapshot_path = manager.create_snapshot().await.unwrap();

        // Verify all 5 files were copied
        for name in &SoulManager::ALLOWED_FILES {
            assert!(
                snapshot_path.join(name).exists(),
                "snapshot should contain {}",
                name
            );
        }
    }

    #[tokio::test]
    async fn cleanup_snapshots_keeps_n_newest() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let manager = SoulManager::init(&soul_dir).await.unwrap();

        // Create 3 snapshots with distinct names (manually create to control names)
        let snapshots_dir = soul_dir.join(".snapshots");
        for name in &["20250101-120000", "20250102-120000", "20250103-120000"] {
            let dir = snapshots_dir.join(name);
            tokio::fs::create_dir_all(&dir).await.unwrap();
            for file in &SoulManager::ALLOWED_FILES {
                tokio::fs::copy(soul_dir.join(file), dir.join(file))
                    .await
                    .unwrap();
            }
        }

        // Keep only 1
        manager.cleanup_snapshots(1).await.unwrap();

        // Only the newest should remain
        assert!(!snapshots_dir.join("20250101-120000").exists());
        assert!(!snapshots_dir.join("20250102-120000").exists());
        assert!(snapshots_dir.join("20250103-120000").exists());
    }

    #[tokio::test]
    async fn cleanup_snapshots_noop_when_fewer_than_keep() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("soul");
        let manager = SoulManager::init(&soul_dir).await.unwrap();

        // Create 1 snapshot
        let snapshot_path = manager.create_snapshot().await.unwrap();

        // Cleanup keeping 5 should be a no-op
        manager.cleanup_snapshots(5).await.unwrap();

        // The snapshot should still exist
        assert!(snapshot_path.exists());
    }
}
