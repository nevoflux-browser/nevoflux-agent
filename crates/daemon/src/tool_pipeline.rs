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
//! # What this gate does not cover
//!
//! Canvas `callTool` (P1c), the `ToolRegistry` path `code_mode` uses, and the
//! ACP provider's own permission gate in `llm::providers::acp::mcp_bridge` —
//! that last one does not go through `HostFunctions` at all. Invariant I2 is
//! only partly met until those arrive.

pub mod allowlist;
pub mod site_policy;

use nevoflux_builtin_wasm::{ToolCall, ToolContext, ToolDenial, ToolGate};

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
