//! Pure taint classification for the retry gate (P3).
//!
//! Read-only browser tools do not taint an attempt; the first *mutating* tool
//! (one that can cause a real-world side effect) taints it. A tainted attempt
//! that fails is not auto-retried (unless the task is declared idempotent),
//! so we never re-fire a side effect like "submit form" or "run_command".

/// Tools that can cause real-world side effects. Conservative: when unsure, taint.
pub fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "click"
            | "clickAtCoordinates"
            | "type"
            | "fill"
            | "fillRichText"
            | "paste"
            | "uploadFile"
            | "run_command"
            | "bash"
            | "write_file"
            | "write"
            | "edit"
            | "submit"
    )
}

/// Tracks whether the current attempt has dispatched a mutating tool.
#[derive(Debug, Default, Clone)]
pub struct TaintState {
    tainted: bool,
}

impl TaintState {
    /// Observe a dispatched tool; taints the attempt if it is mutating.
    pub fn observe(&mut self, tool: &str) {
        if is_mutating_tool(tool) {
            self.tainted = true;
        }
    }

    /// Whether a mutating tool has been dispatched this attempt.
    pub fn is_tainted(&self) -> bool {
        self.tainted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taint_marks_on_first_mutating_tool() {
        assert!(!is_mutating_tool("navigate"));
        assert!(!is_mutating_tool("get_content"));
        assert!(is_mutating_tool("click"));
        assert!(is_mutating_tool("run_command"));
        let mut t = TaintState::default();
        t.observe("navigate");
        t.observe("snapshot");
        assert!(!t.is_tainted());
        t.observe("click");
        assert!(t.is_tainted());
        t.observe("navigate"); // stays tainted
        assert!(t.is_tainted());
    }
}
