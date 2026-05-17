//! NevoFlux evaluation harness.
//!
//! Architecture:
//! - [`benchmarks`]   — adapters for external benchmarks (BrowseComp, Online-Mind2Web, ...)
//!                      and the in-tree NevoFlux self-suite (YAML-driven).
//! - [`judge`]        — pluggable evaluators (programmatic, LLM-as-judge, privacy audit).
//! - [`metrics`]      — accuracy, cost-normalized accuracy, latency.
//! - [`runner`]       — orchestrates a benchmark run against a NevoFlux daemon.
//! - [`reporter`]     — emits Markdown / JSON reports.
//!
//! Reuses workspace infrastructure:
//! - `nevoflux-daemon-client` for TCP IPC at 127.0.0.1:19500
//! - `nevoflux-protocol` for envelope types
//! - `tracing` for structured logs that align with `traces` SQLite table
//!
//! See `eval/README.md` for run instructions and `crates/eval/tests/` for examples.

pub mod benchmarks;
pub mod browser;
pub mod daemon_client;
pub mod datasets;
pub mod error;
pub mod judge;
pub mod metrics;
pub mod network_audit;
pub mod reporter;
pub mod runner;
pub mod termination;

pub use browser::{BrowserHandle, BrowserLaunchMode};
pub use daemon_client::{DaemonHttpClient, DaemonLock};
pub use error::{EvalError, EvalResult};
pub use runner::{Runner, RunnerConfig};
pub use termination::{AnswerExtractor, DaemonEvent, TerminationDecision, TerminationStrategy};

/// The host platform the eval runner is executing on.
///
/// Used by `Task::supports_platform` to skip tasks that cannot run on the
/// current OS (e.g. Windows-only browser automation, macOS-only keychain
/// tests).  An empty `supports_platform` means "all platforms" for
/// backwards compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    Macos,
    Windows,
}

impl Platform {
    /// Detect the current compile-time target platform.
    pub fn current() -> Self {
        #[cfg(target_os = "linux")]
        return Platform::Linux;
        #[cfg(target_os = "macos")]
        return Platform::Macos;
        #[cfg(target_os = "windows")]
        return Platform::Windows;
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        return Platform::Linux; // fallback for other unix-likes
    }
}

#[allow(dead_code)]
fn default_all_platforms() -> Vec<Platform> {
    vec![Platform::Linux, Platform::Macos, Platform::Windows]
}

/// Signal grade — drives report routing.
///
/// - `Authoritative` reports go to `eval/reports/authoritative/` and are
///   committed to git. Only produced when the browser came from a published
///   nevoflux release binary (so the score reflects what users actually run).
/// - `Exploratory` reports go to `eval/reports/exploratory/` and are gitignored.
///   Produced from `--browser-mode external` (dev iteration) or `daemon-only`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SignalGrade {
    Exploratory,
    Authoritative,
}

impl SignalGrade {
    pub fn subdir(self) -> &'static str {
        match self {
            SignalGrade::Authoritative => "authoritative",
            SignalGrade::Exploratory => "exploratory",
        }
    }
}

/// A single evaluation task — the unit of work executed by the runner.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Task {
    pub id: String,
    pub category: String,
    pub mode: NevoFluxMode,
    pub prompt: String,
    /// Optional setup steps run before the task (e.g. inject prior session messages).
    #[serde(default)]
    pub setup: Vec<SetupStep>,
    /// Optional reference answer for programmatic judging.
    #[serde(default)]
    pub reference: Option<String>,
    /// Assertions used by structured judges (e.g. NevoFlux self-suite).
    #[serde(default)]
    pub assertions: Vec<Assertion>,
    /// Whether this task requires a real browser to execute meaningfully.
    /// `false`  → daemon-only execution is sufficient (memory, mode-authz, mcp-bidir).
    /// `true`   → MUST run in `external` or `release` browser mode; runner skips
    ///            with `BrowserUnavailable` if `BrowserLaunchMode::DaemonOnly`.
    #[serde(default)]
    pub requires_browser: bool,
    /// Free-form metadata for benchmark-specific fields.
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// Platforms this task can run on.  An empty Vec means "all platforms"
    /// (backwards-compatible default).  The runner skips tasks whose
    /// `supports_platform` list does not contain the host platform.
    /// Phase 3d addition.
    #[serde(default)]
    pub supports_platform: Vec<Platform>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NevoFluxMode {
    Chat,
    Browser,
    Agent,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SetupStep {
    /// Inject a prior message into a labeled session.
    InjectMessage {
        session: String,
        role: String,
        content: String,
    },
    /// Pre-create a memory entry.
    SeedMemory { content: String },
    /// Pre-grant a permission scope.
    GrantPermission { resource: String, action: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Assertion {
    /// Final answer must equal one of the listed strings (case-insensitive).
    EqualsAny { targets: Vec<String> },
    /// Output must contain at least one of these substrings.
    ContainsAny { targets: Vec<String> },
    /// Output must NOT contain any of these substrings.
    NotContains { targets: Vec<String> },
    /// Final answer must match the regex.
    Regex { pattern: String },
    /// A daemon-side state assertion (e.g. permission-denied event).
    DaemonEvent { event: String },
    /// Privacy invariant: no outbound traffic to disallowed hosts.
    NoOutboundTo { hosts: Vec<String> },
}

/// Result of executing a single [`Task`] against a NevoFlux daemon.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskResult {
    pub task_id: String,
    pub status: TaskStatus,
    pub final_answer: Option<String>,
    pub latency_ms: u64,
    pub token_cost: Option<TokenCost>,
    pub error: Option<String>,
    /// References to traces in the daemon's `traces` table for post-mortem.
    pub trace_ids: Vec<String>,
    /// Names of daemon_event spans observed during this task run.
    /// Populated by Runner from the /traces endpoint after task completion.
    /// Phase 3a addition (defaults to empty for backwards compat).
    #[serde(default)]
    pub observed_events: Vec<String>,
    /// Outbound hosts observed via tcpdump during this task.  Populated by
    /// the privacy-audit runner hook.  Empty when not under privacy audit.
    /// Phase 3d addition.
    #[serde(default)]
    pub outbound_hosts: Vec<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task ran to completion (may still be judged incorrect).
    Completed,
    /// Task could not run because of an execution error.
    Failed,
    /// Task hit the runner timeout.
    Timeout,
    /// Task requires a browser but runner is in DaemonOnly mode.
    /// Does NOT count against accuracy — task is excluded from denominator.
    SkippedNoBrowser,
}

impl TaskStatus {
    pub fn is_skipped(self) -> bool {
        matches!(self, TaskStatus::SkippedNoBrowser)
    }
    pub fn ran_to_completion(self) -> bool {
        matches!(self, TaskStatus::Completed)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenCost {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    #[test]
    fn platform_current_returns_compile_target() {
        let p = Platform::current();
        #[cfg(target_os = "linux")]
        assert_eq!(p, Platform::Linux);
        #[cfg(target_os = "macos")]
        assert_eq!(p, Platform::Macos);
        #[cfg(target_os = "windows")]
        assert_eq!(p, Platform::Windows);
    }

    #[test]
    fn task_default_supports_empty_vec() {
        let task: Task = serde_yaml::from_str(
            "id: t\ncategory: c\nmode: chat\nprompt: p\nrequires_browser: false\n",
        )
        .unwrap();
        // serde default for Vec is empty — that is our "all platforms" sentinel.
        assert!(task.supports_platform.is_empty());
    }

    #[test]
    fn task_with_explicit_platforms_parses() {
        let task: Task = serde_yaml::from_str(
            "id: t\ncategory: c\nmode: chat\nprompt: p\nrequires_browser: false\nsupports_platform: [linux, macos]\n",
        )
        .unwrap();
        assert_eq!(task.supports_platform.len(), 2);
        assert!(task.supports_platform.contains(&Platform::Linux));
        assert!(task.supports_platform.contains(&Platform::Macos));
    }
}
