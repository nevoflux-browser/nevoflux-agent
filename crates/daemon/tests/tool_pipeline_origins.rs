//! Acceptance for design spec §4.6: the same tool under the same policy gets
//! the same verdict whatever initiated it (invariant I2).
//!
//! # What this does *not* yet cover, and why
//!
//! Only the three origins that reach `Agent::execute_tool` are exercised here:
//! `model`, `subagent:<id>` and `loop:<id>`. The other two named in spec §3.2
//! do not pass through this pipeline yet:
//!
//! - `canvas:<artifact_id>` — Canvas `callTool` runs entirely in the extension
//!   process (`NevofluxChild.sys.mjs` → `background.js` → `executeBrowserTool`)
//!   and never reaches the daemon. P1c routes it here.
//! - `mcp:<client>` — the `ToolRegistry` path `code_mode` uses, and the ACP
//!   provider's own gate in `llm::providers::acp::mcp_bridge`, which does not
//!   go through `HostFunctions` at all.
//!
//! They are listed rather than silently omitted: I2 is three-fifths met, and a
//! reader should not have to infer that from an absence.

use nevoflux_builtin_wasm::{AgentMode, ToolCall, ToolContext, ToolGate};
use nevoflux_daemon::tool_pipeline::{allowlist::AllowlistStage, Pipeline};

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "t1".into(),
        call_id: None,
        name: name.into(),
        arguments: serde_json::json!({}),
        signature: None,
    }
}

fn ctx(origin: &str, unattended: bool, allowed: Option<Vec<String>>) -> ToolContext {
    ToolContext {
        session_id: "s1".into(),
        origin: origin.into(),
        mode: AgentMode::Agent,
        is_unattended: unattended,
        tab_url: None,
        allowed_tools: allowed,
    }
}

fn pipeline() -> Pipeline {
    Pipeline::new(vec![Box::new(AllowlistStage)])
}

/// The origins that reach this pipeline today.
const ROUTED_ORIGINS: &[(&str, bool)] = &[
    ("model", false),
    ("subagent:sa_1", false),
    ("loop:lp_9", true),
];

fn never_asked(_: &str) -> bool {
    panic!("no stage in this pipeline asks")
}

#[test]
fn a_refused_tool_is_refused_for_every_routed_origin() {
    let allowed = Some(vec!["browser_get_content".into()]);
    for (origin, unattended) in ROUTED_ORIGINS {
        let gate = pipeline().run(
            &call("run_command"),
            &ctx(origin, *unattended, allowed.clone()),
            &never_asked,
        );
        match gate {
            ToolGate::Deny(d) => assert_eq!(
                d.code, "NOT_IN_ALLOWLIST",
                "origin {origin} got the wrong refusal"
            ),
            other => panic!("origin {origin} was not refused: {other:?}"),
        }
    }
}

#[test]
fn an_allowed_tool_is_allowed_for_every_routed_origin() {
    let allowed = Some(vec!["browser_*".into()]);
    for (origin, unattended) in ROUTED_ORIGINS {
        let gate = pipeline().run(
            &call("browser_get_content"),
            &ctx(origin, *unattended, allowed.clone()),
            &never_asked,
        );
        assert!(
            matches!(gate, ToolGate::Allow),
            "origin {origin} was not allowed: {gate:?}"
        );
    }
}

/// The verdict must follow the policy, not the origin — otherwise a caller
/// could pick an origin to get a softer answer, which is exactly what I2 rules
/// out.
#[test]
fn the_verdict_depends_on_the_policy_rather_than_on_who_called() {
    let allowed = Some(vec!["read".into()]);
    let verdicts: Vec<bool> = ROUTED_ORIGINS
        .iter()
        .map(|(origin, unattended)| {
            matches!(
                pipeline().run(
                    &call("write"),
                    &ctx(origin, *unattended, allowed.clone()),
                    &never_asked,
                ),
                ToolGate::Deny(_)
            )
        })
        .collect();
    assert!(
        verdicts.iter().all(|d| *d),
        "every origin should be refused, got {verdicts:?}"
    );
}

/// An unattended run cannot answer a dialog, so a stage that wants one must
/// refuse rather than assume yes (invariant I6). Pinned here because the rule
/// is origin-sensitive and easy to regress.
#[test]
fn an_unattended_origin_refuses_what_it_cannot_ask_about() {
    use nevoflux_daemon::tool_pipeline::{Stage, Verdict};

    struct AlwaysAsks;
    impl Stage for AlwaysAsks {
        fn name(&self) -> &'static str {
            "confirm"
        }
        fn check(&self, _c: &ToolCall, _x: &ToolContext) -> Verdict {
            Verdict::Ask {
                prompt: "allow?".into(),
            }
        }
    }

    let p = Pipeline::new(vec![Box::new(AlwaysAsks)]);

    let unattended = p.run(&call("write"), &ctx("loop:lp_9", true, None), &never_asked);
    match unattended {
        ToolGate::Deny(d) => assert_eq!(d.code, "CONFIRMATION_REQUIRED"),
        other => panic!("an unattended ask must refuse, got {other:?}"),
    }

    let interactive = p.run(&call("write"), &ctx("model", false, None), &|_: &str| true);
    assert!(
        matches!(interactive, ToolGate::Allow),
        "an interactive ask that the user allows should pass"
    );
}
