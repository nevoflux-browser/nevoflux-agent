//! Screenshot-tier capture decision (P6): when to auto-capture a screenshot +
//! DOM snapshot during a headless run, for observability without flooding disk.

/// A capture tier. The active set is the union of enabled tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotTier {
    /// Capture on every tool.
    Always,
    /// Capture when a tool errored.
    OnError,
    /// Capture after key navigation/interaction tools.
    KeyActions,
}

/// Decide whether to capture given the active tiers, the tool name, and whether
/// the tool errored. Default set (`[OnError, KeyActions]`) captures on errors and
/// after navigate/click, but not on quiet read-only steps.
pub fn should_capture(tier_set: &[ShotTier], tool: &str, is_error: bool) -> bool {
    tier_set.iter().any(|t| match t {
        ShotTier::Always => true,
        ShotTier::OnError => is_error,
        ShotTier::KeyActions => matches!(tool, "navigate" | "click" | "clickAtCoordinates"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_tiers() {
        let d = &[ShotTier::OnError, ShotTier::KeyActions];
        assert!(should_capture(d, "navigate", false)); // key action
        assert!(should_capture(d, "click", false)); // key action
        assert!(!should_capture(d, "get_content", false)); // not key, not error
        assert!(should_capture(d, "get_content", true)); // error → capture
        assert!(should_capture(&[ShotTier::Always], "get_content", false));
        assert!(!should_capture(&[ShotTier::OnError], "navigate", false)); // on-error only
    }
}
