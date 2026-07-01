//! Transport-agnostic task contract (P4), shared by the HTTP / MCP / CLI
//! front-ends. `TaskRequest` is the public API surface — add fields additively.

use serde::{Deserialize, Serialize};

/// Per-task capability opt-ins (maps to [`crate::automation::policy::Policy`]).
#[derive(Debug, Clone, Deserialize)]
pub struct PolicyRequest {
    /// Admit shell tools (`run_command`, `bash`).
    #[serde(default)]
    pub allow_shell: bool,
    /// Admit filesystem-write tools.
    #[serde(default)]
    pub allow_fs_write: bool,
    /// Admit `uploadFile`.
    #[serde(default)]
    pub allow_upload: bool,
    /// Restrict `navigate`/`web_fetch` to these domains (empty = any).
    #[serde(default)]
    pub domain_allowlist: Vec<String>,
}

impl Default for PolicyRequest {
    fn default() -> Self {
        Self {
            allow_shell: false,
            allow_fs_write: false,
            allow_upload: false,
            domain_allowlist: Vec::new(),
        }
    }
}

/// A submitted automation task.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskRequest {
    /// The instruction for the agent.
    pub task: String,
    /// Agent mode (default `browser`).
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Named base-profile to clone (login state); `None` = blank base.
    #[serde(default)]
    pub profile: Option<String>,
    /// Capability opt-ins.
    #[serde(default)]
    pub policy: PolicyRequest,
    /// Per-task wall-clock deadline (seconds).
    #[serde(default)]
    pub wall_clock_secs: Option<u64>,
    /// Per-task token-spend budget.
    #[serde(default)]
    pub token_budget: Option<u64>,
    /// Retry even after a mutating tool ran (caller asserts idempotency).
    #[serde(default)]
    pub idempotent: bool,
    /// Disable auto-retry entirely.
    #[serde(default)]
    pub no_retry: bool,
}

fn default_mode() -> String {
    "browser".to_string()
}

impl TaskRequest {
    /// Build the automation [`Policy`](crate::automation::policy::Policy) from this request.
    pub fn to_policy(&self) -> crate::automation::policy::Policy {
        crate::automation::policy::Policy {
            allow_shell: self.policy.allow_shell,
            allow_fs_write: self.policy.allow_fs_write,
            allow_upload: self.policy.allow_upload,
            domain_allowlist: self.policy.domain_allowlist.clone(),
            idempotent: self.idempotent,
            no_retry: self.no_retry,
        }
    }
}

/// Lifecycle status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Accepted, awaiting execution.
    Queued,
    /// Executing.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Failed (after retries / caps / cancel).
    Failed,
}

/// Task result / status snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct TaskResponse {
    /// Task id.
    pub id: String,
    /// Current status.
    pub status: TaskStatus,
    /// Attempt count (1 + retries).
    pub attempts: u32,
    /// Final agent output, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Error detail, if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Drained artifact paths (relative to the task workspace).
    pub artifacts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_request_deserializes_with_policy() {
        let json = r#"{"task":"open example.com","policy":{"allow_shell":true}}"#;
        let req: TaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.task, "open example.com");
        assert_eq!(req.mode, "browser"); // default
        assert!(req.policy.allow_shell);
        let p = req.to_policy();
        assert!(p.allow_shell);
        assert!(!p.allow_fs_write);
    }

    #[test]
    fn task_response_serializes_snake_case() {
        let r = TaskResponse {
            id: "t1".into(),
            status: TaskStatus::Running,
            attempts: 1,
            output: None,
            error: None,
            artifacts: vec![],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""status":"running""#));
        assert!(s.contains(r#""id":"t1""#));
        // output/error omitted when None
        assert!(!s.contains("output"));
    }
}
