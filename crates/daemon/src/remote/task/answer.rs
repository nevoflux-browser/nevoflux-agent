//! How `allow` or `deny` becomes the answer the waiting turn expects
//! (contract R15, R16).
//!
//! A permission dialog is answered with the label of the button pressed —
//! the daemon compares it to the options it offered — so the choice is mapped
//! onto those options. The model's own question is answered with text, or
//! cancelled. A plan is confirmed or cancelled.

use nevoflux_protocol::{BrowserToolError, PlanResponse};
use serde_json::json;

use super::record::{Approval, ApprovalKind};
use crate::wasm::services::BrowserResponse;

/// What the answering side chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Allow,
    /// Desktop only (decision C8): "always" leaves a permanent rule behind.
    AllowAlways,
    Deny,
}

impl Choice {
    /// A paired agent's choice: `allow` or `deny`, nothing else.
    pub fn parse_agent(s: &str) -> Option<Choice> {
        match s {
            "allow" => Some(Choice::Allow),
            "deny" => Some(Choice::Deny),
            _ => None,
        }
    }

    /// The desktop status bar's choice (`remote.task_answer`, R23).
    pub fn parse_desktop(s: &str) -> Option<Choice> {
        match s {
            "allow" => Some(Choice::Allow),
            "allow_always" => Some(Choice::AllowAlways),
            "deny" => Some(Choice::Deny),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Choice::Allow => "allow",
            Choice::AllowAlways => "allow_always",
            Choice::Deny => "deny",
        }
    }
}

/// What the waiting turn is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A dialog button's label.
    Option(String),
    /// The text answer to the model's question.
    Text(String),
    /// The model's question, cancelled.
    Cancelled,
    Plan(PlanResponse),
}

pub const ANSWER_REQUIRED: &str = "answer is required for a question";
pub const NOT_OFFERED: &str = "that choice is not offered for this approval";

/// Map a choice onto what `approval` waits for (R15, R16).
pub fn answer_for(
    approval: &Approval,
    choice: Choice,
    text: Option<&str>,
) -> Result<Answer, &'static str> {
    match (approval.kind, choice) {
        (ApprovalKind::Tool, Choice::Allow) => Ok(Answer::Option(allow_option(&approval.options))),
        (ApprovalKind::Tool, Choice::AllowAlways) => approval
            .options
            .iter()
            .find(|o| o.starts_with("Always allow"))
            .map(|o| Answer::Option(o.clone()))
            .ok_or(NOT_OFFERED),
        (ApprovalKind::Tool, Choice::Deny) => Ok(Answer::Option(deny_option(&approval.options))),
        (ApprovalKind::Question, Choice::Allow) => text
            .filter(|t| !t.trim().is_empty())
            .map(|t| Answer::Text(t.to_string()))
            .ok_or(ANSWER_REQUIRED),
        (ApprovalKind::Question, Choice::Deny) => Ok(Answer::Cancelled),
        (ApprovalKind::Plan, Choice::Allow) => Ok(Answer::Plan(PlanResponse::Confirmed)),
        (ApprovalKind::Plan, Choice::Deny) => Ok(Answer::Plan(PlanResponse::Cancelled)),
        (ApprovalKind::Question | ApprovalKind::Plan, Choice::AllowAlways) => Err(NOT_OFFERED),
    }
}

/// R15 `allow`: `Allow` or `Allow once`, else the first option that is
/// neither an "always" nor `Deny`.
fn allow_option(options: &[String]) -> String {
    options
        .iter()
        .find(|o| o.eq_ignore_ascii_case("allow") || o.eq_ignore_ascii_case("allow once"))
        .or_else(|| {
            options
                .iter()
                .find(|o| !o.starts_with("Always") && !o.eq_ignore_ascii_case("deny"))
        })
        .cloned()
        .unwrap_or_else(|| "Allow".to_string())
}

/// R15 `deny`: `Deny`, else the last option.
fn deny_option(options: &[String]) -> String {
    options
        .iter()
        .find(|o| o.eq_ignore_ascii_case("deny"))
        .or_else(|| options.last())
        .cloned()
        .unwrap_or_else(|| "Deny".to_string())
}

/// The `browser_tool_response` the waiting turn receives for `answer`.
pub fn response_for(request_id: &str, answer: Answer) -> BrowserResponse {
    match answer {
        Answer::Option(text) | Answer::Text(text) => BrowserResponse {
            request_id: request_id.to_string(),
            success: true,
            result: Some(json!({ "answer": text })),
            error: None,
        },
        // A cancelled question reads as the sidebar's own cancel does (code
        // 8001). A plan is never answered here (the engine sends it on the
        // plan registry); were it to be, refusing is the safe side.
        Answer::Cancelled | Answer::Plan(_) => BrowserResponse {
            request_id: request_id.to_string(),
            success: false,
            result: None,
            error: Some(BrowserToolError {
                code: 8001,
                message: "The question was cancelled".to_string(),
                recoverable: false,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(kind: ApprovalKind, options: &[&str]) -> Approval {
        Approval {
            approval_id: "a1".into(),
            kind,
            summary: "s".into(),
            question: None,
            options: options.iter().map(|o| o.to_string()).collect(),
            plan_steps: vec![],
            at: 0,
        }
    }

    const PERMISSION: &[&str] = &["Allow", "Always allow this type of action", "Deny"];

    #[test]
    fn a_tool_approval_maps_each_choice_to_its_button() {
        let a = approval(ApprovalKind::Tool, PERMISSION);
        assert_eq!(
            answer_for(&a, Choice::Allow, None),
            Ok(Answer::Option("Allow".into()))
        );
        assert_eq!(
            answer_for(&a, Choice::AllowAlways, None),
            Ok(Answer::Option("Always allow this type of action".into()))
        );
        assert_eq!(
            answer_for(&a, Choice::Deny, None),
            Ok(Answer::Option("Deny".into()))
        );
    }

    #[test]
    fn unusual_buttons_fall_back_the_way_the_contract_says() {
        let a = approval(
            ApprovalKind::Tool,
            &["Always allow it", "Only this once", "Never"],
        );
        assert_eq!(
            answer_for(&a, Choice::Allow, None),
            Ok(Answer::Option("Only this once".into()))
        );
        assert_eq!(
            answer_for(&a, Choice::Deny, None),
            Ok(Answer::Option("Never".into())),
            "no Deny: the last option"
        );
        let once = approval(ApprovalKind::Tool, &["allow once", "Deny"]);
        assert_eq!(
            answer_for(&once, Choice::Allow, None),
            Ok(Answer::Option("allow once".into()))
        );
    }

    #[test]
    fn allow_always_needs_a_button_that_offers_it() {
        let a = approval(ApprovalKind::Tool, &["Allow", "Deny"]);
        assert_eq!(answer_for(&a, Choice::AllowAlways, None), Err(NOT_OFFERED));
        assert_eq!(
            answer_for(
                &approval(ApprovalKind::Plan, &[]),
                Choice::AllowAlways,
                None
            ),
            Err(NOT_OFFERED)
        );
    }

    #[test]
    fn a_question_needs_text_and_its_deny_is_a_cancel() {
        let q = approval(ApprovalKind::Question, &["Blue", "Green"]);
        assert_eq!(answer_for(&q, Choice::Allow, None), Err(ANSWER_REQUIRED));
        assert_eq!(
            answer_for(&q, Choice::Allow, Some("  ")),
            Err(ANSWER_REQUIRED)
        );
        assert_eq!(
            answer_for(&q, Choice::Allow, Some("Blue")),
            Ok(Answer::Text("Blue".into()))
        );
        assert_eq!(answer_for(&q, Choice::Deny, None), Ok(Answer::Cancelled));
        let cancelled = response_for("r1", Answer::Cancelled);
        assert!(!cancelled.success);
        assert_eq!(cancelled.error.unwrap().code, 8001);
        let answered = response_for("r1", Answer::Text("Blue".into()));
        assert!(answered.success);
        assert_eq!(answered.result.unwrap(), json!({"answer": "Blue"}));
    }

    #[test]
    fn a_plan_is_confirmed_or_cancelled() {
        let p = approval(ApprovalKind::Plan, &[]);
        assert_eq!(
            answer_for(&p, Choice::Allow, None),
            Ok(Answer::Plan(PlanResponse::Confirmed))
        );
        assert_eq!(
            answer_for(&p, Choice::Deny, None),
            Ok(Answer::Plan(PlanResponse::Cancelled))
        );
    }

    #[test]
    fn a_paired_agent_can_never_choose_allow_always() {
        assert_eq!(Choice::parse_agent("allow"), Some(Choice::Allow));
        assert_eq!(Choice::parse_agent("deny"), Some(Choice::Deny));
        assert_eq!(Choice::parse_agent("allow_always"), None);
        assert_eq!(
            Choice::parse_desktop("allow_always"),
            Some(Choice::AllowAlways)
        );
        assert_eq!(Choice::parse_desktop("Allow"), None, "exact spelling");
    }
}
