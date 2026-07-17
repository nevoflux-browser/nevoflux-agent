//! Agent role definitions and registry.
//!
//! Roles are defined as Markdown files with YAML frontmatter, similar to Skills.
//! The registry scans directories at startup and provides L1 (summary) and L2 (full) loading.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use nevoflux_protocol::subagent::{AgentRoleSummary, ToolsConfig};

/// Frontmatter delimiter for role definition files.
const FRONTMATTER_DELIMITER: &str = "---";

/// YAML frontmatter metadata for a role definition file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRoleMetadata {
    /// Role name identifier
    pub name: String,
    /// Human-readable description
    #[serde(default)]
    pub description: String,
    /// Agent mode: "chat", "browser", or "agent"
    #[serde(default = "default_mode")]
    pub mode: String,
    /// LLM provider name (e.g. "anthropic", "openai")
    #[serde(default)]
    pub provider: Option<String>,
    /// Model name override
    #[serde(default)]
    pub model: Option<String>,
    /// Tool allowlist patterns (e.g. ["browser_*", "read_file"])
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Tool access mode; only valid value is "none" to disable all tools
    #[serde(default)]
    pub tools: Option<String>,
    /// Maximum iterations before timeout
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
}

fn default_mode() -> String {
    "agent".to_string()
}

fn default_max_iterations() -> u32 {
    10
}

/// Full agent role definition with parsed configuration.
#[derive(Debug, Clone)]
pub struct AgentRoleDefinition {
    /// Role name identifier
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// System prompt (Markdown body after frontmatter)
    pub system_prompt: String,
    /// Agent mode: "chat", "browser", or "agent"
    pub mode: String,
    /// LLM provider name
    pub provider: Option<String>,
    /// Model name override
    pub model: Option<String>,
    /// Tool access configuration; None means inherit mode's full tool set
    pub tools_config: Option<ToolsConfig>,
    /// Maximum iterations before timeout
    pub max_iterations: u32,
}

impl AgentRoleDefinition {
    /// Create a definition from parsed metadata and body content.
    ///
    /// # Validation rules
    /// - `tools: "none"` and non-empty `allowed_tools` are mutually exclusive
    /// - `model` requires `provider` to be set
    /// - `tools: "none"` forces `max_iterations` to 1
    pub fn from_metadata_and_body(meta: AgentRoleMetadata, body: String) -> Result<Self, String> {
        // Validate: tools=none and allowed_tools are mutually exclusive
        if meta.tools.as_deref() == Some("none") && !meta.allowed_tools.is_empty() {
            return Err("Cannot specify both 'tools: none' and 'allowed_tools'".to_string());
        }

        // Validate: model requires provider
        if meta.model.is_some() && meta.provider.is_none() {
            return Err("Specifying 'model' requires 'provider' to also be set".to_string());
        }

        // Compute tools_config and max_iterations
        let (tools_config, max_iterations) = if meta.tools.as_deref() == Some("none") {
            // tools: none disables all tools and forces single iteration
            (Some(ToolsConfig::None), 1)
        } else if !meta.allowed_tools.is_empty() {
            (
                Some(ToolsConfig::Allow(meta.allowed_tools)),
                meta.max_iterations,
            )
        } else {
            // No tool restrictions specified: inherit mode's defaults
            (None, meta.max_iterations)
        };

        Ok(Self {
            name: meta.name,
            description: meta.description,
            system_prompt: body,
            mode: meta.mode,
            provider: meta.provider,
            model: meta.model,
            tools_config,
            max_iterations,
        })
    }
}

/// Parse YAML frontmatter from a role definition file.
///
/// Returns the parsed metadata and the body content (system prompt).
/// The file format is:
/// ```text
/// ---
/// name: role-name
/// description: A brief description
/// ---
///
/// System prompt markdown content here.
/// ```
pub fn parse_role_frontmatter(content: &str) -> Result<(AgentRoleMetadata, String), String> {
    let content = content.trim();

    if !content.starts_with(FRONTMATTER_DELIMITER) {
        return Err("Missing frontmatter delimiter".into());
    }

    let after_start = &content[FRONTMATTER_DELIMITER.len()..];
    let end_pos = after_start
        .find(&format!("\n{}", FRONTMATTER_DELIMITER))
        .ok_or("Missing closing frontmatter delimiter")?;

    let yaml_content = after_start[..end_pos].trim();
    let metadata: AgentRoleMetadata =
        serde_yaml::from_str(yaml_content).map_err(|e| format!("YAML parse error: {}", e))?;

    let body_start = FRONTMATTER_DELIMITER.len() + end_pos + 1 + FRONTMATTER_DELIMITER.len();
    let body = if body_start < content.len() {
        content[body_start..].trim().to_string()
    } else {
        String::new()
    };

    Ok((metadata, body))
}

/// Registry for agent role definitions.
///
/// Supports two-layer loading:
/// - L1 (scan): Parse frontmatter only to build summaries
/// - L2 (get): Full parse on demand with caching
///
/// Built-in roles are compiled into the binary and act as a read-only base
/// layer; user-defined roles from `user_dir` override built-ins with the same
/// name.
pub struct AgentRoleRegistry {
    /// L1 summaries (name -> description)
    summaries: HashMap<String, AgentRoleSummary>,
    /// L2 cached full definitions (RwLock for interior mutability through &self)
    definitions: RwLock<HashMap<String, AgentRoleDefinition>>,
    /// User role definitions directory
    user_dir: PathBuf,
    /// Built-in role sources as (name, file content) pairs.
    ///
    /// These are compiled into the binary rather than read from disk, so they
    /// resolve on an installed machine where no source tree is present.
    builtin: Vec<(String, String)>,
}

impl AgentRoleRegistry {
    /// Create a registry over `user_dir`, backed by the compiled-in built-in roles.
    pub fn new(user_dir: PathBuf) -> Self {
        Self::with_builtin_sources(
            user_dir,
            nevoflux_builtin_wasm::BUILTIN_AGENT_ROLES
                .iter()
                .map(|(name, content)| (name.to_string(), content.to_string()))
                .collect(),
        )
    }

    /// Create a registry with an explicit built-in layer.
    ///
    /// Tests use this to exercise the fallback behavior against synthetic roles
    /// instead of whichever roles happen to ship in the binary.
    pub fn with_builtin_sources(user_dir: PathBuf, builtin: Vec<(String, String)>) -> Self {
        Self {
            summaries: HashMap::new(),
            definitions: RwLock::new(HashMap::new()),
            user_dir,
            builtin,
        }
    }

    /// Collect role summaries from both layers (L1 loading).
    ///
    /// Reads the built-in layer first, then the user directory.
    /// User roles override built-in roles with the same name.
    /// Returns the total number of distinct roles found.
    pub fn scan(&mut self) -> Result<usize, String> {
        self.summaries.clear();
        self.definitions.write().unwrap().clear();

        // Built-in layer first
        let builtin = self.builtin.clone();
        for (name, content) in &builtin {
            self.insert_summary(content, &format!("<builtin>/{}.md", name));
        }
        // Scan user directory (overrides builtins with same name)
        self.scan_directory(&self.user_dir.clone())?;

        Ok(self.summaries.len())
    }

    /// List all available role summaries.
    pub fn list(&self) -> Vec<AgentRoleSummary> {
        self.summaries.values().cloned().collect()
    }

    /// Get a full role definition by name (L2 loading with caching).
    ///
    /// Checks the definition cache first. On cache miss, loads from
    /// the user directory first, falling back to the built-in directory.
    /// Returns a cloned definition to avoid holding the lock.
    pub fn get(&self, name: &str) -> Result<AgentRoleDefinition, String> {
        // Check cache first
        {
            let cache = self.definitions.read().unwrap();
            if let Some(def) = cache.get(name) {
                return Ok(def.clone());
            }
        }

        // Load from disk
        let definition = self.load_definition(name)?;

        // Cache it
        {
            let mut cache = self.definitions.write().unwrap();
            cache.insert(name.to_string(), definition.clone());
        }

        Ok(definition)
    }

    /// Scan a single directory for role definition files.
    fn scan_directory(&mut self, dir: &Path) -> Result<usize, String> {
        if !dir.exists() {
            return Ok(0);
        }

        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

        let mut count = 0;
        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read directory entry: {}", e))?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to read role file {}: {}", path.display(), e);
                    continue;
                }
            };

            if self.insert_summary(&content, &path.display().to_string()) {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Parse `content` and record its L1 summary, overwriting any earlier entry
    /// with the same role name. `origin` labels the source in warnings.
    ///
    /// Returns `true` when a summary was recorded.
    fn insert_summary(&mut self, content: &str, origin: &str) -> bool {
        let (metadata, _body) = match parse_role_frontmatter(content) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!("Failed to parse role file {}: {}", origin, e);
                return false;
            }
        };

        if metadata.name.is_empty() {
            tracing::warn!("Skipping role file {} with empty name", origin);
            return false;
        }

        if metadata.description.is_empty() {
            tracing::warn!("Role file {} has empty description", origin);
        }

        self.summaries.insert(
            metadata.name.clone(),
            AgentRoleSummary {
                name: metadata.name,
                description: metadata.description,
            },
        );
        true
    }

    /// Load a full role definition.
    ///
    /// Checks the user directory first, then falls back to the compiled-in
    /// built-in layer.
    fn load_definition(&self, name: &str) -> Result<AgentRoleDefinition, String> {
        // Try user directory first
        let user_path = self.user_dir.join(format!("{}.md", name));
        if user_path.exists() {
            return self.parse_definition_file(&user_path);
        }

        // Fall back to the built-in layer
        if let Some((_, content)) = self.builtin.iter().find(|(n, _)| n == name) {
            let (metadata, body) = parse_role_frontmatter(content)?;
            return AgentRoleDefinition::from_metadata_and_body(metadata, body);
        }

        Err(format!("Role '{}' not found", name))
    }

    /// Parse a full role definition from a file.
    fn parse_definition_file(&self, path: &Path) -> Result<AgentRoleDefinition, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read role file {}: {}", path.display(), e))?;
        let (metadata, body) = parse_role_frontmatter(&content)?;
        AgentRoleDefinition::from_metadata_and_body(metadata, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_role_frontmatter_basic() {
        let content = r#"---
name: researcher
description: A role for web research tasks
mode: browser
provider: anthropic
model: claude-sonnet-4-20250514
allowed_tools:
  - browser_*
  - read_file
max_iterations: 20
---

You are a research assistant. Your job is to find and summarize information.

## Guidelines

- Always cite your sources
- Be thorough but concise
"#;

        let (meta, body) = parse_role_frontmatter(content).unwrap();
        assert_eq!(meta.name, "researcher");
        assert_eq!(meta.description, "A role for web research tasks");
        assert_eq!(meta.mode, "browser");
        assert_eq!(meta.provider, Some("anthropic".to_string()));
        assert_eq!(meta.model, Some("claude-sonnet-4-20250514".to_string()));
        assert_eq!(meta.allowed_tools, vec!["browser_*", "read_file"]);
        assert_eq!(meta.max_iterations, 20);
        assert!(body.contains("research assistant"));
        assert!(body.contains("## Guidelines"));
    }

    #[test]
    fn test_parse_role_frontmatter_minimal() {
        let content = r#"---
name: simple
description: A simple role
---

Just a simple system prompt.
"#;

        let (meta, body) = parse_role_frontmatter(content).unwrap();
        assert_eq!(meta.name, "simple");
        assert_eq!(meta.description, "A simple role");
        assert_eq!(meta.mode, "agent"); // default
        assert_eq!(meta.max_iterations, 10); // default
        assert!(meta.provider.is_none());
        assert!(meta.model.is_none());
        assert!(meta.allowed_tools.is_empty());
        assert!(meta.tools.is_none());
        assert!(body.contains("simple system prompt"));
    }

    #[test]
    fn test_parse_role_frontmatter_missing_delimiter() {
        let content = "name: broken\nno frontmatter here";
        let result = parse_role_frontmatter(content);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Missing frontmatter delimiter"));
    }

    #[test]
    fn test_parse_role_frontmatter_invalid_yaml() {
        let content = r#"---
name: [invalid yaml
---

Content
"#;

        let result = parse_role_frontmatter(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("YAML parse error"));
    }

    #[test]
    fn test_definition_model_without_provider() {
        let meta = AgentRoleMetadata {
            name: "test".to_string(),
            description: "test".to_string(),
            mode: default_mode(),
            provider: None,
            model: Some("gpt-4o".to_string()),
            allowed_tools: vec![],
            tools: None,
            max_iterations: default_max_iterations(),
        };

        let result = AgentRoleDefinition::from_metadata_and_body(meta, String::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("provider"));
    }

    #[test]
    fn test_definition_tools_none_and_allowed_tools() {
        let meta = AgentRoleMetadata {
            name: "test".to_string(),
            description: "test".to_string(),
            mode: default_mode(),
            provider: None,
            model: None,
            allowed_tools: vec!["read_file".to_string()],
            tools: Some("none".to_string()),
            max_iterations: default_max_iterations(),
        };

        let result = AgentRoleDefinition::from_metadata_and_body(meta, String::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("tools: none"));
    }

    #[test]
    fn test_definition_tools_none_forces_max_iterations_1() {
        let meta = AgentRoleMetadata {
            name: "analyzer".to_string(),
            description: "Pure analysis".to_string(),
            mode: "chat".to_string(),
            provider: None,
            model: None,
            allowed_tools: vec![],
            tools: Some("none".to_string()),
            max_iterations: 20, // will be forced to 1
        };

        let def =
            AgentRoleDefinition::from_metadata_and_body(meta, "Analyze this.".to_string()).unwrap();
        assert_eq!(def.max_iterations, 1);
        assert_eq!(def.tools_config, Some(ToolsConfig::None));
    }

    #[test]
    fn test_definition_tools_config_allow() {
        let meta = AgentRoleMetadata {
            name: "restricted".to_string(),
            description: "Restricted tools".to_string(),
            mode: default_mode(),
            provider: None,
            model: None,
            allowed_tools: vec!["browser_*".to_string(), "read_file".to_string()],
            tools: None,
            max_iterations: 15,
        };

        let def = AgentRoleDefinition::from_metadata_and_body(meta, String::new()).unwrap();
        assert_eq!(
            def.tools_config,
            Some(ToolsConfig::Allow(vec![
                "browser_*".to_string(),
                "read_file".to_string()
            ]))
        );
        assert_eq!(def.max_iterations, 15);
    }

    #[test]
    fn test_definition_tools_config_none() {
        let meta = AgentRoleMetadata {
            name: "no-tools".to_string(),
            description: "No tools".to_string(),
            mode: "chat".to_string(),
            provider: None,
            model: None,
            allowed_tools: vec![],
            tools: Some("none".to_string()),
            max_iterations: default_max_iterations(),
        };

        let def = AgentRoleDefinition::from_metadata_and_body(meta, String::new()).unwrap();
        assert_eq!(def.tools_config, Some(ToolsConfig::None));
        assert_eq!(def.max_iterations, 1);
    }

    #[test]
    fn test_definition_tools_config_inherit() {
        let meta = AgentRoleMetadata {
            name: "inherit".to_string(),
            description: "Inherits tools".to_string(),
            mode: default_mode(),
            provider: None,
            model: None,
            allowed_tools: vec![],
            tools: None,
            max_iterations: default_max_iterations(),
        };

        let def = AgentRoleDefinition::from_metadata_and_body(meta, String::new()).unwrap();
        assert_eq!(def.tools_config, None); // None = inherit
    }

    #[test]
    fn test_registry_scan_and_list() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        std::fs::create_dir_all(&roles_dir).unwrap();

        // Create two role files
        std::fs::write(
            roles_dir.join("researcher.md"),
            r#"---
name: researcher
description: Web research role
mode: browser
---

You are a researcher.
"#,
        )
        .unwrap();

        std::fs::write(
            roles_dir.join("coder.md"),
            r#"---
name: coder
description: Code writing role
---

You write clean code.
"#,
        )
        .unwrap();

        // Non-.md file should be ignored
        std::fs::write(roles_dir.join("notes.txt"), "not a role").unwrap();

        let mut registry = AgentRoleRegistry::with_builtin_sources(roles_dir, Vec::new());
        let count = registry.scan().unwrap();
        assert_eq!(count, 2);

        let summaries = registry.list();
        assert_eq!(summaries.len(), 2);

        let names: Vec<&str> = summaries.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"researcher"));
        assert!(names.contains(&"coder"));
    }

    #[test]
    fn test_registry_user_overrides_builtin() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let user_dir = temp_dir.path().join("user");
        std::fs::create_dir_all(&user_dir).unwrap();

        let builtin = vec![(
            "writer".to_string(),
            r#"---
name: writer
description: Built-in writer role
---

Built-in prompt.
"#
            .to_string(),
        )];

        // User role with same name
        std::fs::write(
            user_dir.join("writer.md"),
            r#"---
name: writer
description: Custom writer role
---

Custom prompt.
"#,
        )
        .unwrap();

        let mut registry = AgentRoleRegistry::with_builtin_sources(user_dir, builtin);
        let count = registry.scan().unwrap();
        assert_eq!(count, 1); // Same name, so only 1 summary

        let summaries = registry.list();
        assert_eq!(summaries.len(), 1);
        // User description should override builtin
        assert_eq!(summaries[0].description, "Custom writer role");
    }

    #[test]
    fn test_registry_get_caches() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let user_dir = temp_dir.path().join("user");
        std::fs::create_dir_all(&user_dir).unwrap();

        std::fs::write(
            user_dir.join("tester.md"),
            r#"---
name: tester
description: Test role
mode: agent
---

You test things.
"#,
        )
        .unwrap();

        let mut registry = AgentRoleRegistry::with_builtin_sources(user_dir, Vec::new());
        registry.scan().unwrap();

        // First get loads from disk
        let def1 = registry.get("tester").unwrap();
        assert_eq!(def1.name, "tester");
        assert_eq!(def1.mode, "agent");

        // Second get should return cached definition
        let def2 = registry.get("tester").unwrap();
        assert_eq!(def2.name, "tester");

        // Verify it's in the cache
        assert!(registry.definitions.read().unwrap().contains_key("tester"));
    }

    #[test]
    fn test_registry_get_not_found() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let user_dir = temp_dir.path().join("user");
        std::fs::create_dir_all(&user_dir).unwrap();

        let mut registry = AgentRoleRegistry::with_builtin_sources(user_dir, Vec::new());
        registry.scan().unwrap();

        let result = registry.get("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_registry_get_prefers_user_over_builtin() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let user_dir = temp_dir.path().join("user");
        std::fs::create_dir_all(&user_dir).unwrap();

        // Same name in both layers
        let builtin = vec![(
            "helper".to_string(),
            r#"---
name: helper
description: Built-in helper
---

Built-in helper prompt.
"#
            .to_string(),
        )];

        std::fs::write(
            user_dir.join("helper.md"),
            r#"---
name: helper
description: User helper
---

User helper prompt.
"#,
        )
        .unwrap();

        let mut registry = AgentRoleRegistry::with_builtin_sources(user_dir, builtin);
        registry.scan().unwrap();

        let def = registry.get("helper").unwrap();
        assert_eq!(def.description, "User helper");
        assert!(def.system_prompt.contains("User helper prompt"));
    }

    #[test]
    fn test_registry_scan_nonexistent_directory() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let user_dir = temp_dir.path().join("nonexistent_user");

        let mut registry = AgentRoleRegistry::with_builtin_sources(user_dir, Vec::new());
        let count = registry.scan().unwrap();
        assert_eq!(count, 0);
        assert!(registry.list().is_empty());
    }

    #[test]
    fn test_registry_scan_skips_invalid_files() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let user_dir = temp_dir.path().join("user");
        std::fs::create_dir_all(&user_dir).unwrap();

        // Valid role
        std::fs::write(
            user_dir.join("valid.md"),
            r#"---
name: valid
description: Valid role
---

Content.
"#,
        )
        .unwrap();

        // Invalid YAML
        std::fs::write(
            user_dir.join("broken.md"),
            r#"---
name: [broken
---

Content.
"#,
        )
        .unwrap();

        // Missing frontmatter
        std::fs::write(user_dir.join("nofm.md"), "No frontmatter here").unwrap();

        let mut registry = AgentRoleRegistry::with_builtin_sources(user_dir, Vec::new());
        let count = registry.scan().unwrap();
        assert_eq!(count, 1);
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.list()[0].name, "valid");
    }

    #[test]
    fn test_definition_with_provider_and_model() {
        let meta = AgentRoleMetadata {
            name: "custom".to_string(),
            description: "Custom model".to_string(),
            mode: "chat".to_string(),
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            allowed_tools: vec![],
            tools: None,
            max_iterations: 5,
        };

        let def =
            AgentRoleDefinition::from_metadata_and_body(meta, "Use GPT-4o.".to_string()).unwrap();
        assert_eq!(def.provider, Some("openai".to_string()));
        assert_eq!(def.model, Some("gpt-4o".to_string()));
        assert_eq!(def.max_iterations, 5);
    }

    #[test]
    fn test_parse_frontmatter_empty_body() {
        let content = r#"---
name: empty
description: No body
---
"#;

        let (meta, body) = parse_role_frontmatter(content).unwrap();
        assert_eq!(meta.name, "empty");
        assert!(body.is_empty());
    }

    #[test]
    fn test_parse_frontmatter_with_code_blocks() {
        let content = r#"---
name: coder
description: Code helper
---

Write code following these patterns:

```rust
fn main() {
    println!("Hello");
}
```

Always add tests.
"#;

        let (meta, body) = parse_role_frontmatter(content).unwrap();
        assert_eq!(meta.name, "coder");
        assert!(body.contains("```rust"));
        assert!(body.contains("fn main()"));
        assert!(body.contains("Always add tests."));
    }

    /// Look up a role's embedded source by its catalog key.
    fn embedded_role(name: &str) -> &'static str {
        nevoflux_builtin_wasm::BUILTIN_AGENT_ROLES
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("Built-in role not embedded: {}", name))
            .1
    }

    #[test]
    fn test_builtin_roles_parse() {
        let expected_roles = vec![
            ("explorer", "browser", 10),
            ("researcher", "browser", 20),
            ("worker", "agent", 15),
            ("reader", "agent", 10),
        ];

        for (name, mode, max_iter) in expected_roles {
            let (meta, body) = parse_role_frontmatter(embedded_role(name)).unwrap();

            assert_eq!(meta.name, name, "Name mismatch for {}", name);
            assert!(
                !meta.description.is_empty(),
                "Description empty for {}",
                name
            );
            assert_eq!(meta.mode, mode, "Mode mismatch for {}", name);
            assert_eq!(
                meta.max_iterations, max_iter,
                "max_iterations mismatch for {}",
                name
            );
            assert!(!body.is_empty(), "Body empty for {}", name);

            // Verify it validates as a definition too
            let def = AgentRoleDefinition::from_metadata_and_body(meta, body).unwrap();
            assert_eq!(def.name, name);
        }
    }

    /// `load_definition` looks built-ins up by their catalog key, while `scan`
    /// advertises the frontmatter `name`. If the two ever diverged, `get()`
    /// would fail for a role that `list()` reports as available.
    #[test]
    fn test_builtin_catalog_key_matches_frontmatter_name() {
        for (key, content) in nevoflux_builtin_wasm::BUILTIN_AGENT_ROLES {
            let (meta, _) = parse_role_frontmatter(content)
                .unwrap_or_else(|e| panic!("Built-in role '{}' failed to parse: {}", key, e));
            assert_eq!(
                &meta.name, key,
                "Built-in role catalog key '{}' does not match its frontmatter name '{}'",
                key, meta.name
            );
        }
    }

    /// The built-in roles must resolve with no role directory on disk anywhere.
    /// This is what an installed binary sees: the source tree it was built from
    /// is gone, and the user has never created `<config>/nevoflux/agents`.
    #[test]
    fn test_builtin_roles_resolve_without_any_directory() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let missing_user_dir = temp_dir.path().join("no_such_agents_dir");
        assert!(!missing_user_dir.exists());

        let mut registry = AgentRoleRegistry::new(missing_user_dir);
        let count = registry.scan().unwrap();
        assert_eq!(count, 4, "Expected the 4 embedded built-in roles");

        let names: Vec<String> = registry.list().into_iter().map(|s| s.name).collect();
        for expected in ["explorer", "researcher", "worker", "reader"] {
            assert!(
                names.contains(&expected.to_string()),
                "Missing {}",
                expected
            );

            // L2 load must work too, not just the L1 summary.
            let def = registry.get(expected).unwrap();
            assert_eq!(def.name, expected);
            assert!(!def.system_prompt.is_empty());
        }
    }
}
