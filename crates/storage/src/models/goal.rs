//! Goal (session-scoped success-condition) models.

use serde::{Deserialize, Serialize};

/// Lifecycle state of a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Achieved,
    /// Turn budget (`max_turns`) exhausted without the condition being met.
    Expired,
    /// Superseded by a newer goal for the same session, or explicitly cancelled.
    Cleared,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Achieved => "achieved",
            Self::Expired => "expired",
            Self::Cleared => "cleared",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "achieved" => Some(Self::Achieved),
            "expired" => Some(Self::Expired),
            "cleared" => Some(Self::Cleared),
            _ => None,
        }
    }
}

/// A persisted goal row. See migration 022 for column semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRecord {
    pub id: String,
    pub session_id: String,
    pub condition: String,
    pub evaluator_provider: Option<String>,
    pub evaluator_model: Option<String>,
    pub max_turns: i64,
    pub turns_used: i64,
    pub status: GoalStatus,
    pub last_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub achieved_at: Option<i64>,
}
