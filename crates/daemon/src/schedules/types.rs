//! Core schedule types.

use crate::loops::types::generate_short_id;

/// Unique 8-char schedule id (same alphabet/length as `LoopId`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScheduleId(pub String);

impl ScheduleId {
    pub fn generate() -> Self {
        Self(generate_short_id())
    }
}

impl std::fmt::Display for ScheduleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_id_is_8_chars_alphanumeric() {
        let id = ScheduleId::generate();
        assert_eq!(id.0.len(), 8);
        assert!(id.0.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn generated_ids_differ() {
        let a = ScheduleId::generate();
        let b = ScheduleId::generate();
        assert_ne!(a, b, "two consecutive UUID-derived ids should not collide");
    }

    #[test]
    fn display_matches_inner_string() {
        let id = ScheduleId("abcd1234".into());
        assert_eq!(id.to_string(), "abcd1234");
    }
}
