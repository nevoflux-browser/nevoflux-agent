// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Pure strategy decision function.
//!
//! `decide()` is the entry point for the strategy engine: given an
//! element fingerprint, input text, desired mode, and current
//! platform adapter (always None in PR #2), it returns an
//! `ExecutionPlan` describing how to fulfill the input request.
//!
//! This module is deliberately side-effect-free. All IO happens
//! in the executor. Testing `decide()` is a matter of constructing
//! input structs and asserting on the returned plan — no mocks or
//! async runtime required.

use crate::agent::browser_input::fingerprint::Fingerprint;
use crate::agent::browser_input::plan::{ExecutionPlan, InputMode};
use crate::agent::browser_input::platform_adapter::Recipe;

/// Input to the strategy decision.
///
/// `adapter` is the platform recipe for the current page's hostname,
/// if one is registered. PR #3 introduced this; callers resolve
/// hostname and look up the recipe before calling `decide()`.
pub struct StrategyInput<'a> {
    pub selector: &'a str,
    pub text: &'a str,
    pub mode: InputMode,
    pub fingerprint: &'a Fingerprint,
    pub hostname: &'a str,
    /// Platform recipe for this hostname, if one is registered.
    /// `None` means "no recipe applies; fall through to generic strategy".
    pub adapter: Option<&'a Recipe>,
}

/// Pure strategy decision.
///
/// Given a fingerprint and input spec, produce an `ExecutionPlan`.
/// This function performs no IO; it is a transform over its inputs.
pub fn decide(input: &StrategyInput) -> ExecutionPlan {
    // Rejected cases: disabled, readonly, invisible.
    if input.fingerprint.disabled || input.fingerprint.readonly {
        return ExecutionPlan::Abort {
            reason: "Element is disabled or readonly".into(),
            recoverable: false,
        };
    }
    if !input.fingerprint.is_visible {
        return ExecutionPlan::Abort {
            reason: "Element is not visible".into(),
            recoverable: true,
        };
    }

    // contentEditable branch: different plan per input mode.
    if input.fingerprint.is_content_editable {
        // If the Actor identified a deeper editable target (e.g., Draft.js
        // nested wrappers), prefer it over the caller-supplied selector.
        let target_selector = input
            .fingerprint
            .innermost_editable_selector
            .as_deref()
            .unwrap_or(input.selector)
            .to_string();

        return match input.mode {
            InputMode::Fill => ExecutionPlan::RichTextFill {
                selector: target_selector,
                text: input.text.to_string(),
            },
            InputMode::Type => ExecutionPlan::Paste {
                selector: target_selector,
                text: input.text.to_string(),
            },
        };
    }

    // Standard input/textarea: native value setter.
    if input.fingerprint.has_value_property {
        return ExecutionPlan::NativeFill {
            selector: input.selector.to_string(),
            text: input.text.to_string(),
        };
    }

    // Last resort: unknown element with no supported path.
    ExecutionPlan::Abort {
        reason: format!(
            "Element tag={} is not editable (no .value property and not contentEditable)",
            input.fingerprint.tag
        ),
        recoverable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::browser_input::fingerprint::EditorFramework;

    /// Build a baseline Fingerprint that matches a standard <input type="text">.
    fn standard_input_fp() -> Fingerprint {
        Fingerprint {
            tag: "input".into(),
            input_type: Some("text".into()),
            has_value_property: true,
            is_content_editable: false,
            disabled: false,
            readonly: false,
            is_visible: true,
            is_focusable: true,
            editor_framework: None,
            react_fiber_present: false,
            inside_iframe: false,
            shadow_root_depth: 0,
            innermost_editable_selector: None,
            computed_role: None,
        }
    }

    /// Build a baseline Fingerprint that matches a Draft.js-style compose box.
    fn draft_js_fp() -> Fingerprint {
        Fingerprint {
            tag: "div".into(),
            input_type: None,
            has_value_property: false,
            is_content_editable: true,
            disabled: false,
            readonly: false,
            is_visible: true,
            is_focusable: true,
            editor_framework: Some(EditorFramework::DraftJs),
            react_fiber_present: true,
            inside_iframe: false,
            shadow_root_depth: 0,
            innermost_editable_selector: Some("div.public-DraftEditor-content".into()),
            computed_role: Some("textbox".into()),
        }
    }

    // ===== Standard input + rejection tests (Task 7) =====

    #[test]
    fn standard_input_fill_returns_native_fill() {
        let fp = standard_input_fp();
        let input = StrategyInput {
            selector: "#name",
            text: "Hello",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "example.com",
            adapter: None,
        };
        let plan = decide(&input);
        assert_eq!(
            plan,
            ExecutionPlan::NativeFill {
                selector: "#name".into(),
                text: "Hello".into(),
            }
        );
    }

    #[test]
    fn standard_textarea_fill_returns_native_fill() {
        let mut fp = standard_input_fp();
        fp.tag = "textarea".into();
        fp.input_type = None;

        let input = StrategyInput {
            selector: "#msg",
            text: "Multi\nline",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "example.com",
            adapter: None,
        };
        let plan = decide(&input);
        assert_eq!(
            plan,
            ExecutionPlan::NativeFill {
                selector: "#msg".into(),
                text: "Multi\nline".into(),
            }
        );
    }

    #[test]
    fn disabled_element_aborts_non_recoverable() {
        let mut fp = standard_input_fp();
        fp.disabled = true;

        let input = StrategyInput {
            selector: "#x",
            text: "",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "example.com",
            adapter: None,
        };
        let plan = decide(&input);
        match plan {
            ExecutionPlan::Abort { recoverable, .. } => assert!(!recoverable),
            other => panic!("expected Abort, got {:?}", other),
        }
    }

    #[test]
    fn readonly_element_aborts_non_recoverable() {
        let mut fp = standard_input_fp();
        fp.readonly = true;

        let input = StrategyInput {
            selector: "#x",
            text: "",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "example.com",
            adapter: None,
        };
        assert!(matches!(
            decide(&input),
            ExecutionPlan::Abort {
                recoverable: false,
                ..
            }
        ));
    }

    #[test]
    fn invisible_element_aborts_recoverable() {
        let mut fp = standard_input_fp();
        fp.is_visible = false;

        let input = StrategyInput {
            selector: "#x",
            text: "",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "example.com",
            adapter: None,
        };
        assert!(matches!(
            decide(&input),
            ExecutionPlan::Abort {
                recoverable: true,
                ..
            }
        ));
    }

    #[test]
    fn unknown_element_aborts_non_recoverable() {
        let mut fp = standard_input_fp();
        fp.tag = "span".into();
        fp.has_value_property = false;

        let input = StrategyInput {
            selector: "#x",
            text: "",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "example.com",
            adapter: None,
        };
        match decide(&input) {
            ExecutionPlan::Abort {
                recoverable,
                reason,
            } => {
                assert!(!recoverable);
                assert!(reason.contains("span"));
            }
            other => panic!("expected Abort, got {:?}", other),
        }
    }

    // ===== contentEditable tests (Task 8) =====

    #[test]
    fn draft_js_fill_uses_rich_text_fill_with_innermost_selector() {
        let fp = draft_js_fp();
        let input = StrategyInput {
            selector: "[data-testid='tweetTextarea_0']",
            text: "Hello world",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "x.com",
            adapter: None,
        };
        let plan = decide(&input);
        assert_eq!(
            plan,
            ExecutionPlan::RichTextFill {
                // innermost selector wins, not the caller-supplied one
                selector: "div.public-DraftEditor-content".into(),
                text: "Hello world".into(),
            }
        );
    }

    #[test]
    fn draft_js_type_uses_paste() {
        let fp = draft_js_fp();
        let input = StrategyInput {
            selector: "[data-testid='tweetTextarea_0']",
            text: "!!!",
            mode: InputMode::Type,
            fingerprint: &fp,
            hostname: "x.com",
            adapter: None,
        };
        assert_eq!(
            decide(&input),
            ExecutionPlan::Paste {
                selector: "div.public-DraftEditor-content".into(),
                text: "!!!".into(),
            }
        );
    }

    #[test]
    fn lexical_fill_uses_rich_text_fill() {
        let mut fp = draft_js_fp();
        fp.editor_framework = Some(EditorFramework::Lexical);
        fp.innermost_editable_selector = Some("#lex".into());

        let input = StrategyInput {
            selector: "#lex",
            text: "Lex text",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "facebook.com",
            adapter: None,
        };
        assert_eq!(
            decide(&input),
            ExecutionPlan::RichTextFill {
                selector: "#lex".into(),
                text: "Lex text".into(),
            }
        );
    }

    #[test]
    fn prosemirror_fill_uses_rich_text_fill() {
        let mut fp = draft_js_fp();
        fp.editor_framework = Some(EditorFramework::ProseMirror);
        fp.innermost_editable_selector = Some(".ProseMirror".into());

        let input = StrategyInput {
            selector: ".ProseMirror",
            text: "PM text",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "bsky.app",
            adapter: None,
        };
        assert_eq!(
            decide(&input),
            ExecutionPlan::RichTextFill {
                selector: ".ProseMirror".into(),
                text: "PM text".into(),
            }
        );
    }

    #[test]
    fn slate_fill_uses_rich_text_fill() {
        let mut fp = draft_js_fp();
        fp.editor_framework = Some(EditorFramework::Slate);
        fp.innermost_editable_selector = Some("#slate".into());

        let input = StrategyInput {
            selector: "#slate",
            text: "Slate text",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "discord.com",
            adapter: None,
        };
        assert_eq!(
            decide(&input),
            ExecutionPlan::RichTextFill {
                selector: "#slate".into(),
                text: "Slate text".into(),
            }
        );
    }

    #[test]
    fn unknown_framework_still_uses_rich_text_fill() {
        // Bare contentEditable div with no framework detected.
        let mut fp = draft_js_fp();
        fp.editor_framework = None;
        fp.react_fiber_present = false;
        fp.innermost_editable_selector = None;

        let input = StrategyInput {
            selector: "#bareCE",
            text: "Generic",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "example.com",
            adapter: None,
        };
        assert_eq!(
            decide(&input),
            ExecutionPlan::RichTextFill {
                // Falls back to caller-supplied selector since no
                // innermost editable was provided.
                selector: "#bareCE".into(),
                text: "Generic".into(),
            }
        );
    }

    #[test]
    fn content_editable_without_innermost_uses_caller_selector() {
        let mut fp = draft_js_fp();
        fp.innermost_editable_selector = None;

        let input = StrategyInput {
            selector: "#outer",
            text: "hi",
            mode: InputMode::Fill,
            fingerprint: &fp,
            hostname: "example.com",
            adapter: None,
        };
        assert_eq!(
            decide(&input),
            ExecutionPlan::RichTextFill {
                selector: "#outer".into(),
                text: "hi".into(),
            }
        );
    }
}
