//! Iteration-specific tool guards.
//!
//! Historical note: this module used to maintain a parallel "tool class"
//! taxonomy (Read / Write / DomClick / Nav / etc.) for /loop iterations.
//! That taxonomy used fictitious tool names that didn't match the real
//! `builtin-wasm::Agent::get_chat_tools()` catalog, so iterations ended
//! up with empty allowlists (no `browser_query` etc.).
//!
//! Migration 018 replaced the class system with the existing
//! `AgentMode { Chat, Browser, Agent }` enum from `builtin-wasm`, and
//! the iteration's tool catalog now comes directly from
//! `Agent::get_tools_for_mode(mode)`. The only iteration-specific filter
//! left is [`is_forbidden_in_iteration`].

/// Single source of truth for the iteration-forbidden tool names.
/// `loop_create` would let an iteration spawn nested loops; `ask_user` blocks
/// on a sidebar that may be closed.
const ITERATION_FORBIDDEN: &[&str] = &["loop_create", "ask_user"];

/// Tools that are forbidden inside loop iterations regardless of mode.
pub fn is_forbidden_in_iteration(tool_name: &str) -> bool {
    ITERATION_FORBIDDEN.contains(&tool_name)
}

/// The iteration-forbidden tool names as an owned list.
///
/// Behaviourally equivalent to filtering a catalog with
/// [`is_forbidden_in_iteration`], but materialised so it can be handed to the
/// shared `agent_exec` kernel as `AgentExecRequest::forbidden_tools`.
pub fn iteration_forbidden_tools() -> Vec<String> {
    ITERATION_FORBIDDEN.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_set() {
        assert!(is_forbidden_in_iteration("loop_create"));
        assert!(is_forbidden_in_iteration("ask_user"));
        assert!(!is_forbidden_in_iteration("read"));
        assert!(!is_forbidden_in_iteration("loop_scratchpad_set"));
        assert!(!is_forbidden_in_iteration("browser_get_content"));
    }

    #[test]
    fn forbidden_list_matches_predicate() {
        let list = iteration_forbidden_tools();
        // Every listed name is forbidden by the predicate, and both entries
        // are present — the kernel filter and the predicate stay in lockstep.
        assert_eq!(
            list,
            vec!["loop_create".to_string(), "ask_user".to_string()]
        );
        for name in &list {
            assert!(is_forbidden_in_iteration(name));
        }
    }
}
