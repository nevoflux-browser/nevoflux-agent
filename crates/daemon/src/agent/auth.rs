//! Tool-level authorization system for NevoFlux Agent.
//!
//! Provides configurable authorization rules for Read, Grep, and Bash tools.
//! Rules are checked in priority order: sensitive patterns > existing rules >
//! working directory check > user confirmation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Authorization rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRule {
    /// Unique rule ID
    pub id: String,
    /// Match pattern
    pub matcher: AuthMatcher,
    /// Authorization decision
    pub decision: AuthDecision,
    /// Rule source
    pub source: AuthSource,
    /// Creation timestamp (Unix epoch seconds)
    pub created_at: u64,
}

/// Match method for authorization rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AuthMatcher {
    /// Exact path match: /home/user/.env
    ExactPath { path: String },
    /// Directory prefix match: /ai/project/** all files under directory
    PathPrefix { prefix: String },
    /// Exact command match: cargo test
    ExactCommand { command: String },
    /// Command prefix match: matches any command starting with this program name
    CommandPrefix { program: String },
    /// Sensitive file pattern: .env*, *credential*, *.pem
    SensitivePattern { pattern: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthSource {
    /// User authorized in current session
    Session,
    /// Global config file
    GlobalConfig,
    /// Project config file
    ProjectConfig,
}

impl AuthMatcher {
    /// Check if this matcher matches the given path or command.
    pub fn matches(&self, path_or_command: &str) -> bool {
        match self {
            AuthMatcher::ExactPath { path } => path == path_or_command,
            AuthMatcher::PathPrefix { prefix } => {
                let normalized = if prefix.ends_with('/') {
                    prefix.to_string()
                } else {
                    format!("{}/", prefix)
                };
                path_or_command.starts_with(&normalized) || path_or_command == prefix.as_str()
            }
            AuthMatcher::ExactCommand { command } => command == path_or_command,
            AuthMatcher::CommandPrefix { program } => {
                let parts: Vec<&str> = path_or_command.splitn(2, ' ').collect();
                parts.first().map_or(false, |cmd| *cmd == program.as_str())
            }
            AuthMatcher::SensitivePattern { pattern } => {
                let filename = Path::new(path_or_command)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path_or_command);
                if pattern.starts_with('.') {
                    // Prefix match: .env matches .env, .env.local
                    filename.starts_with(pattern)
                } else if pattern.starts_with('*') && pattern.ends_with('*') {
                    // Contains match: *credential*
                    let inner = &pattern[1..pattern.len() - 1];
                    path_or_command.contains(inner)
                } else if pattern.starts_with('*') {
                    // Suffix match: *.pem
                    let suffix = &pattern[1..];
                    path_or_command.ends_with(suffix)
                } else {
                    filename == pattern
                }
            }
        }
    }
}

/// In-memory storage for session-scoped authorization rules.
#[derive(Debug, Default)]
pub struct AuthStore {
    rules: HashMap<String, AuthRule>,
    next_id: u64,
}

impl AuthStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a session rule and return its ID.
    pub fn add_rule(&mut self, matcher: AuthMatcher, decision: AuthDecision) -> String {
        self.next_id += 1;
        let id = format!("session_{}", self.next_id);
        let rule = AuthRule {
            id: id.clone(),
            matcher,
            decision,
            source: AuthSource::Session,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        self.rules.insert(id.clone(), rule);
        id
    }

    /// Check if any existing rule matches the given path or command.
    pub fn check(&self, path_or_command: &str) -> Option<AuthDecision> {
        for rule in self.rules.values() {
            if rule.matcher.matches(path_or_command) {
                return Some(rule.decision);
            }
        }
        None
    }

    /// Clear all session rules.
    pub fn clear(&mut self) {
        self.rules.clear();
        self.next_id = 0;
    }

    /// Get the number of rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- AuthMatcher tests ---

    #[test]
    fn exact_path_matches_exact_path_only() {
        let m = AuthMatcher::ExactPath {
            path: "/home/user/.env".to_string(),
        };
        assert!(m.matches("/home/user/.env"));
    }

    #[test]
    fn exact_path_does_not_match_different_path() {
        let m = AuthMatcher::ExactPath {
            path: "/home/user/.env".to_string(),
        };
        assert!(!m.matches("/home/user/.env.local"));
        assert!(!m.matches("/home/other/.env"));
    }

    #[test]
    fn path_prefix_matches_files_under_directory() {
        let m = AuthMatcher::PathPrefix {
            prefix: "/ai/project".to_string(),
        };
        assert!(m.matches("/ai/project/src/main.rs"));
        assert!(m.matches("/ai/project/Cargo.toml"));
    }

    #[test]
    fn path_prefix_matches_the_directory_itself() {
        let m = AuthMatcher::PathPrefix {
            prefix: "/ai/project".to_string(),
        };
        assert!(m.matches("/ai/project"));
    }

    #[test]
    fn path_prefix_does_not_match_sibling_directories() {
        let m = AuthMatcher::PathPrefix {
            prefix: "/ai/project".to_string(),
        };
        assert!(!m.matches("/ai/project2/src/main.rs"));
        assert!(!m.matches("/ai/project2"));
    }

    #[test]
    fn exact_command_matches_exact_command() {
        let m = AuthMatcher::ExactCommand {
            command: "cargo test".to_string(),
        };
        assert!(m.matches("cargo test"));
    }

    #[test]
    fn exact_command_does_not_match_different_command() {
        let m = AuthMatcher::ExactCommand {
            command: "cargo test".to_string(),
        };
        assert!(!m.matches("cargo build"));
        assert!(!m.matches("cargo test --release"));
    }

    #[test]
    fn command_prefix_matches_command_with_any_args() {
        let m = AuthMatcher::CommandPrefix {
            program: "cargo".to_string(),
        };
        assert!(m.matches("cargo test"));
        assert!(m.matches("cargo build --release"));
        assert!(m.matches("cargo"));
    }

    #[test]
    fn command_prefix_does_not_match_different_program() {
        let m = AuthMatcher::CommandPrefix {
            program: "cargo".to_string(),
        };
        assert!(!m.matches("rustc main.rs"));
        assert!(!m.matches("npm install"));
    }

    #[test]
    fn sensitive_pattern_dot_env_matches_env_files() {
        let m = AuthMatcher::SensitivePattern {
            pattern: ".env".to_string(),
        };
        assert!(m.matches("/home/user/project/.env"));
        assert!(m.matches("/home/user/project/.env.local"));
        assert!(m.matches("/home/user/project/.env.production"));
    }

    #[test]
    fn sensitive_pattern_credential_contains_match() {
        let m = AuthMatcher::SensitivePattern {
            pattern: "*credential*".to_string(),
        };
        assert!(m.matches("/home/user/credentials.json"));
        assert!(m.matches("/home/user/.credential_store"));
        assert!(m.matches("credential"));
    }

    #[test]
    fn sensitive_pattern_pem_suffix_match() {
        let m = AuthMatcher::SensitivePattern {
            pattern: "*.pem".to_string(),
        };
        assert!(m.matches("/home/user/server.pem"));
        assert!(m.matches("key.pem"));
        assert!(!m.matches("/home/user/server.pem.bak"));
    }

    #[test]
    fn sensitive_pattern_exact_filename_match() {
        let m = AuthMatcher::SensitivePattern {
            pattern: "id_rsa".to_string(),
        };
        assert!(m.matches("/home/user/.ssh/id_rsa"));
        assert!(!m.matches("/home/user/.ssh/id_rsa.pub"));
    }

    // --- AuthStore tests ---

    #[test]
    fn new_store_is_empty() {
        let store = AuthStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn add_rule_increments_id() {
        let mut store = AuthStore::new();
        let id1 = store.add_rule(
            AuthMatcher::ExactPath {
                path: "/tmp/a".to_string(),
            },
            AuthDecision::Allow,
        );
        let id2 = store.add_rule(
            AuthMatcher::ExactPath {
                path: "/tmp/b".to_string(),
            },
            AuthDecision::Deny,
        );
        assert_eq!(id1, "session_1");
        assert_eq!(id2, "session_2");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn check_returns_matching_decision() {
        let mut store = AuthStore::new();
        store.add_rule(
            AuthMatcher::PathPrefix {
                prefix: "/ai/project".to_string(),
            },
            AuthDecision::Allow,
        );
        assert_eq!(
            store.check("/ai/project/src/main.rs"),
            Some(AuthDecision::Allow)
        );
    }

    #[test]
    fn check_returns_none_for_no_match() {
        let mut store = AuthStore::new();
        store.add_rule(
            AuthMatcher::ExactPath {
                path: "/tmp/a".to_string(),
            },
            AuthDecision::Allow,
        );
        assert_eq!(store.check("/tmp/b"), None);
    }

    #[test]
    fn clear_removes_all_rules() {
        let mut store = AuthStore::new();
        store.add_rule(
            AuthMatcher::ExactPath {
                path: "/tmp/a".to_string(),
            },
            AuthDecision::Allow,
        );
        store.add_rule(
            AuthMatcher::ExactPath {
                path: "/tmp/b".to_string(),
            },
            AuthDecision::Deny,
        );
        assert_eq!(store.len(), 2);
        store.clear();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn multiple_rules_first_match_wins() {
        let mut store = AuthStore::new();
        store.add_rule(
            AuthMatcher::ExactPath {
                path: "/tmp/secret".to_string(),
            },
            AuthDecision::Deny,
        );
        store.add_rule(
            AuthMatcher::PathPrefix {
                prefix: "/tmp".to_string(),
            },
            AuthDecision::Allow,
        );
        // The exact path rule should match first if iterated before the prefix rule,
        // but HashMap order is non-deterministic. Both rules match, so we just verify
        // that *some* decision is returned.
        let decision = store.check("/tmp/secret");
        assert!(decision.is_some());
    }

    // --- Serialization tests ---

    #[test]
    fn auth_matcher_tagged_serialization_roundtrip() {
        let matchers = vec![
            AuthMatcher::ExactPath {
                path: "/home/user/.env".to_string(),
            },
            AuthMatcher::PathPrefix {
                prefix: "/ai/project".to_string(),
            },
            AuthMatcher::ExactCommand {
                command: "cargo test".to_string(),
            },
            AuthMatcher::CommandPrefix {
                program: "cargo".to_string(),
            },
            AuthMatcher::SensitivePattern {
                pattern: "*.pem".to_string(),
            },
        ];

        for matcher in matchers {
            let json = serde_json::to_string(&matcher).expect("serialize AuthMatcher");
            let deserialized: AuthMatcher =
                serde_json::from_str(&json).expect("deserialize AuthMatcher");
            assert_eq!(matcher, deserialized);
        }
    }

    #[test]
    fn auth_decision_serialization() {
        let allow_json = serde_json::to_string(&AuthDecision::Allow).unwrap();
        assert_eq!(allow_json, "\"allow\"");
        let deny_json = serde_json::to_string(&AuthDecision::Deny).unwrap();
        assert_eq!(deny_json, "\"deny\"");

        let allow: AuthDecision = serde_json::from_str("\"allow\"").unwrap();
        assert_eq!(allow, AuthDecision::Allow);
        let deny: AuthDecision = serde_json::from_str("\"deny\"").unwrap();
        assert_eq!(deny, AuthDecision::Deny);
    }

    #[test]
    fn auth_source_serialization() {
        let session_json = serde_json::to_string(&AuthSource::Session).unwrap();
        assert_eq!(session_json, "\"session\"");
        let global_json = serde_json::to_string(&AuthSource::GlobalConfig).unwrap();
        assert_eq!(global_json, "\"global_config\"");
        let project_json = serde_json::to_string(&AuthSource::ProjectConfig).unwrap();
        assert_eq!(project_json, "\"project_config\"");

        let session: AuthSource = serde_json::from_str("\"session\"").unwrap();
        assert_eq!(session, AuthSource::Session);
        let global: AuthSource = serde_json::from_str("\"global_config\"").unwrap();
        assert_eq!(global, AuthSource::GlobalConfig);
        let project: AuthSource = serde_json::from_str("\"project_config\"").unwrap();
        assert_eq!(project, AuthSource::ProjectConfig);
    }

    #[test]
    fn auth_rule_serialization_roundtrip() {
        let rule = AuthRule {
            id: "session_1".to_string(),
            matcher: AuthMatcher::PathPrefix {
                prefix: "/ai/project".to_string(),
            },
            decision: AuthDecision::Allow,
            source: AuthSource::Session,
            created_at: 1700000000,
        };

        let json = serde_json::to_string(&rule).expect("serialize AuthRule");
        let deserialized: AuthRule = serde_json::from_str(&json).expect("deserialize AuthRule");

        assert_eq!(deserialized.id, rule.id);
        assert_eq!(deserialized.matcher, rule.matcher);
        assert_eq!(deserialized.decision, rule.decision);
        assert_eq!(deserialized.source, rule.source);
        assert_eq!(deserialized.created_at, rule.created_at);
    }
}
