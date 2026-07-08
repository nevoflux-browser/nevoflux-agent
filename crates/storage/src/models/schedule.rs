//! Schedule (routines-style background job) models.

use serde::{Deserialize, Serialize};

/// Lifecycle state of a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Active,
    Paused,
    /// One-off schedule that has fired (auto-disabled).
    Ran,
    Cancelled,
}

impl ScheduleStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Ran => "ran",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "ran" => Some(Self::Ran),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Outcome of a single schedule run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleRunStatus {
    Running,
    Ok,
    Error,
    /// Fire time passed while the daemon was not running.
    Missed,
    /// Skipped by policy (e.g. live browser unavailable, on_unavailable=skip).
    Skipped,
    /// Waiting for the live browser to come back (on_unavailable=defer).
    Deferred,
    Cancelled,
}

impl ScheduleRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Missed => "missed",
            Self::Skipped => "skipped",
            Self::Deferred => "deferred",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "ok" => Some(Self::Ok),
            "error" => Some(Self::Error),
            "missed" => Some(Self::Missed),
            "skipped" => Some(Self::Skipped),
            "deferred" => Some(Self::Deferred),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// A persisted schedule row. See migration 021 for column semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleRecord {
    pub id: String,
    pub creator_session_id: Option<String>,
    pub name: String,
    pub cron_expr: Option<String>,
    pub at_ts: Option<i64>,
    pub prompt_text: Option<String>,
    pub wrapped_skill: Option<String>,
    pub mode: String,
    pub browser_policy: String,
    pub on_unavailable: Option<String>,
    pub headless_profile: Option<String>,
    pub catch_up: bool,
    pub goal_condition: Option<String>,
    pub goal_max_turns: Option<i64>,
    pub max_tokens_per_run: Option<i64>,
    pub evaluator_model: Option<String>,
    pub status: ScheduleStatus,
    pub next_fire_at: Option<i64>,
    pub last_run_status: Option<String>,
    pub last_run_at: Option<i64>,
    pub consecutive_failures: i64,
    pub run_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A single run (execution) of a schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleRun {
    pub id: i64,
    pub schedule_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: ScheduleRunStatus,
    pub fire_kind: String,
    pub error_message: Option<String>,
    pub final_text: Option<String>,
    pub tokens_used: Option<i64>,
    pub goal_turns: Option<i64>,
}
