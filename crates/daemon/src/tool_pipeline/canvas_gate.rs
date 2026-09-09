//! The policy check a Canvas panel asks for before it runs a tool
//! (design spec §4.5).
//!
//! # Why the data path is left alone
//!
//! `NevofluxSDK.callTool` executes inside the extension process and never
//! reaches the daemon: the child actor hands it to the background script, which
//! runs the browser action directly. Routing that data path through the daemon
//! would add a round trip to every panel action and rewrite a working
//! mechanism.
//!
//! So the panel asks first instead. It sends what it is about to do, gets a
//! verdict from the same pipeline the model's calls go through, and acts on it.
//! That is enough for invariant I2 — one policy, one answer, whoever is asking —
//! without moving a byte of the data path.
//!
//! # What that trade costs
//!
//! A panel that ignores the verdict is not stopped by anything here. This is a
//! guard against a *pack* doing something the user did not agree to, not a
//! sandbox against hostile page code; a panel already runs with the extension's
//! own reach. The capability declaration in the manifest is what narrows a
//! panel's surface, and it is enforced where the SDK is built rather than here.

use nevoflux_builtin_wasm::{AgentMode, ToolCall, ToolContext, ToolGate};

use super::site_policy::{load_active_rules, load_installed_rules, SitePolicyStage};
use super::Pipeline;

/// What a panel is asking about.
pub struct CanvasRequest<'a> {
    /// The artifact id of the panel making the call.
    pub artifact_id: &'a str,
    /// The browser action it wants to run, e.g. `navigate`, `click`.
    pub action: &'a str,
    /// The action's parameters.
    pub params: serde_json::Value,
    /// URL of the tab it targets, when known.
    pub tab_url: Option<String>,
    /// The session the panel belongs to.
    pub session_id: &'a str,
    /// Packs this session has activated.
    pub active_packs: Vec<String>,
}

/// The answer a panel acts on.
#[derive(Debug, PartialEq, Eq)]
pub enum CanvasVerdict {
    /// Go ahead.
    Allow,
    /// Refuse, with a reason to show.
    Deny {
        /// Machine-readable code.
        code: String,
        /// Explanation for the panel to render.
        message: String,
    },
}

/// Whether an action only reads.
///
/// Used for the daemon-unreachable fallback: refusing every panel read when the
/// daemon is down would break panels that are doing nothing dangerous, while
/// allowing a write in the same situation would let a rule be bypassed by
/// killing the daemon.
pub fn is_read_only_action(action: &str) -> bool {
    matches!(
        action,
        "get_content"
            | "get_markdown"
            | "screenshot"
            | "snapshot"
            | "get_element"
            | "get_elements"
            | "query_all"
            | "get_tabs"
            | "query_tabs"
            | "list_tabs"
            | "read_artifact"
            | "wait_for"
            | "wait_for_stable"
            | "web_fetch"
            | "web_search"
    )
}

/// Run a panel's request through the same pipeline the model's calls use.
pub fn check(packs_dir: &std::path::Path, req: &CanvasRequest<'_>) -> CanvasVerdict {
    let call = ToolCall {
        id: format!("canvas-{}", req.artifact_id),
        call_id: None,
        // Panel actions arrive bare (`click`), while rules are written against
        // tool names (`browser_click`). Normalising here means a pack author
        // writes one rule and it covers both callers.
        name: format!("browser_{}", req.action),
        arguments: req.params.clone(),
        signature: None,
    };

    let ctx = ToolContext {
        session_id: req.session_id.to_string(),
        origin: format!("canvas:{}", req.artifact_id),
        mode: AgentMode::Browser,
        // A panel action is user-driven: someone is looking at the panel they
        // clicked, so a confirmation has an audience.
        is_unattended: false,
        tab_url: req.tab_url.clone(),
        // A panel is not a run with a tool allowlist; its surface is narrowed
        // by the manifest's capability declaration instead.
        allowed_tools: None,
    };

    let mut rules = load_installed_rules(packs_dir);
    rules.extend(load_active_rules(packs_dir, &req.active_packs));

    let pipeline = Pipeline::new(vec![Box::new(SitePolicyStage::new(rules))]);

    // A panel cannot host a modal dialog mid-call, so an `ask` settles as a
    // refusal here. Better a rule that occasionally over-refuses a panel than
    // one a panel can walk past because nobody could be asked.
    match pipeline.run(&call, &ctx, &|_| false) {
        ToolGate::Allow | ToolGate::Rewrite { .. } => CanvasVerdict::Allow,
        ToolGate::Deny(d) => CanvasVerdict::Deny {
            code: d.code,
            message: d.message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bank_pack(dir: &std::path::Path, scope: &str) {
        let pd = dir.join("bank-guard");
        std::fs::create_dir_all(&pd).unwrap();
        std::fs::write(
            pd.join("pack.toml"),
            format!(
                r#"
[pack]
name = "bank-guard"
version = "1.0.0"
protocol = "pack-protocol/0.2"
min_nevoflux = "0.3.0"

[[components.tool_policy]]
scope   = "{scope}"
match   = {{ url = ["*.bank.com/*"] }}
deny    = ["browser_click"]
message = "no automation on banking sites"
"#
            ),
        )
        .unwrap();
    }

    fn req<'a>(action: &'a str, tab_url: Option<&str>) -> CanvasRequest<'a> {
        CanvasRequest {
            artifact_id: "bank/panel",
            action,
            params: serde_json::json!({}),
            tab_url: tab_url.map(|s| s.to_string()),
            session_id: "s1",
            active_packs: vec![],
        }
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nf-cg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The point of the whole exercise: a panel gets the same answer the model
    /// would get for the same action on the same site (invariant I2).
    #[test]
    fn a_panel_gets_the_same_answer_the_model_would() {
        let dir = tmp("same");
        write_bank_pack(&dir, "installed");

        let verdict = check(&dir, &req("click", Some("https://www.bank.com/transfer")));
        match verdict {
            CanvasVerdict::Deny { code, message } => {
                assert_eq!(code, "POLICY_DENIED");
                assert_eq!(message, "no automation on banking sites");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Panel actions arrive bare while rules are written against tool names, so
    /// a pack author writes one rule and it covers both callers.
    #[test]
    fn a_bare_action_name_is_matched_against_the_tool_name_rules_use() {
        let dir = tmp("norm");
        write_bank_pack(&dir, "installed");
        // The rule says `browser_click`; the panel says `click`.
        assert!(matches!(
            check(&dir, &req("click", Some("https://www.bank.com/x"))),
            CanvasVerdict::Deny { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_action_elsewhere_is_allowed() {
        let dir = tmp("elsewhere");
        write_bank_pack(&dir, "installed");
        assert_eq!(
            check(&dir, &req("click", Some("https://example.com/"))),
            CanvasVerdict::Allow
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An active-scope rule only binds a panel while the session has activated
    /// the pack, exactly as it does for the model.
    #[test]
    fn an_active_scope_rule_binds_a_panel_only_once_the_pack_is_active() {
        let dir = tmp("active");
        write_bank_pack(&dir, "active");

        let mut r = req("click", Some("https://www.bank.com/x"));
        assert_eq!(check(&dir, &r), CanvasVerdict::Allow);

        r.active_packs = vec!["bank-guard".to_string()];
        assert!(matches!(check(&dir, &r), CanvasVerdict::Deny { .. }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no rules at all a panel is unaffected — this gate exists to enforce
    /// what a pack declared, not to second-guess panels in general.
    #[test]
    fn a_panel_is_unaffected_when_no_pack_says_otherwise() {
        let dir = tmp("none");
        assert_eq!(
            check(&dir, &req("click", Some("https://www.bank.com/x"))),
            CanvasVerdict::Allow
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Refusing every read when the daemon is unreachable would break panels
    /// doing nothing dangerous; allowing a write would let a rule be bypassed
    /// by killing the daemon.
    #[test]
    fn the_read_only_split_covers_the_actions_panels_actually_use() {
        for a in ["get_content", "screenshot", "query_tabs", "web_fetch"] {
            assert!(is_read_only_action(a), "{a} should be read-only");
        }
        for a in [
            "click",
            "type",
            "fill",
            "navigate",
            "eval_js",
            "upload_file",
        ] {
            assert!(!is_read_only_action(a), "{a} should not be read-only");
        }
    }
}
