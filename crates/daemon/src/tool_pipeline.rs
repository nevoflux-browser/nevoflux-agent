//! The ordered gate every tool call passes through (design spec §4.1).
//!
//! # Why the order is fixed
//!
//! The stages are not interchangeable: a policy that refuses must be able to
//! refuse before a stage that would have prompted the user, or the user gets
//! asked about something that was never going to be allowed. `Deny` therefore
//! beats `Ask`, which beats `Allow` (invariant I3), and the first `Deny` ends
//! the run.
//!
//! # Why `Ask` does not leave this module
//!
//! Resolving an `Ask` needs UI, which lives here in the daemon. The pipeline
//! prompts and returns the settled answer, so [`nevoflux_builtin_wasm::ToolGate`]
//! carries only Allow / Deny / Rewrite and the agent loop never has to know a
//! dialog happened.
//!
//! # Who runs it
//!
//! Two entry points, because there are two ways a tool call reaches the daemon
//! and neither goes through the other: the builtin-wasm agent loop
//! (`DaemonHostFunctions::tool_pre`), and the MCP dispatcher that ACP-bridge
//! providers and external MCP clients share
//! (`wasm::mcp_tool_executor::execute_mcp_tool`). Both build their pipeline
//! with [`default_pipeline`] so there is still only one order.
//!
//! # What this gate does not cover
//!
//! Canvas `callTool` has its own gate ([`canvas_gate`]) because the browser
//! settles tab ownership before the daemon is asked. The `ToolRegistry` path
//! `code_mode` uses is still outside.
//!
//! An ACP agent's **own** tools — claude-code's Bash, antigravity's file
//! editor — are outside and cannot be brought in: they run in that agent's
//! process and NevoFlux sees only the finished result on the update stream.
//! A rule that denies a NevoFlux tool does not stop an agent from reaching the
//! same end with its own; denying `create_artifact` stops the artifact, not an
//! agent that writes the HTML to a file instead. Those results are logged with
//! `origin: acp` so the log distinguishes what was gated from what was merely
//! watched — see `wasm::llm::log_native_acp_tool`.

pub mod allowlist;
pub mod canvas_gate;
pub mod pack_hook_stage;
pub mod site_policy;

use nevoflux_builtin_wasm::{ToolCall, ToolContext, ToolDenial, ToolGate};

/// Site rules from installed packs, cached against the packs directory's mtime.
///
/// Process-wide rather than per-caller. A second cache would not be *wrong* —
/// both are keyed on the same mtime — but compiling a hook module is the
/// expensive half, and a private copy means each entry point pays for it
/// separately the first time a pack is used. One cache also means both entry
/// points start honouring a newly installed pack at the same moment instead of
/// whenever each happens to notice.
fn shared_rules() -> &'static site_policy::InstalledRules {
    static RULES: std::sync::OnceLock<site_policy::InstalledRules> = std::sync::OnceLock::new();
    RULES.get_or_init(Default::default)
}

/// Compiled pack hook modules, cached on the same signal as [`shared_rules`].
fn shared_hooks() -> &'static pack_hook_stage::HookRegistry {
    static HOOKS: std::sync::OnceLock<pack_hook_stage::HookRegistry> = std::sync::OnceLock::new();
    HOOKS.get_or_init(Default::default)
}

/// The kernel's gate, in the one order it has (design spec 4.1).
///
/// Entry points call this instead of listing stages themselves. Two lists are
/// two policies the moment someone edits one of them, and I2 is precisely the
/// claim that a call is judged the same way whoever asked.
///
/// `active_packs` are the packs this session has activated: their `installed`
/// -scope rules apply either way, their `active`-scope rules only while they
/// are in here.
pub fn default_pipeline(packs_dir: &std::path::Path, active_packs: &[String]) -> Pipeline {
    // Read `active`-scope rules fresh rather than from the cache: activation
    // changes within a session, which is the one change an mtime cannot see.
    let mut rules = shared_rules().get(packs_dir);
    rules.extend(site_policy::load_active_rules(packs_dir, active_packs));

    // Hooks run after the declarative rules: a rule is cheap and a module is
    // not, so a call a rule already refuses never pays for one.
    let hooks = shared_hooks().for_session(packs_dir, active_packs);

    Pipeline::new(vec![
        Box::new(site_policy::SitePolicyStage::new(rules)),
        Box::new(pack_hook_stage::PackHookStage::new(hooks)),
        Box::new(allowlist::AllowlistStage),
    ])
}

/// What one stage decides about a call.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// This stage has no objection.
    Allow,
    /// This stage wants the user asked first.
    Ask {
        /// What to put in front of the user.
        prompt: String,
    },
    /// This stage refuses.
    Deny(ToolDenial),
    /// This stage wants the call to run with different arguments.
    Rewrite {
        /// Replacement arguments.
        arguments: serde_json::Value,
    },
}

/// One ordered step of the gate.
pub trait Stage: Send + Sync {
    /// Stable name, used in traces and in the event log.
    fn name(&self) -> &'static str;
    /// Decide about this call.
    fn check(&self, call: &ToolCall, ctx: &ToolContext) -> Verdict;
}

/// How the pipeline puts a question to the user.
///
/// Returns `true` when the user allowed it. Taking this as a parameter rather
/// than reaching for the browser directly is what lets the ordering be tested
/// without a UI.
pub type AskFn<'a> = dyn Fn(&str) -> bool + 'a;

/// The ordered gate.
pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    /// Build a pipeline from stages, in the order they must run.
    pub fn new(stages: Vec<Box<dyn Stage>>) -> Self {
        Self { stages }
    }

    /// Stage names in order — the property tests pin, since reordering silently
    /// changes what the policy means.
    pub fn stage_names(&self) -> Vec<&'static str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// Run every stage in order and settle on one verdict.
    ///
    /// An `Ask` from any stage is resolved here: unattended runs refuse it
    /// outright (invariant I6 — a guard that cannot ask must not assume yes),
    /// and interactive runs put it to the user.
    pub fn run(&self, call: &ToolCall, ctx: &ToolContext, ask: &AskFn<'_>) -> ToolGate {
        let mut effective = call.clone();
        let mut rewritten: Option<serde_json::Value> = None;

        for stage in &self.stages {
            match stage.check(&effective, ctx) {
                Verdict::Allow => {}
                Verdict::Deny(d) => return ToolGate::Deny(d),
                Verdict::Rewrite { arguments } => {
                    // Later stages judge what will actually run, not what was
                    // asked for — otherwise a rewrite could smuggle a call past
                    // a stage that would have refused the new arguments.
                    effective.arguments = arguments.clone();
                    rewritten = Some(arguments);
                }
                Verdict::Ask { prompt } => {
                    if ctx.is_unattended {
                        return ToolGate::Deny(ToolDenial {
                            code: "CONFIRMATION_REQUIRED".into(),
                            message: format!(
                                "`{}` needs confirmation, and this run is unattended",
                                call.name
                            ),
                            rule: Some(stage.name().to_string()),
                            pack: None,
                        });
                    }
                    if !ask(&prompt) {
                        return ToolGate::Deny(ToolDenial {
                            code: "USER_DENIED".into(),
                            message: format!("the user declined `{}`", call.name),
                            rule: Some(stage.name().to_string()),
                            pack: None,
                        });
                    }
                }
            }
        }

        match rewritten {
            Some(arguments) => ToolGate::Rewrite { arguments },
            None => ToolGate::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    struct Recording {
        label: &'static str,
        verdict: Verdict,
        seen: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Stage for Recording {
        fn name(&self) -> &'static str {
            self.label
        }
        fn check(&self, _call: &ToolCall, _ctx: &ToolContext) -> Verdict {
            self.seen.lock().unwrap().push(self.label);
            self.verdict.clone()
        }
    }

    fn stage(
        label: &'static str,
        verdict: Verdict,
        seen: &Arc<Mutex<Vec<&'static str>>>,
    ) -> Box<dyn Stage> {
        Box::new(Recording {
            label,
            verdict,
            seen: Arc::clone(seen),
        })
    }

    fn a_call() -> ToolCall {
        ToolCall {
            id: "t1".into(),
            call_id: None,
            name: "browser_click".into(),
            arguments: serde_json::json!({ "selector": "#pay" }),
            signature: None,
        }
    }

    fn a_context(unattended: bool) -> ToolContext {
        ToolContext {
            session_id: "s1".into(),
            origin: "model".into(),
            mode: nevoflux_builtin_wasm::AgentMode::Browser,
            is_unattended: unattended,
            tab_url: Some("https://bank.example/transfer".into()),
            allowed_tools: None,
        }
    }

    fn denial(code: &str) -> ToolDenial {
        ToolDenial {
            code: code.into(),
            message: "nope".into(),
            rule: None,
            pack: None,
        }
    }

    fn never_asked(_: &str) -> bool {
        panic!("the user must not be asked in this test")
    }

    #[test]
    fn the_first_deny_wins_and_later_stages_do_not_run() {
        let seen = Arc::new(Mutex::new(vec![]));
        let p = Pipeline::new(vec![
            stage("first", Verdict::Allow, &seen),
            stage("second", Verdict::Deny(denial("POLICY_DENIED")), &seen),
            stage("third", Verdict::Allow, &seen),
        ]);

        let gate = p.run(&a_call(), &a_context(false), &never_asked);
        assert!(matches!(gate, ToolGate::Deny(_)));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["first", "second"],
            "a stage after the refusal must not run"
        );
    }

    #[test]
    fn an_ask_is_resolved_inside_the_pipeline_and_surfaces_as_allow() {
        let seen = Arc::new(Mutex::new(vec![]));
        let p = Pipeline::new(vec![stage(
            "confirm",
            Verdict::Ask {
                prompt: "allow?".into(),
            },
            &seen,
        )]);

        let asked = RefCell::new(Vec::new());
        let gate = p.run(&a_call(), &a_context(false), &|prompt: &str| {
            asked.borrow_mut().push(prompt.to_string());
            true
        });

        assert!(matches!(gate, ToolGate::Allow));
        assert_eq!(asked.borrow().len(), 1, "the user was asked exactly once");
    }

    #[test]
    fn a_declined_ask_surfaces_as_a_user_denial() {
        let seen = Arc::new(Mutex::new(vec![]));
        let p = Pipeline::new(vec![stage(
            "confirm",
            Verdict::Ask {
                prompt: "allow?".into(),
            },
            &seen,
        )]);

        let gate = p.run(&a_call(), &a_context(false), &|_: &str| false);
        match gate {
            ToolGate::Deny(d) => assert_eq!(d.code, "USER_DENIED"),
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn an_ask_becomes_a_denial_when_nobody_can_answer_it() {
        // I6: a guard that cannot ask must not assume yes.
        let seen = Arc::new(Mutex::new(vec![]));
        let p = Pipeline::new(vec![stage(
            "confirm",
            Verdict::Ask {
                prompt: "allow?".into(),
            },
            &seen,
        )]);

        let gate = p.run(&a_call(), &a_context(true), &never_asked);
        match gate {
            ToolGate::Deny(d) => assert_eq!(d.code, "CONFIRMATION_REQUIRED"),
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn a_rewrite_is_what_later_stages_judge() {
        // Otherwise a rewrite could smuggle a call past a stage that would have
        // refused the replacement arguments.
        struct Inspect {
            seen: Arc<Mutex<Vec<serde_json::Value>>>,
        }
        impl Stage for Inspect {
            fn name(&self) -> &'static str {
                "inspect"
            }
            fn check(&self, call: &ToolCall, _ctx: &ToolContext) -> Verdict {
                self.seen.lock().unwrap().push(call.arguments.clone());
                Verdict::Allow
            }
        }

        let seen = Arc::new(Mutex::new(vec![]));
        let inspected = Arc::new(Mutex::new(vec![]));
        let p = Pipeline::new(vec![
            stage(
                "rewrite",
                Verdict::Rewrite {
                    arguments: serde_json::json!({ "selector": "#safe" }),
                },
                &seen,
            ),
            Box::new(Inspect {
                seen: Arc::clone(&inspected),
            }),
        ]);

        let gate = p.run(&a_call(), &a_context(false), &never_asked);
        match gate {
            ToolGate::Rewrite { arguments } => assert_eq!(arguments["selector"], "#safe"),
            other => panic!("expected a rewrite, got {other:?}"),
        }
        assert_eq!(
            inspected.lock().unwrap()[0]["selector"],
            "#safe",
            "the stage after a rewrite must see the replacement arguments"
        );
    }

    #[test]
    fn an_empty_pipeline_allows() {
        let p = Pipeline::new(vec![]);
        assert!(matches!(
            p.run(&a_call(), &a_context(false), &never_asked),
            ToolGate::Allow
        ));
    }

    /// The order the kernel actually ships, pinned separately from the
    /// synthetic pipelines above.
    ///
    /// Both entry points — the agent loop and the MCP dispatcher — build from
    /// `default_pipeline`, so this list *is* what every installed pack's
    /// policy means. Reordering it changes that silently: hooks ahead of rules
    /// would make a pack pay to compile a module for a call a rule already
    /// refuses, and the allowlist ahead of either would report
    /// `NOT_IN_ALLOWLIST` for calls a pack had a better answer for.
    #[test]
    fn the_shipped_pipeline_runs_rules_then_hooks_then_allowlist() {
        // A directory that does not exist yields no rules and no hooks, which
        // is all this assertion needs: the stages are present either way.
        let no_packs = std::path::Path::new("this-directory-does-not-exist");
        assert_eq!(
            default_pipeline(no_packs, &[]).stage_names(),
            vec!["site_policy", "pack_hooks", "allowlist"],
        );
    }

    #[test]
    fn stage_names_report_the_order_they_run_in() {
        let seen = Arc::new(Mutex::new(vec![]));
        let p = Pipeline::new(vec![
            stage("policy", Verdict::Allow, &seen),
            stage("allowlist", Verdict::Allow, &seen),
            stage("repetition", Verdict::Allow, &seen),
            stage("permission", Verdict::Allow, &seen),
        ]);
        assert_eq!(
            p.stage_names(),
            vec!["policy", "allowlist", "repetition", "permission"],
            "the order the spec fixes"
        );
        p.run(&a_call(), &a_context(false), &never_asked);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["policy", "allowlist", "repetition", "permission"],
            "stages must run in the declared order"
        );
    }
}
