//! Models for the /loop skill (spec §6.1).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopState {
    Pending,
    Running,
    Idle,
    Failed,
    Cancelled,
}

impl LoopState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "idle" => Self::Idle,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRecord {
    pub id: String,
    pub session_id: String,
    pub trigger_expr: String,
    pub prompt_text: Option<String>,
    pub wrapped_skill: Option<String>,
    pub allowed_tool_classes: Vec<String>,
    pub scratchpad: String,
    pub state: LoopState,
    pub consecutive_failures: i64,
    pub skipped_triggers: i64,
    pub iteration_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IterationStatus {
    Running,
    Ok,
    Error,
    Skipped,
    Cancelled,
}

impl IterationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopIteration {
    pub id: i64,
    pub loop_id: String,
    pub sequence_number: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: IterationStatus,
    pub error_message: Option<String>,
    pub tool_calls_json: Option<String>,
}
