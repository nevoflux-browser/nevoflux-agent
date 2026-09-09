//! Execution-time enforcement of a run's tool allowlist.
//!
//! # Why this stage exists
//!
//! An unattended run is limited today by *not being told* about other tools:
//! `AgentInput.tools_config` decides which tools appear in the request, and the
//! permission gate then auto-approves everything because `services.is_iteration`
//! is true and there is no sidebar to ask. Nothing refuses a call for a tool
//! that was never offered — a model that names one anyway (from an earlier
//! turn's context, or by guessing) reaches the tool.
//!
//! The design doc listed this as migrating an existing `tools_allowlist`, but
//! that setter has no production caller: `AgentRunner::with_tools_allowlist` is
//! only reachable from `AgentRunner`, whose every construction site is inside
//! `#[cfg(test)]`. So this is the first execution-time allowlist the kernel has.

use nevoflux_builtin_wasm::{ToolCall, ToolContext, ToolDenial};

use super::{Stage, Verdict};

/// Refuses tools outside the run's allowlist.
pub struct AllowlistStage;

impl Stage for AllowlistStage {
    fn name(&self) -> &'static str {
        "allowlist"
    }

    fn check(&self, call: &ToolCall, ctx: &ToolContext) -> Verdict {
        let Some(allowed) = ctx.allowed_tools.as_deref() else {
            // No allowlist configured — this run is not restricted.
            return Verdict::Allow;
        };

        if nevoflux_protocol::subagent::is_tool_allowed(allowed, &call.name) {
            return Verdict::Allow;
        }

        Verdict::Deny(ToolDenial {
            code: "NOT_IN_ALLOWLIST".into(),
            message: format!("`{}` is not among the tools this run may use", call.name),
            rule: Some("allowed_tool_classes".into()),
            pack: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            call_id: None,
            name: name.into(),
            arguments: serde_json::json!({}),
            signature: None,
        }
    }

    fn ctx(allowed: Option<Vec<String>>, unattended: bool) -> ToolContext {
        ToolContext {
            session_id: "s1".into(),
            origin: if unattended {
                "loop:lp_1".into()
            } else {
                "model".into()
            },
            mode: nevoflux_builtin_wasm::AgentMode::Agent,
            is_unattended: unattended,
            tab_url: None,
            allowed_tools: allowed,
        }
    }

    fn is_denied(v: &Verdict) -> bool {
        matches!(v, Verdict::Deny(_))
    }

    #[test]
    fn a_tool_outside_the_allowlist_is_refused() {
        let s = AllowlistStage;
        let v = s.check(
            &call("browser_click"),
            &ctx(Some(vec!["browser_get_content".into()]), true),
        );
        match v {
            Verdict::Deny(d) => {
                assert_eq!(d.code, "NOT_IN_ALLOWLIST");
                assert!(d.message.contains("browser_click"), "{}", d.message);
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_inside_the_allowlist_passes() {
        let s = AllowlistStage;
        assert!(!is_denied(&s.check(
            &call("browser_get_content"),
            &ctx(Some(vec!["browser_get_content".into()]), true)
        )));
    }

    #[test]
    fn a_wildcard_entry_covers_its_family() {
        let s = AllowlistStage;
        let c = ctx(Some(vec!["browser_*".into()]), true);
        assert!(!is_denied(&s.check(&call("browser_get_content"), &c)));
        assert!(!is_denied(&s.check(&call("browser_navigate"), &c)));
        assert!(is_denied(&s.check(&call("run_command"), &c)));
    }

    #[test]
    fn a_run_with_no_allowlist_is_not_restricted() {
        let s = AllowlistStage;
        assert!(!is_denied(&s.check(&call("anything"), &ctx(None, true))));
        assert!(!is_denied(&s.check(&call("anything"), &ctx(None, false))));
    }

    #[test]
    fn an_interactive_run_with_an_allowlist_is_still_bound_by_it() {
        // A subagent is interactive-ish but still carries tools_config; the
        // allowlist means the same thing wherever it came from, so it is not
        // conditioned on `is_unattended`.
        let s = AllowlistStage;
        assert!(is_denied(&s.check(
            &call("run_command"),
            &ctx(Some(vec!["read".into()]), false)
        )));
    }

    #[test]
    fn an_empty_allowlist_refuses_everything() {
        // `Allow(vec![])` says "no tools", which is different from "no
        // allowlist" — the distinction is the whole reason the field is an
        // Option rather than a Vec.
        let s = AllowlistStage;
        assert!(is_denied(&s.check(&call("read"), &ctx(Some(vec![]), true))));
    }
}
