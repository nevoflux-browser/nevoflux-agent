//! Container → soul bindings.
//!
//! Users think of this as "which assistant lives in which Space", but the key on
//! disk is the Space's container (its cookieStoreId). That is the only Space-level
//! identifier that reaches the daemon, and it is also the browser's own
//! cookie/storage isolation boundary — so a soul, its private memory and its
//! cookie jar all share one key.
//!
//! ```toml
//! # <config_dir>/nevoflux/space_souls.toml
//! [bindings]
//! "firefox-container-1" = "research"   # value is a role directory (slug)
//! "firefox-container-2" = "engineer"
//! ```
//!
//! An absent file is not an error: nothing is bound, every container resolves to
//! no soul, and the assistant behaves exactly as it did before souls existed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename under the config directory.
pub const BINDINGS_FILE: &str = "space_souls.toml";

/// On-disk shape of `space_souls.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BindingsFile {
    #[serde(default)]
    bindings: HashMap<String, String>,
}

/// Container (cookieStoreId) → role slug.
#[derive(Debug, Clone, Default)]
pub struct SpaceSoulBindings {
    map: HashMap<String, String>,
}

impl SpaceSoulBindings {
    /// Load bindings from `<config_dir>/space_souls.toml`.
    ///
    /// A missing file yields empty bindings. A malformed file is reported and
    /// also yields empty bindings: a typo in one line should leave the assistant
    /// working, not refuse to start.
    pub fn load(config_dir: &Path) -> Self {
        Self::load_from(&config_dir.join(BINDINGS_FILE))
    }

    /// Load bindings from an explicit path.
    pub fn load_from(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Could not read {}: {}", path.display(), e);
                return Self::default();
            }
        };

        match toml::from_str::<BindingsFile>(&content) {
            Ok(parsed) => {
                tracing::info!(
                    "Loaded {} space→soul binding(s) from {}",
                    parsed.bindings.len(),
                    path.display()
                );
                Self {
                    map: parsed.bindings,
                }
            }
            Err(e) => {
                tracing::error!(
                    "Could not parse {}: {}. No souls will be bound until this is fixed.",
                    path.display(),
                    e
                );
                Self::default()
            }
        }
    }

    /// Build bindings directly. Used by tests and by callers that already hold a map.
    pub fn from_map(map: HashMap<String, String>) -> Self {
        Self { map }
    }

    /// The slug bound to `cookie_store_id`, if any.
    pub fn get(&self, cookie_store_id: &str) -> Option<&str> {
        self.map.get(cookie_store_id).map(|s| s.as_str())
    }

    /// Every binding, as `(cookieStoreId, slug)`.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Number of bindings.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether nothing is bound.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Bind `cookie_store_id` to `slug`, replacing any existing binding.
    pub fn set(&mut self, cookie_store_id: impl Into<String>, slug: impl Into<String>) {
        self.map.insert(cookie_store_id.into(), slug.into());
    }

    /// Remove a container's binding. Returns whether there was one.
    pub fn remove(&mut self, cookie_store_id: &str) -> bool {
        self.map.remove(cookie_store_id).is_some()
    }

    /// Write the bindings to `<config_dir>/space_souls.toml`.
    ///
    /// The file is the source of truth and stays hand-editable, so it is written
    /// whole rather than patched.
    pub fn save(&self, config_dir: &Path) -> Result<(), String> {
        let file = BindingsFile {
            bindings: self.map.clone(),
        };
        let body = toml::to_string_pretty(&file)
            .map_err(|e| format!("Could not serialize bindings: {}", e))?;

        let contents = format!(
            "# Which soul answers in which container.\n\
             #\n\
             # Keys are cookieStoreIds (a Space's container); values are role\n\
             # directory names under agents/. Edit here or in Settings → Space Souls.\n\
             {}",
            body
        );

        std::fs::create_dir_all(config_dir)
            .map_err(|e| format!("Could not create {}: {}", config_dir.display(), e))?;
        std::fs::write(bindings_path(config_dir), contents)
            .map_err(|e| format!("Could not write {}: {}", BINDINGS_FILE, e))
    }
}

/// Whether `value` looks like a cookieStoreId this daemon should key on.
///
/// Bindings are written from the UI, so the key is validated rather than trusted:
/// an arbitrary string would silently become a binding nothing can ever match,
/// and a path-shaped one has no business in a config key.
pub fn is_valid_container_id(value: &str) -> bool {
    if value == nevoflux_protocol::chat::DEFAULT_COOKIE_STORE_ID {
        return true;
    }
    match value.strip_prefix("firefox-container-") {
        Some(n) => !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Path to the bindings file under `config_dir`.
pub fn bindings_path(config_dir: &Path) -> PathBuf {
    config_dir.join(BINDINGS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join(BINDINGS_FILE);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn loads_bindings() {
        let tmp = tempfile::TempDir::new().unwrap();
        write(
            tmp.path(),
            r#"
[bindings]
"firefox-container-1" = "research"
"firefox-container-2" = "engineer"
"#,
        );

        let bindings = SpaceSoulBindings::load(tmp.path());
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings.get("firefox-container-1"), Some("research"));
        assert_eq!(bindings.get("firefox-container-2"), Some("engineer"));
        assert_eq!(bindings.get("firefox-container-9"), None);
    }

    /// No file is the normal state for anyone who has not bound a soul; it must
    /// not look like an error.
    #[test]
    fn missing_file_is_empty_not_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bindings = SpaceSoulBindings::load(tmp.path());
        assert!(bindings.is_empty());
        assert_eq!(bindings.get("firefox-default"), None);
    }

    /// A typo must not take the assistant down with it.
    #[test]
    fn malformed_file_yields_empty_bindings() {
        let tmp = tempfile::TempDir::new().unwrap();
        write(tmp.path(), "[bindings\nthis is not toml");

        let bindings = SpaceSoulBindings::load(tmp.path());
        assert!(bindings.is_empty());
    }

    #[test]
    fn empty_table_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        write(tmp.path(), "[bindings]\n");

        let bindings = SpaceSoulBindings::load(tmp.path());
        assert!(bindings.is_empty());
    }

    /// The default container is an ordinary key: a user may bind a soul to
    /// container-less tabs.
    #[test]
    fn default_container_can_be_bound() {
        let tmp = tempfile::TempDir::new().unwrap();
        write(tmp.path(), "[bindings]\n\"firefox-default\" = \"tester\"\n");

        let bindings = SpaceSoulBindings::load(tmp.path());
        assert_eq!(bindings.get("firefox-default"), Some("tester"));
    }

    // ── writing ────────────────────────────────────────────────────────

    /// The file stays hand-editable, so what the UI writes must be what `load`
    /// reads back.
    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut bindings = SpaceSoulBindings::default();
        bindings.set("firefox-container-1", "research");
        bindings.set("firefox-default", "tester");

        bindings.save(tmp.path()).unwrap();
        let reloaded = SpaceSoulBindings::load(tmp.path());

        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get("firefox-container-1"), Some("research"));
        assert_eq!(reloaded.get("firefox-default"), Some("tester"));
    }

    #[test]
    fn save_writes_a_readable_file_with_guidance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut bindings = SpaceSoulBindings::default();
        bindings.set("firefox-container-1", "research");
        bindings.save(tmp.path()).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(BINDINGS_FILE)).unwrap();
        assert!(text.contains("[bindings]"));
        assert!(text.contains("firefox-container-1"));
        assert!(
            text.starts_with('#'),
            "someone opening this file by hand should find out what it is"
        );
    }

    #[test]
    fn set_replaces_and_remove_reports() {
        let mut bindings = SpaceSoulBindings::default();
        bindings.set("firefox-container-1", "research");
        bindings.set("firefox-container-1", "engineer");
        assert_eq!(bindings.get("firefox-container-1"), Some("engineer"));

        assert!(bindings.remove("firefox-container-1"));
        assert!(!bindings.remove("firefox-container-1"), "already gone");
        assert!(bindings.is_empty());
    }

    #[test]
    fn saving_nothing_leaves_an_empty_but_valid_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        SpaceSoulBindings::default().save(tmp.path()).unwrap();

        assert!(SpaceSoulBindings::load(tmp.path()).is_empty());
    }

    // ── container id validation ────────────────────────────────────────

    #[test]
    fn accepts_real_container_ids() {
        assert!(is_valid_container_id("firefox-default"));
        assert!(is_valid_container_id("firefox-container-1"));
        assert!(is_valid_container_id("firefox-container-42"));
    }

    /// Bindings come from the UI, so a key that could never match a tab — or that
    /// is shaped like a path — is refused rather than written.
    #[test]
    fn rejects_anything_that_is_not_a_container_id() {
        for bad in [
            "",
            "default",
            "firefox-container-",
            "firefox-container-abc",
            "firefox-container-1x",
            "../../etc/passwd",
            "Work",
        ] {
            assert!(
                !is_valid_container_id(bad),
                "'{}' should not be accepted as a container id",
                bad
            );
        }
    }
}
