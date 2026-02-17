# Self-Learning System Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement NevoFlux's self-learning system: a three-layer knowledge pipeline (DashMap → SQLite → Markdown files) with five-document personality architecture, configurable validation/promotion, lazy decay, and privacy controls.

**Architecture:** Events from the Agent Runner, MCP Tools, and WASM Host flow through a `LearningCollector` into an in-memory `DashMap` buffer, then flush to SQLite. A validation pipeline promotes entries from `pending` to `validated`. A promotion pipeline lifts validated knowledge into five Markdown files (IDENTITY/SOUL/USER/TOOLS/AGENTS) managed by `SoulManager`. A separate `KnowledgeRetriever` reads the SoulManager cache + SQLite to feed the Agent Runner.

**Tech Stack:** Rust, SQLite (rusqlite), DashMap, pulldown-cmark, notify (file watcher), tokio::fs, tar, AES-256-GCM (aes-gcm crate), OS keychain (keyring crate)

**Design Doc:** `docs/plans/2026-02-17-self-learning-system-design.md`

---

## Phase 0: Instrumentation

> Define core types and instrument perception points. Log-only output, no storage backend yet.

### Task 1: Create LearningEntry and LearningCategory types

**Files:**
- Create: `crates/daemon/src/learning/mod.rs`
- Create: `crates/daemon/src/learning/types.rs`
- Modify: `crates/daemon/src/lib.rs` — add `pub mod learning;`
- Test: `crates/daemon/src/learning/types.rs` (inline tests)

**Step 1: Write the failing test**

In `crates/daemon/src/learning/types.rs`, add at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learning_entry_creation() {
        let entry = LearningEntry::new(
            LearningCategory::SiteInteraction,
            "click_failed",
            "Click on .btn-submit failed: element not found",
        );
        assert_eq!(entry.category, LearningCategory::SiteInteraction);
        assert_eq!(entry.source_event, "click_failed");
        assert!(entry.id.starts_with("LE-"));
        assert_eq!(entry.status, EntryStatus::Pending);
        assert_eq!(entry.occurrence_count, 1);
        assert!(entry.confidence > 0.0);
    }

    #[test]
    fn learning_entry_serialization_roundtrip() {
        let entry = LearningEntry::new(
            LearningCategory::ToolOptimization,
            "tool_timeout",
            "web_fetch timed out after 5000ms",
        );
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: LearningEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.id, deserialized.id);
        assert_eq!(entry.category, deserialized.category);
    }

    #[test]
    fn priority_ordering() {
        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High > Priority::Medium);
        assert!(Priority::Medium > Priority::Low);
    }

    #[test]
    fn privacy_level_defaults() {
        let entry = LearningEntry::new(
            LearningCategory::UserPreference,
            "language_preference",
            "User prefers Chinese",
        );
        assert_eq!(entry.privacy_level, PrivacyLevel::Internal);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p nevoflux-daemon learning::types::tests -- --nocapture`
Expected: FAIL — module doesn't exist yet

**Step 3: Write minimal implementation**

Create `crates/daemon/src/learning/mod.rs`:

```rust
pub mod types;
```

Create `crates/daemon/src/learning/types.rs`:

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningCategory {
    SiteInteraction,
    ToolOptimization,
    UserPreference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    Pending,
    Validated,
    Promoted,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyLevel {
    Public,
    Internal,
    Sensitive,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DocumentTarget {
    Identity,
    Soul,
    User,
    Tools,
    Agents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningContext {
    pub url: Option<String>,
    pub domain: Option<String>,
    pub selector: Option<String>,
    pub tool_name: Option<String>,
    pub session_id: Option<String>,
}

impl Default for LearningContext {
    fn default() -> Self {
        Self {
            url: None,
            domain: None,
            selector: None,
            tool_name: None,
            session_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningEntry {
    pub id: String,
    pub category: LearningCategory,
    pub subcategory: Option<String>,
    pub source_event: String,
    pub summary: String,
    pub details: Option<String>,
    pub context: LearningContext,
    pub priority: Priority,
    pub status: EntryStatus,
    pub confidence: f64,
    pub occurrence_count: u32,
    pub privacy_level: PrivacyLevel,
    pub promotion_target: Option<DocumentTarget>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

impl LearningEntry {
    pub fn new(
        category: LearningCategory,
        source_event: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: format!("LE-{}-{}", now.format("%Y%m%d%H%M%S"), &Uuid::new_v4().to_string()[..6]),
            category,
            subcategory: None,
            source_event: source_event.into(),
            summary: summary.into(),
            details: None,
            context: LearningContext::default(),
            priority: Priority::Medium,
            status: EntryStatus::Pending,
            confidence: 0.5,
            occurrence_count: 1,
            privacy_level: PrivacyLevel::Internal,
            promotion_target: None,
            created_at: now,
            last_seen_at: now,
        }
    }

    pub fn with_context(mut self, context: LearningContext) -> Self {
        self.context = context;
        self
    }

    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_privacy(mut self, level: PrivacyLevel) -> Self {
        self.privacy_level = level;
        self
    }

    pub fn with_subcategory(mut self, sub: impl Into<String>) -> Self {
        self.subcategory = Some(sub.into());
        self
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn with_promotion_target(mut self, target: DocumentTarget) -> Self {
        self.promotion_target = Some(target);
        self
    }
}
```

Add to `crates/daemon/src/lib.rs`:

```rust
pub mod learning;
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p nevoflux-daemon learning::types::tests -- --nocapture`
Expected: PASS (all 4 tests)

**Step 5: Commit**

```bash
git add crates/daemon/src/learning/mod.rs crates/daemon/src/learning/types.rs crates/daemon/src/lib.rs
git commit -m "feat(learning): add LearningEntry, LearningCategory, and core types"
```

---

### Task 2: Create LearningSource trait

**Files:**
- Create: `crates/daemon/src/learning/source.rs`
- Modify: `crates/daemon/src/learning/mod.rs` — add `pub mod source;`
- Test: `crates/daemon/src/learning/source.rs` (inline tests)

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning::types::*;

    struct MockSource {
        entries: Vec<LearningEntry>,
    }

    impl LearningSource for MockSource {
        fn source_name(&self) -> &str {
            "mock"
        }

        fn collect(&self) -> Vec<LearningEntry> {
            self.entries.clone()
        }
    }

    #[test]
    fn mock_source_produces_entries() {
        let source = MockSource {
            entries: vec![LearningEntry::new(
                LearningCategory::SiteInteraction,
                "test",
                "test summary",
            )],
        };
        assert_eq!(source.source_name(), "mock");
        assert_eq!(source.collect().len(), 1);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p nevoflux-daemon learning::source::tests -- --nocapture`
Expected: FAIL

**Step 3: Write minimal implementation**

Create `crates/daemon/src/learning/source.rs`:

```rust
use super::types::LearningEntry;

/// Trait for components that produce learning entries.
/// Implemented by Agent Runner, MCP Tools, WASM Host, and Bridge.
pub trait LearningSource: Send + Sync {
    /// Human-readable name of this source (e.g., "agent_runner", "mcp_tools")
    fn source_name(&self) -> &str;

    /// Collect pending learning entries from this source.
    /// Called by the LearningCollector during its collection cycle.
    fn collect(&self) -> Vec<LearningEntry>;
}
```

Update `crates/daemon/src/learning/mod.rs`:

```rust
pub mod source;
pub mod types;
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p nevoflux-daemon learning::source::tests -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/daemon/src/learning/source.rs crates/daemon/src/learning/mod.rs
git commit -m "feat(learning): add LearningSource trait"
```

---

### Task 3: Create LearningCollector skeleton with log-only output

**Files:**
- Create: `crates/daemon/src/learning/collector.rs`
- Modify: `crates/daemon/src/learning/mod.rs` — add `pub mod collector;`
- Test: `crates/daemon/src/learning/collector.rs` (inline tests)

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning::source::LearningSource;
    use crate::learning::types::*;
    use std::sync::{Arc, Mutex};

    struct FakeSource {
        entries: Arc<Mutex<Vec<LearningEntry>>>,
    }

    impl LearningSource for FakeSource {
        fn source_name(&self) -> &str {
            "fake"
        }

        fn collect(&self) -> Vec<LearningEntry> {
            let mut entries = self.entries.lock().unwrap();
            entries.drain(..).collect()
        }
    }

    #[test]
    fn collector_registers_source_and_collects() {
        let entries = Arc::new(Mutex::new(vec![
            LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed"),
        ]));

        let source = FakeSource {
            entries: entries.clone(),
        };

        let mut collector = LearningCollector::new();
        collector.register_source(Box::new(source));

        let collected = collector.collect_all();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].source_event, "click");
    }

    #[test]
    fn collector_dedup_exact_match() {
        let entry1 = LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed")
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                selector: Some(".btn".into()),
                ..Default::default()
            });

        let entry2 = LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed")
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                selector: Some(".btn".into()),
                ..Default::default()
            });

        let mut collector = LearningCollector::new();
        let deduped = collector.dedup(vec![entry1, entry2]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].occurrence_count, 2);
    }

    #[test]
    fn collector_filters_private_entries() {
        let entry = LearningEntry::new(LearningCategory::UserPreference, "password", "user typed password")
            .with_privacy(PrivacyLevel::Private);

        let mut collector = LearningCollector::new();
        let filtered = collector.filter_privacy(vec![entry]);
        assert!(filtered.is_empty());
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p nevoflux-daemon learning::collector::tests -- --nocapture`
Expected: FAIL

**Step 3: Write minimal implementation**

Create `crates/daemon/src/learning/collector.rs`:

```rust
use super::source::LearningSource;
use super::types::{LearningEntry, PrivacyLevel};
use tracing::{debug, info};

/// Collects learning entries from registered sources,
/// deduplicates, filters, and outputs them.
/// Phase 0: log-only output. Phase 2: writes to DashMap buffer.
pub struct LearningCollector {
    sources: Vec<Box<dyn LearningSource>>,
    domain_blacklist: Vec<String>,
}

impl LearningCollector {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            domain_blacklist: Vec::new(),
        }
    }

    pub fn register_source(&mut self, source: Box<dyn LearningSource>) {
        self.sources.push(source);
    }

    pub fn set_domain_blacklist(&mut self, blacklist: Vec<String>) {
        self.domain_blacklist = blacklist;
    }

    /// Collect entries from all registered sources.
    pub fn collect_all(&mut self) -> Vec<LearningEntry> {
        let mut all_entries = Vec::new();

        for source in &self.sources {
            let entries = source.collect();
            debug!(
                source = source.source_name(),
                count = entries.len(),
                "Collected learning entries"
            );
            all_entries.extend(entries);
        }

        let filtered = self.filter_privacy(all_entries);
        let filtered = self.filter_blacklisted_domains(filtered);
        let deduped = self.dedup(filtered);

        info!(count = deduped.len(), "Learning collector cycle complete");

        deduped
    }

    /// Filter out private entries — they must never be persisted.
    pub fn filter_privacy(&self, entries: Vec<LearningEntry>) -> Vec<LearningEntry> {
        entries
            .into_iter()
            .filter(|e| e.privacy_level != PrivacyLevel::Private)
            .collect()
    }

    /// Filter out entries from blacklisted domains.
    fn filter_blacklisted_domains(&self, entries: Vec<LearningEntry>) -> Vec<LearningEntry> {
        if self.domain_blacklist.is_empty() {
            return entries;
        }
        entries
            .into_iter()
            .filter(|e| {
                if let Some(domain) = &e.context.domain {
                    !self.domain_blacklist.iter().any(|b| domain.contains(b))
                } else {
                    true
                }
            })
            .collect()
    }

    /// Deduplicate entries by (domain, selector, category) exact match.
    /// Merges duplicates by incrementing occurrence_count.
    pub fn dedup(&mut self, entries: Vec<LearningEntry>) -> Vec<LearningEntry> {
        let mut result: Vec<LearningEntry> = Vec::new();

        for entry in entries {
            if let Some(existing) = result.iter_mut().find(|e| Self::is_duplicate(e, &entry)) {
                existing.occurrence_count += 1;
                existing.last_seen_at = entry.last_seen_at;
                if entry.confidence > existing.confidence {
                    existing.confidence = entry.confidence;
                }
            } else {
                result.push(entry);
            }
        }

        result
    }

    fn is_duplicate(a: &LearningEntry, b: &LearningEntry) -> bool {
        a.category == b.category
            && a.context.domain == b.context.domain
            && a.context.selector == b.context.selector
            && a.source_event == b.source_event
    }
}
```

Update `crates/daemon/src/learning/mod.rs`:

```rust
pub mod collector;
pub mod source;
pub mod types;
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p nevoflux-daemon learning::collector::tests -- --nocapture`
Expected: PASS (all 3 tests)

**Step 5: Commit**

```bash
git add crates/daemon/src/learning/collector.rs crates/daemon/src/learning/mod.rs
git commit -m "feat(learning): add LearningCollector with dedup, privacy filter, and domain blacklist"
```

---

### Task 4: Add learning config section to AgentConfig

**Files:**
- Modify: `crates/daemon/src/config.rs` — add `LearningConfig` struct and `learning` field
- Test: inline tests in config.rs

**Step 1: Write the failing test**

```rust
#[test]
fn learning_config_defaults() {
    let config = LearningConfig::default();
    assert!(config.enabled);
    assert_eq!(config.flush_threshold, 20);
    assert_eq!(config.flush_interval_secs, 30);
    assert_eq!(config.validation.min_alive_hours, 24);
    assert_eq!(config.validation.min_occurrences, 3);
}

#[test]
fn learning_config_from_toml() {
    let toml_str = r#"
    [learning]
    enabled = false
    flush_threshold = 50

    [learning.validation]
    min_alive_hours = 48
    "#;
    let config: LearningConfig = toml::from_str(toml_str).unwrap();
    assert!(!config.enabled);
    assert_eq!(config.flush_threshold, 50);
    assert_eq!(config.validation.min_alive_hours, 48);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p nevoflux-daemon config::tests::learning_config -- --nocapture`
Expected: FAIL — `LearningConfig` doesn't exist

**Step 3: Write minimal implementation**

Add to `crates/daemon/src/config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningConfig {
    pub enabled: bool,
    pub flush_threshold: usize,
    pub flush_interval_secs: u64,
    pub rate_limit_per_hour: u32,
    pub soul_dir: Option<String>,
    pub validation: ValidationConfig,
    pub promotion: PromotionConfig,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            flush_threshold: 20,
            flush_interval_secs: 30,
            rate_limit_per_hour: 5,
            soul_dir: None,
            validation: ValidationConfig::default(),
            promotion: PromotionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidationConfig {
    pub min_alive_hours: u64,
    pub min_occurrences: u32,
    pub min_confidence: f64,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            min_alive_hours: 24,
            min_occurrences: 3,
            min_confidence: 0.6,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromotionConfig {
    pub site_interaction_min_hits: u32,
    pub site_interaction_min_effectiveness: f64,
    pub tool_optimization_min_hits: u32,
    pub tool_optimization_min_effectiveness: f64,
    pub user_preference_min_hits: u32,
    pub min_alive_days: u64,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            site_interaction_min_hits: 10,
            site_interaction_min_effectiveness: 0.6,
            tool_optimization_min_hits: 10,
            tool_optimization_min_effectiveness: 0.7,
            user_preference_min_hits: 5,
            min_alive_days: 7,
        }
    }
}
```

Add `learning: LearningConfig` field to the existing `AgentConfig` struct (with `#[serde(default)]`).

**Step 4: Run test to verify it passes**

Run: `cargo test -p nevoflux-daemon config::tests::learning_config -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/daemon/src/config.rs
git commit -m "feat(config): add learning config section with validation and promotion thresholds"
```

---

### Task 5: Instrument tool execution in AgentRunner

**Files:**
- Modify: `crates/daemon/src/agent/runner.rs` — add learning entry emission after tool execution
- Modify: `crates/daemon/src/agent/tools.rs` — add `ToolExecutionRecord` struct

**Step 1: Write the failing test**

In `crates/daemon/src/agent/tools.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_execution_record_captures_outcome() {
        let record = ToolExecutionRecord {
            tool_name: "web_fetch".into(),
            arguments_summary: r#"{"url":"https://example.com"}"#.into(),
            success: true,
            error_message: None,
            duration_ms: 1500,
            session_id: "sess-123".into(),
        };
        assert!(record.success);
        assert_eq!(record.duration_ms, 1500);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p nevoflux-daemon agent::tools::tests::tool_execution_record -- --nocapture`
Expected: FAIL

**Step 3: Write minimal implementation**

Add to `crates/daemon/src/agent/tools.rs`:

```rust
/// Record of a tool execution, used by the learning system.
#[derive(Debug, Clone)]
pub struct ToolExecutionRecord {
    pub tool_name: String,
    pub arguments_summary: String,
    pub success: bool,
    pub error_message: Option<String>,
    pub duration_ms: u64,
    pub session_id: String,
}
```

This struct is used by the Agent Runner to produce `LearningEntry` instances after each tool call. The actual emission logic is added in Phase 2 when the memory buffer exists.

**Step 4: Run test to verify it passes**

Run: `cargo test -p nevoflux-daemon agent::tools::tests::tool_execution_record -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/daemon/src/agent/tools.rs crates/daemon/src/agent/runner.rs
git commit -m "feat(agent): add ToolExecutionRecord for learning system instrumentation"
```

---

## Phase 1: Five-Document Framework

> Build SoulManager with full five-document lifecycle, protection levels, changelog, and snapshots.

### Task 6: Create initial five Markdown files and directory structure

**Files:**
- Create: `crates/daemon/src/learning/soul/mod.rs`
- Create: `crates/daemon/src/learning/soul/templates.rs` — default content for each file
- Modify: `crates/daemon/src/learning/mod.rs` — add `pub mod soul;`
- Test: `crates/daemon/src/learning/soul/templates.rs` (inline tests)

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_identity_contains_required_sections() {
        let content = default_identity();
        assert!(content.contains("# NevoFlux Identity"));
        assert!(content.contains("## Name"));
        assert!(content.contains("## Core Positioning"));
        assert!(content.contains("## Capability Boundaries"));
    }

    #[test]
    fn default_soul_contains_safety_boundaries() {
        let content = default_soul();
        assert!(content.contains("## Safety Boundaries"));
        assert!(content.contains("## Core Values"));
        assert!(content.contains("## Default Communication Style"));
    }

    #[test]
    fn all_five_templates_are_valid_markdown() {
        let templates = [
            default_identity(),
            default_soul(),
            default_user(),
            default_tools(),
            default_agents(),
        ];
        for template in &templates {
            assert!(template.starts_with("# NevoFlux"));
            assert!(template.contains("> Protection level:"));
        }
    }
}
```

**Step 2: Run test — FAIL**

**Step 3: Implement**

Create `crates/daemon/src/learning/soul/templates.rs` with five functions returning default Markdown content. Copy from the design doc (Section 5.1-5.5). Each function returns `String`.

**Step 4: Run test — PASS**

**Step 5: Commit**

```bash
git commit -m "feat(soul): add default templates for five Markdown documents"
```

---

### Task 7: Implement ChangePermission and protection level checking

**Files:**
- Create: `crates/daemon/src/learning/soul/protection.rs`
- Test: inline tests

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soul_safety_boundaries_are_forbidden() {
        assert_eq!(
            check_permission("SOUL.md", "Safety Boundaries"),
            ChangePermission::Forbidden
        );
    }

    #[test]
    fn identity_requires_double_confirm() {
        assert_eq!(
            check_permission("IDENTITY.md", "Name"),
            ChangePermission::RequireDoubleConfirm
        );
        assert_eq!(
            check_permission("IDENTITY.md", "Core Positioning"),
            ChangePermission::RequireDoubleConfirm
        );
    }

    #[test]
    fn tools_md_is_auto_with_notify() {
        assert_eq!(
            check_permission("TOOLS.md", "Site Adaptation Graph"),
            ChangePermission::AutoWithNotify
        );
        assert_eq!(
            check_permission("TOOLS.md", "Runtime Parameters"),
            ChangePermission::AutoWithNotify
        );
    }

    #[test]
    fn user_md_mixed_protection() {
        assert_eq!(
            check_permission("USER.md", "Basic Information"),
            ChangePermission::RequireConfirm
        );
        assert_eq!(
            check_permission("USER.md", "Communication Overrides"),
            ChangePermission::AutoWithNotify
        );
    }

    #[test]
    fn agents_md_mixed_protection() {
        assert_eq!(
            check_permission("AGENTS.md", "Task Execution Flow"),
            ChangePermission::RequireConfirm
        );
        assert_eq!(
            check_permission("AGENTS.md", "Multi-Task Orchestration"),
            ChangePermission::AutoWithNotify
        );
    }
}
```

**Step 2: Run test — FAIL**

**Step 3: Implement**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangePermission {
    Forbidden,
    RequireDoubleConfirm,
    RequireConfirm,
    AutoWithNotify,
}

pub fn check_permission(target: &str, section: &str) -> ChangePermission {
    match (target, section) {
        ("IDENTITY.md", _) => ChangePermission::RequireDoubleConfirm,

        ("SOUL.md", "Safety Boundaries") => ChangePermission::Forbidden,
        ("SOUL.md", "Core Values") => ChangePermission::RequireDoubleConfirm,
        ("SOUL.md", _) => ChangePermission::RequireConfirm,

        ("USER.md", "Basic Information") => ChangePermission::RequireConfirm,
        ("USER.md", "Sensitive Domain Blacklist") => ChangePermission::RequireConfirm,
        ("USER.md", _) => ChangePermission::AutoWithNotify,

        ("TOOLS.md", _) => ChangePermission::AutoWithNotify,

        ("AGENTS.md", "Task Execution Flow") => ChangePermission::RequireConfirm,
        ("AGENTS.md", "Failure Fallback Strategy") => ChangePermission::RequireConfirm,
        ("AGENTS.md", _) => ChangePermission::AutoWithNotify,

        _ => ChangePermission::RequireConfirm,
    }
}
```

**Step 4: Run test — PASS**

**Step 5: Commit**

```bash
git commit -m "feat(soul): add ChangePermission enum and check_permission function"
```

---

### Task 8: Implement SoulManager — file I/O, initialization, cache loading

**Files:**
- Create: `crates/daemon/src/learning/soul/manager.rs`
- Test: inline tests + temp directory

**Step 1: Write the failing test**

```rust
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
}
```

**Step 2: Run test — FAIL**

**Step 3: Implement**

Create `crates/daemon/src/learning/soul/manager.rs`:

```rust
use std::path::{Path, PathBuf};
use anyhow::Result;
use chrono::Utc;

use super::templates;

pub struct FiveDocCache {
    pub identity_raw: String,
    pub soul_raw: String,
    pub user_raw: String,
    pub tools_raw: String,
    pub agents_raw: String,
    pub last_parsed_at: chrono::DateTime<Utc>,
}

pub struct SoulManager {
    soul_dir: PathBuf,
    cache: FiveDocCache,
}

impl SoulManager {
    /// Initialize a new soul directory with default templates.
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
            if !path.exists() {
                tokio::fs::write(&path, content).await?;
            }
        }

        Self::load(soul_dir).await
    }

    /// Load existing soul directory into cache.
    pub async fn load(soul_dir: &Path) -> Result<Self> {
        let cache = FiveDocCache {
            identity_raw: tokio::fs::read_to_string(soul_dir.join("IDENTITY.md")).await?,
            soul_raw: tokio::fs::read_to_string(soul_dir.join("SOUL.md")).await?,
            user_raw: tokio::fs::read_to_string(soul_dir.join("USER.md")).await?,
            tools_raw: tokio::fs::read_to_string(soul_dir.join("TOOLS.md")).await?,
            agents_raw: tokio::fs::read_to_string(soul_dir.join("AGENTS.md")).await?,
            last_parsed_at: Utc::now(),
        };

        Ok(Self {
            soul_dir: soul_dir.to_path_buf(),
            cache,
        })
    }

    pub fn cache(&self) -> &FiveDocCache {
        &self.cache
    }

    pub fn soul_dir(&self) -> &Path {
        &self.soul_dir
    }
}
```

Add `tempfile` to daemon `Cargo.toml` dev-dependencies.

**Step 4: Run test — PASS**

**Step 5: Commit**

```bash
git commit -m "feat(soul): add SoulManager with init and load for five documents"
```

---

### Task 9: Implement SoulChange and atomic write with changelog

**Files:**
- Modify: `crates/daemon/src/learning/soul/manager.rs` — add `apply_change()`, `append_changelog()`
- Test: inline tests

**Step 1: Write the failing test**

```rust
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
    let content = tokio::fs::read_to_string(soul_dir.join("TOOLS.md")).await.unwrap();
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
```

**Step 2: Run test — FAIL**

**Step 3: Implement `SoulChange` struct, `apply_change()`, and `append_changelog()`**

The `apply_change()` method:
1. Checks permission via `check_permission()`
2. Creates a snapshot before modifying
3. Appends to the daily changelog file
4. Reads the target file, appends/modifies the target section
5. Atomic write via tmp file + rename
6. Reloads cache

**Step 4: Run test — PASS**

**Step 5: Commit**

```bash
git commit -m "feat(soul): add SoulChange, apply_change with permission check and changelog"
```

---

### Task 10: Implement snapshot creation and rollback

**Files:**
- Modify: `crates/daemon/src/learning/soul/manager.rs` — add `create_snapshot()`, `rollback()`, `cleanup_snapshots()`
- Test: inline tests

**Step 1: Write the failing test**

```rust
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
```

**Step 2-5: Implement, test, commit**

```bash
git commit -m "feat(soul): add snapshot creation and rollback"
```

---

### Task 11: Implement MD section parsing with pulldown-cmark

**Files:**
- Create: `crates/daemon/src/learning/soul/parser.rs`
- Modify: `crates/daemon/Cargo.toml` — add `pulldown-cmark` dependency
- Test: inline tests

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sections_from_tools_md() {
        let md = r#"# NevoFlux Tools

> Protection level: L3

## MCP Tool Inventory

Some tools here.

## Site Adaptation Graph

### github.com
- **Trust level**: trusted

### google.com
- **Trust level**: trusted

## Runtime Parameters

### Timing Parameters
- Default operation interval: 500ms
"#;

        let sections = parse_sections(md);
        assert!(sections.contains_key("MCP Tool Inventory"));
        assert!(sections.contains_key("Site Adaptation Graph"));
        assert!(sections.contains_key("Runtime Parameters"));
        assert!(sections["Site Adaptation Graph"].contains("github.com"));
    }

    #[test]
    fn parse_metadata_from_header() {
        let md = r#"# NevoFlux Soul

> Protection level: L0-L1 | Safety boundaries immutable
> Last updated: 2026-02-17T10:00:00Z

## Core Values
"#;
        let meta = parse_metadata(md);
        assert!(meta.protection_level.contains("L0-L1"));
        assert!(meta.last_updated.is_some());
    }

    #[test]
    fn insert_content_into_section() {
        let md = "# Doc\n\n## Section A\n\nContent A\n\n## Section B\n\nContent B\n";
        let result = insert_into_section(md, "Section A", "\nNew content\n");
        assert!(result.contains("Content A"));
        assert!(result.contains("New content"));
        assert!(result.contains("Section B"));
    }
}
```

**Step 2-5: Implement, test, commit**

Use `pulldown-cmark` to parse the AST, identify heading events to split into sections. Custom logic extracts metadata from blockquote lines and key-value pairs from list items.

```bash
git commit -m "feat(soul): add MD section parser with pulldown-cmark"
```

---

## Phase 2: Learning Pipeline

> Build end-to-end pipeline from events to validated knowledge in SQLite.

### Task 12: Add SQLite migration 005_learning.sql

**Files:**
- Create: `crates/storage/src/migrations/005_learning.sql`
- Modify: `crates/storage/src/migrations.rs` — add migration entry
- Test: `crates/storage/tests/integration.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn migration_005_creates_learning_tables() {
    let storage = Storage::open_in_memory().unwrap();
    storage.database().with_connection(|conn| {
        // Verify tables exist
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='knowledge'",
            [], |row| row.get(0),
        )?;
        assert_eq!(count, 1);

        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='site_adaptations'",
            [], |row| row.get(0),
        )?;
        assert_eq!(count, 1);

        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tool_stats'",
            [], |row| row.get(0),
        )?;
        assert_eq!(count, 1);

        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='learning_metrics'",
            [], |row| row.get(0),
        )?;
        assert_eq!(count, 1);

        Ok(())
    }).unwrap();
}
```

**Step 2: Run test — FAIL**

**Step 3: Implement**

Create `crates/storage/src/migrations/005_learning.sql` with the exact schema from design doc Section 4.3. Add migration to `MIGRATIONS` array in `migrations.rs`.

**Step 4: Run test — PASS**

**Step 5: Commit**

```bash
git commit -m "feat(storage): add migration 005_learning with knowledge, site_adaptations, tool_stats, learning_metrics tables"
```

---

### Task 13: Create Knowledge model and KnowledgeRepository

**Files:**
- Create: `crates/storage/src/models/knowledge.rs`
- Create: `crates/storage/src/repositories/knowledge.rs`
- Modify: `crates/storage/src/models/mod.rs` — add export
- Modify: `crates/storage/src/repositories/mod.rs` — add export
- Modify: `crates/storage/src/storage.rs` — add `knowledge()` method
- Test: inline tests + integration tests

**Step 1: Write the failing test**

```rust
#[test]
fn knowledge_crud_lifecycle() {
    let storage = Storage::open_in_memory().unwrap();

    // Create
    let created = storage.knowledge().create(CreateKnowledgeParams {
        category: "site_interaction".into(),
        subcategory: Some("selector_result".into()),
        domain: Some("github.com".into()),
        summary: "github.com uses data-testid selectors".into(),
        details: "Verified across 10 pages".into(),
        ..Default::default()
    }).unwrap();

    assert!(created.id.starts_with("K-"));
    assert_eq!(created.status, "pending");
    assert_eq!(created.confidence, 0.5);

    // Read
    let found = storage.knowledge().get(&created.id).unwrap().unwrap();
    assert_eq!(found.summary, created.summary);

    // Update status
    storage.knowledge().update_status(&created.id, "validated").unwrap();
    let updated = storage.knowledge().get(&created.id).unwrap().unwrap();
    assert_eq!(updated.status, "validated");

    // Query by domain
    let results = storage.knowledge().query_by_domain("github.com", 5).unwrap();
    assert_eq!(results.len(), 1);

    // Delete
    storage.knowledge().delete(&created.id).unwrap();
    assert!(storage.knowledge().get(&created.id).unwrap().is_none());
}
```

**Step 2-5: Implement, test, commit**

Follow the existing repository pattern: `KnowledgeRepository<'a>` with `db: &'a Database`, CRUD methods using `with_connection()`.

```bash
git commit -m "feat(storage): add Knowledge model and KnowledgeRepository"
```

---

### Task 14: Create SiteAdaptation and ToolStats repositories

**Files:**
- Create: `crates/storage/src/models/site_adaptation.rs`
- Create: `crates/storage/src/models/tool_stat.rs`
- Create: `crates/storage/src/repositories/site_adaptation.rs`
- Create: `crates/storage/src/repositories/tool_stat.rs`
- Create: `crates/storage/src/repositories/learning_metrics.rs`
- Modify: `crates/storage/src/storage.rs` — add accessor methods

**Steps: TDD for each repository — create, get, query, update, delete**

```bash
git commit -m "feat(storage): add SiteAdaptation, ToolStats, and LearningMetrics repositories"
```

---

### Task 15: Implement DashMap MemoryBuffer with flush to SQLite

**Files:**
- Create: `crates/daemon/src/learning/buffer.rs`
- Modify: `crates/daemon/Cargo.toml` — add `dashmap` dependency
- Test: inline tests

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning::types::*;

    #[test]
    fn buffer_inserts_and_retrieves() {
        let buffer = MemoryBuffer::new(20, Duration::from_secs(30));
        let entry = LearningEntry::new(LearningCategory::SiteInteraction, "test", "test");
        let id = entry.id.clone();

        buffer.insert(entry);
        assert_eq!(buffer.len(), 1);

        let drained = buffer.drain_all();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, id);
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn buffer_signals_flush_at_threshold() {
        let buffer = MemoryBuffer::new(3, Duration::from_secs(30));
        for i in 0..3 {
            buffer.insert(LearningEntry::new(
                LearningCategory::SiteInteraction,
                &format!("event-{}", i),
                "summary",
            ));
        }
        assert!(buffer.should_flush());
    }

    #[test]
    fn buffer_merges_duplicate_entries() {
        let buffer = MemoryBuffer::new(20, Duration::from_secs(30));

        let entry1 = LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed")
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                selector: Some(".btn".into()),
                ..Default::default()
            });

        let entry2 = LearningEntry::new(LearningCategory::SiteInteraction, "click", "click failed")
            .with_context(LearningContext {
                domain: Some("example.com".into()),
                selector: Some(".btn".into()),
                ..Default::default()
            });

        buffer.insert(entry1);
        buffer.insert(entry2);

        // Should merge into one entry with occurrence_count=2
        assert_eq!(buffer.len(), 1);
        let entries = buffer.drain_all();
        assert_eq!(entries[0].occurrence_count, 2);
    }
}
```

**Step 2-5: Implement, test, commit**

```rust
pub struct MemoryBuffer {
    entries: DashMap<String, LearningEntry>,
    flush_threshold: usize,
    flush_interval: Duration,
    last_flush: Mutex<Instant>,
}
```

```bash
git commit -m "feat(learning): add DashMap MemoryBuffer with flush threshold and dedup"
```

---

### Task 16: Implement flush pipeline (MemoryBuffer → SQLite)

**Files:**
- Create: `crates/daemon/src/learning/pipeline.rs`
- Test: inline tests with in-memory SQLite

**Steps:** Create `LearningPipeline` struct that holds a `MemoryBuffer` and `Storage` reference. Implement `flush()` method that drains the buffer and inserts into SQLite knowledge table. Implement `run_background()` that spawns a tokio task running flush on interval.

```bash
git commit -m "feat(learning): add LearningPipeline with buffer-to-SQLite flush"
```

---

### Task 17: Implement validation pipeline (pending → validated)

**Files:**
- Modify: `crates/daemon/src/learning/pipeline.rs` — add `validate()` method
- Test: inline tests

**Steps:** Query pending entries from SQLite. Check against configurable thresholds (min_alive_hours, min_occurrences, min_confidence). Update status to `validated` for qualifying entries.

```bash
git commit -m "feat(learning): add validation pipeline with configurable thresholds"
```

---

### Task 18: Implement promotion pipeline (validated → MD files)

**Files:**
- Modify: `crates/daemon/src/learning/pipeline.rs` — add `promote()` method
- Create: `crates/daemon/src/learning/routing.rs` — knowledge category → document routing
- Test: integration test with temp directory + in-memory SQLite

**Steps:**
1. Create `route_knowledge()` function mapping category/subcategory → (target file, section)
2. In `promote()`: query validated entries meeting promotion thresholds
3. Check conflicts with existing MD content
4. Check manual-edit priority
5. Call `SoulManager::apply_change()` for qualifying entries
6. Update SQLite status to `promoted`

```bash
git commit -m "feat(learning): add promotion pipeline with knowledge routing"
```

---

## Phase 3: Intelligence Layer

> Build retrieval, decay, and conflict resolution.

### Task 19: Implement KnowledgeRetriever with session-scoped cache

**Files:**
- Create: `crates/daemon/src/learning/retriever.rs`
- Test: inline tests

**Steps:** Create `KnowledgeRetriever` struct holding `Arc<FiveDocCache>` and `Storage` ref. Implement `retrieve_context()` that returns `AgentContext`. Session-scoped: loaded once at session start, invalidated by SoulManager file watcher.

```bash
git commit -m "feat(learning): add KnowledgeRetriever with session-scoped cache"
```

---

### Task 20: Implement lazy decay calculation

**Files:**
- Create: `crates/daemon/src/learning/decay.rs`
- Test: inline tests with proptest

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn fresh_entry_has_full_decay_score() {
        let score = calculate_decay(Utc::now(), "site_interaction", 0.8, 10);
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn old_entry_decays_significantly() {
        let old = Utc::now() - chrono::Duration::days(90);
        let score = calculate_decay(old, "site_interaction", 0.5, 5);
        assert!(score < 0.2);
    }

    #[test]
    fn high_effectiveness_extends_halflife() {
        let date = Utc::now() - chrono::Duration::days(30);
        let low_eff = calculate_decay(date, "site_interaction", 0.1, 1);
        let high_eff = calculate_decay(date, "site_interaction", 0.9, 50);
        assert!(high_eff > low_eff);
    }

    proptest! {
        #[test]
        fn decay_always_between_0_and_1(
            days in 0u32..3650,
            effectiveness in 0.0f64..1.0,
            hit_count in 1u32..10000,
        ) {
            let date = Utc::now() - chrono::Duration::days(days as i64);
            let score = calculate_decay(date, "site_interaction", effectiveness, hit_count);
            prop_assert!(score >= 0.0);
            prop_assert!(score <= 1.0);
        }
    }
}
```

**Step 2-5: Implement the formula from design doc Section 10.1, test, commit**

```bash
git commit -m "feat(learning): add lazy decay calculation with category-specific half-lives"
```

---

### Task 21: Implement resurrection mechanism

**Files:**
- Modify: `crates/daemon/src/learning/pipeline.rs` — add `resurrect()` method
- Test: inline test

**Steps:** When a query hits an archived entry, reset status to `validated`, update `last_hit_at`, increment `hit_count`.

```bash
git commit -m "feat(learning): add knowledge resurrection for archived entries"
```

---

### Task 22: Implement conflict resolution

**Files:**
- Create: `crates/daemon/src/learning/conflict.rs`
- Test: inline tests

**Steps:** Implement `detect_conflict()` and `resolve_conflict()` covering 4 types: DirectContradiction, StrategyConflict, TemporalConflict, ScopeConflict. Include manual-edit priority check.

```bash
git commit -m "feat(learning): add conflict detection and resolution with manual-edit priority"
```

---

### Task 23: Implement relevance scoring for retrieval

**Files:**
- Modify: `crates/daemon/src/learning/retriever.rs` — add `relevance_score()` function
- Test: inline tests

**Steps:** Composite score: `category_match * domain_match * decay_score * confidence`. Sort results by score descending.

```bash
git commit -m "feat(learning): add composite relevance scoring for knowledge retrieval"
```

---

### Task 24: Wire KnowledgeRetriever into AgentRunner

**Files:**
- Modify: `crates/daemon/src/agent/runner.rs` — add `KnowledgeRetriever` field
- Modify: `crates/daemon/src/agent_host.rs` — inject retriever into HostServices

**Steps:** At session start, create `KnowledgeRetriever` and load cache. Before each tool execution, call `retrieve_context()` to get relevant knowledge. Add knowledge to agent context.

```bash
git commit -m "feat(agent): wire KnowledgeRetriever into AgentRunner execution loop"
```

---

### Task 25: Add learning_metrics recording

**Files:**
- Modify: `crates/daemon/src/learning/pipeline.rs` — add metrics recording after flush, validation, promotion
- Test: inline tests

**Steps:** Record daily metrics: flush count, validation rate, promotion rate, knowledge hit counts.

```bash
git commit -m "feat(learning): add learning_metrics recording for effectiveness tracking"
```

---

## Phase 4: User Experience & Privacy

> Add encryption, privacy controls, and user-facing features.

### Task 26: Add OS keychain integration for encryption key

**Files:**
- Create: `crates/daemon/src/learning/crypto.rs`
- Modify: `crates/daemon/Cargo.toml` — add `keyring`, `aes-gcm` dependencies

**Steps:** Use `keyring` crate to store/retrieve AES-256-GCM key. Generate key on first use. Encrypt/decrypt functions for sensitive data.

```bash
git commit -m "feat(learning): add OS keychain integration for AES-256-GCM encryption"
```

---

### Task 27: Encrypt sensitive SQLite rows and USER.md

**Files:**
- Modify: `crates/storage/src/repositories/knowledge.rs` — encrypt/decrypt sensitive rows
- Modify: `crates/daemon/src/learning/soul/manager.rs` — encrypt USER.md at rest

**Steps:** Before writing sensitive data to SQLite, encrypt with AES-256-GCM. Decrypt on read. USER.md stored encrypted, decrypted into cache.

```bash
git commit -m "feat(learning): encrypt sensitive knowledge rows and USER.md at rest"
```

---

### Task 28: Add export functionality

**Files:**
- Create: `crates/daemon/src/learning/export.rs`

**Steps:** Export public knowledge directly. Anonymize internal knowledge (domains → SHA-256 hashes). Never export sensitive/private.

```bash
git commit -m "feat(learning): add knowledge export with anonymization"
```

---

### Task 29: Add learning system controls (pause/resume/clear)

**Files:**
- Modify: `crates/daemon/src/learning/pipeline.rs` — add pause/resume/clear methods
- Modify: `crates/daemon/src/learning/collector.rs` — check enabled flag

**Steps:** Add `enabled: AtomicBool` to `LearningPipeline`. `pause()` sets false, `resume()` sets true. `clear_all()` drops SQLite tables and removes soul/ directory. Collector checks enabled before collecting.

```bash
git commit -m "feat(learning): add pause/resume/clear controls for learning system"
```

---

### Task 30: Add FileWatcher for soul/ directory

**Files:**
- Modify: `crates/daemon/src/learning/soul/manager.rs` — add `notify::RecommendedWatcher`
- Modify: `crates/daemon/Cargo.toml` — add `notify` dependency

**Steps:** Watch soul/ directory for external changes. Debounce 500ms. On change: validate format, reload cache, log to changelog.

```bash
git commit -m "feat(soul): add FileWatcher with debounce for external edit detection"
```

---

### Task 31: Full CI verification

**Steps:**

```bash
just ci    # cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace
```

Fix any issues. Final commit.

```bash
git commit -m "chore: fix clippy warnings and fmt issues for learning system"
```

---

## Dependency Graph

```
Task 1  (types)
  ↓
Task 2  (source trait) ──→ Task 5 (instrument tools)
  ↓
Task 3  (collector)
  ↓
Task 4  (config) ─────────────────────────────→ Task 17 (validation thresholds)
  ↓
Task 6  (templates) → Task 7 (protection) → Task 8 (manager) → Task 9 (changes) → Task 10 (snapshots)
  ↓                                                                ↓
Task 11 (parser)                                            Task 18 (promotion)
  ↓                                                                ↓
Task 12 (migration) → Task 13 (knowledge repo) → Task 14 (site/tool repos) → Task 15 (buffer)
                                                                                    ↓
                                                                              Task 16 (flush)
                                                                                    ↓
                                                                              Task 17 (validate)
                                                                                    ↓
Task 20 (decay) → Task 19 (retriever) → Task 23 (scoring) → Task 24 (wire to agent)
  ↓
Task 21 (resurrect)
  ↓
Task 22 (conflict) → Task 18 (promotion)
  ↓
Task 25 (metrics)
  ↓
Task 26 (keychain) → Task 27 (encrypt) → Task 28 (export)
  ↓
Task 29 (controls) → Task 30 (watcher) → Task 31 (CI)
```

---

## Key Files Reference

| Component | File Path |
|-----------|-----------|
| Learning types | `crates/daemon/src/learning/types.rs` |
| LearningSource trait | `crates/daemon/src/learning/source.rs` |
| LearningCollector | `crates/daemon/src/learning/collector.rs` |
| MemoryBuffer | `crates/daemon/src/learning/buffer.rs` |
| LearningPipeline | `crates/daemon/src/learning/pipeline.rs` |
| KnowledgeRetriever | `crates/daemon/src/learning/retriever.rs` |
| Decay calculation | `crates/daemon/src/learning/decay.rs` |
| Conflict resolution | `crates/daemon/src/learning/conflict.rs` |
| Knowledge routing | `crates/daemon/src/learning/routing.rs` |
| SoulManager | `crates/daemon/src/learning/soul/manager.rs` |
| MD parser | `crates/daemon/src/learning/soul/parser.rs` |
| Protection levels | `crates/daemon/src/learning/soul/protection.rs` |
| Templates | `crates/daemon/src/learning/soul/templates.rs` |
| Crypto | `crates/daemon/src/learning/crypto.rs` |
| Export | `crates/daemon/src/learning/export.rs` |
| SQLite migration | `crates/storage/src/migrations/005_learning.sql` |
| Knowledge model | `crates/storage/src/models/knowledge.rs` |
| Knowledge repo | `crates/storage/src/repositories/knowledge.rs` |
| Site adaptation repo | `crates/storage/src/repositories/site_adaptation.rs` |
| Tool stats repo | `crates/storage/src/repositories/tool_stat.rs` |
| Metrics repo | `crates/storage/src/repositories/learning_metrics.rs` |
| Config | `crates/daemon/src/config.rs` (add `LearningConfig`) |
| Agent runner | `crates/daemon/src/agent/runner.rs` (wire retriever) |
| Agent host | `crates/daemon/src/agent_host.rs` (inject retriever) |
