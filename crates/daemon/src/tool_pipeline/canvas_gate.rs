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

/// What a pack's panel declared, if the artifact belongs to a pack.
///
/// Returns `None` when no installed pack owns this artifact — a canvas the user
/// or a session created is not a pack panel, so there is no declaration to
/// enforce. It is still bound by site rules; it just has no capability list.
fn panel_capabilities(
    packs_dir: &std::path::Path,
    artifact_id: &str,
) -> Option<nevoflux_pack::manifest::DashboardCapabilities> {
    let entries = std::fs::read_dir(packs_dir).ok()?;
    for entry in entries.flatten() {
        let Ok(src) = std::fs::read_to_string(entry.path().join("pack.toml")) else {
            continue;
        };
        let Ok(manifest) = nevoflux_pack::manifest::Manifest::parse(&src) else {
            continue;
        };
        if let Some(d) = &manifest.components.dashboard {
            if d.artifact_id == artifact_id {
                return Some(d.capabilities.clone());
            }
        }
    }
    None
}

/// Run a panel's request through the same pipeline the model's calls use.
///
/// A pack panel is checked against its declared capabilities first. Declaring
/// nothing means reaching nothing: a panel should have to say what it needs, so
/// that reviewing a pack tells you what its panel can do without reading its
/// code.
pub fn check(packs_dir: &std::path::Path, req: &CanvasRequest<'_>) -> CanvasVerdict {
    if let Some(caps) = panel_capabilities(packs_dir, req.artifact_id) {
        if !caps.call_tool.iter().any(|a| a == req.action) {
            return CanvasVerdict::Deny {
                code: "CAPABILITY_NOT_DECLARED".into(),
                message: format!(
                    "this panel did not declare `{}` in dashboard.capabilities.call_tool",
                    req.action
                ),
            };
        }
    }

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

/// Command prefixes no panel may send, declared or not: settings, pairing,
/// accounts, raw storage, souls, local models and the KB installer.
const FORBIDDEN_COMMAND_PREFIXES: &[&str] = &[
    "config.",
    "remote.",
    "account.",
    "content_store.",
    "soul.",
    "local.",
    "kb.wizard.",
];

/// Individual commands no panel may send, declared or not.
const FORBIDDEN_COMMANDS: &[&str] = &[
    "mcp.add",
    "mcp.update",
    "mcp.delete",
    "mcp.connect",
    "mcp.disconnect",
    "pack.install",
    "pack.update",
    "pack.uninstall",
    "jev.set",
    "models.download",
    "session.delete",
    "session.clone",
    "file.pick",
];

/// What a canvas no pack owns may send without declaring anything.
const GENERATED_CANVAS_COMMANDS: &[&str] = &["daemon.info", "skill.list", "artifact.get"];

/// Whether a command is off limits to every panel (design spec M3-0 §4.1).
pub fn is_forbidden_command(command: &str) -> bool {
    FORBIDDEN_COMMANDS.contains(&command)
        || FORBIDDEN_COMMAND_PREFIXES.iter().any(|p| command.starts_with(p))
}

fn deny(code: &str, message: String) -> CanvasVerdict {
    CanvasVerdict::Deny {
        code: code.into(),
        message,
    }
}

/// Whether a panel may send this daemon system command.
///
/// Order matters: the permanent deny list wins over any declaration, a pack
/// panel then gets exactly what its manifest declared, and a canvas no pack
/// owns gets the fixed read-only set. `artifact.get` is further held to the
/// caller's own artifact whichever way it was allowed.
pub fn check_command(
    packs_dir: &std::path::Path,
    artifact_id: &str,
    command: &str,
    command_params: &serde_json::Value,
) -> CanvasVerdict {
    if is_forbidden_command(command) {
        return deny(
            "FORBIDDEN_COMMAND",
            format!("`{command}` cannot be sent from a canvas"),
        );
    }

    match panel_capabilities(packs_dir, artifact_id) {
        Some(caps) => {
            if !caps.commands.iter().any(|c| c == command) {
                return deny(
                    "COMMAND_NOT_DECLARED",
                    format!(
                        "this panel did not declare `{command}` in dashboard.capabilities.commands"
                    ),
                );
            }
        }
        None => {
            if !GENERATED_CANVAS_COMMANDS.contains(&command) {
                // The message is the only guidance an agent gets when its
                // installed app skill predates these rules (the user kept old
                // skills at the update prompt), so it names the way forward.
                return deny(
                    "COMMAND_NOT_AVAILABLE",
                    format!(
                        "`{command}` is not available to a generated canvas; it may send {}. Use callTool or tool.invoke to run tools",
                        GENERATED_CANVAS_COMMANDS.join(", ")
                    ),
                );
            }
        }
    }

    if command == "artifact.get" {
        let target = command_params.get("artifact_id").and_then(|v| v.as_str());
        if target != Some(artifact_id) {
            return deny(
                "NOT_OWN_ARTIFACT",
                "a canvas may only read its own artifact".into(),
            );
        }
    }

    CanvasVerdict::Allow
}

/// The `canvas.policy_check` answer for `kind: "command"`.
///
/// Always carries `"kind": "command"`: the browser treats an answer without it
/// as coming from a daemon that predates command checks, and refuses.
pub fn command_verdict_json(packs_dir: &std::path::Path, params: &serde_json::Value) -> serde_json::Value {
    let artifact_id = params.get("artifact_id").and_then(|v| v.as_str()).unwrap_or("");
    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if artifact_id.is_empty() || command.is_empty() {
        return serde_json::json!({
            "kind": "command",
            "allow": false,
            "code": "MALFORMED_REQUEST",
            "message": "a command check needs artifact_id and command"
        });
    }
    let command_params = params
        .get("command_params")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    match check_command(packs_dir, artifact_id, command, &command_params) {
        CanvasVerdict::Allow => serde_json::json!({ "kind": "command", "allow": true }),
        CanvasVerdict::Deny { code, message } => serde_json::json!({
            "kind": "command",
            "allow": false,
            "code": code,
            "message": message
        }),
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

    fn write_panel_pack(dir: &std::path::Path, caps: &str) {
        let pd = dir.join("okf");
        std::fs::create_dir_all(&pd).unwrap();
        std::fs::write(
            pd.join("pack.toml"),
            format!(
                r#"
[pack]
name = "okf"
version = "1.0.0"
protocol = "pack-protocol/0.2"
min_nevoflux = "0.3.0"

[components.dashboard]
artifact_id  = "okf-dashboard"
content_type = "text/html"
files_from   = "panel"
entry        = "index.html"
capabilities = {{ call_tool = [{caps}], invoke = [] }}
"#
            ),
        )
        .unwrap();
    }

    fn panel_req<'a>(artifact: &'a str, action: &'a str) -> CanvasRequest<'a> {
        CanvasRequest {
            artifact_id: artifact,
            action,
            params: serde_json::json!({}),
            tab_url: Some("https://example.com/".into()),
            session_id: "s1",
            active_packs: vec![],
        }
    }

    /// Declaring nothing means reaching nothing: reviewing a pack should tell
    /// you what its panel can do without reading its code.
    #[test]
    fn a_panel_that_declared_nothing_reaches_nothing() {
        let dir = tmp("nocaps");
        write_panel_pack(&dir, "");
        match check(&dir, &panel_req("okf-dashboard", "get_content")) {
            CanvasVerdict::Deny { code, .. } => assert_eq!(code, "CAPABILITY_NOT_DECLARED"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_panel_reaches_exactly_what_it_declared() {
        let dir = tmp("caps");
        write_panel_pack(&dir, "\"get_content\", \"navigate\"");

        assert_eq!(
            check(&dir, &panel_req("okf-dashboard", "get_content")),
            CanvasVerdict::Allow
        );
        match check(&dir, &panel_req("okf-dashboard", "click")) {
            CanvasVerdict::Deny { code, message } => {
                assert_eq!(code, "CAPABILITY_NOT_DECLARED");
                assert!(message.contains("click"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A canvas the user or a session made is not a pack panel, so there is no
    /// declaration to enforce -- but site rules still bind it.
    #[test]
    fn a_canvas_no_pack_owns_has_no_capability_list_but_still_obeys_rules() {
        let dir = tmp("usercanvas");
        write_panel_pack(&dir, "\"get_content\"");
        write_bank_pack(&dir, "installed");

        // Not owned by any pack: capability gate does not apply.
        assert_eq!(
            check(&dir, &panel_req("scratch-notes", "click")),
            CanvasVerdict::Allow
        );

        // ...but the site rule still refuses it on a matching site.
        let mut r = panel_req("scratch-notes", "click");
        r.tab_url = Some("https://www.bank.com/transfer".into());
        assert!(matches!(check(&dir, &r), CanvasVerdict::Deny { .. }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The capability check runs before site rules, so a panel reaching for
    /// something it never declared is told that, rather than being told about a
    /// site rule it was never going to reach.
    #[test]
    fn an_undeclared_action_is_named_as_such_even_on_a_restricted_site() {
        let dir = tmp("order");
        write_panel_pack(&dir, "\"get_content\"");
        write_bank_pack(&dir, "installed");

        let mut r = panel_req("okf-dashboard", "click");
        r.tab_url = Some("https://www.bank.com/transfer".into());
        match check(&dir, &r) {
            CanvasVerdict::Deny { code, .. } => assert_eq!(code, "CAPABILITY_NOT_DECLARED"),
            other => panic!("expected the capability refusal, got {other:?}"),
        }
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

    fn write_command_pack(dir: &std::path::Path, commands: &str) {
        let pd = dir.join("okf");
        std::fs::create_dir_all(&pd).unwrap();
        std::fs::write(
            pd.join("pack.toml"),
            format!(
                r#"
[pack]
name = "okf"
version = "1.0.0"
protocol = "pack-protocol/0.2"
min_nevoflux = "0.3.0"

[components.dashboard]
artifact_id  = "okf-dashboard"
content_type = "text/html"
files_from   = "panel"
entry        = "index.html"
capabilities = {{ call_tool = [], invoke = [], commands = [{commands}] }}
"#
            ),
        )
        .unwrap();
    }

    fn code_of(v: CanvasVerdict) -> String {
        match v {
            CanvasVerdict::Deny { code, .. } => code,
            CanvasVerdict::Allow => "ALLOW".into(),
        }
    }

    #[test]
    fn forbidden_prefixes_match_whole_segments_only() {
        assert!(is_forbidden_command("config.llm.set"));
        assert!(is_forbidden_command("remote.agent_set_tier"));
        assert!(is_forbidden_command("account.device_grant_start"));
        assert!(is_forbidden_command("content_store.set"));
        assert!(is_forbidden_command("kb.wizard.install_bun"));
        assert!(is_forbidden_command("mcp.add"));
        assert!(is_forbidden_command("session.delete"));
        assert!(!is_forbidden_command("configure"));
        assert!(!is_forbidden_command("accounting.report"));
        assert!(!is_forbidden_command("mcp.list"));
        assert!(!is_forbidden_command("session.list"));
        assert!(!is_forbidden_command("schedule.run_now"));
    }

    #[test]
    fn a_generated_canvas_gets_only_the_fixed_three() {
        let dir = tmp("cmd-gen");
        let none = serde_json::json!({});
        assert_eq!(check_command(&dir, "gen-1", "daemon.info", &none), CanvasVerdict::Allow);
        assert_eq!(check_command(&dir, "gen-1", "skill.list", &none), CanvasVerdict::Allow);
        assert_eq!(
            code_of(check_command(&dir, "gen-1", "schedule.run_now", &none)),
            "COMMAND_NOT_AVAILABLE"
        );
        // An agent following an outdated installed skill learns the way
        // forward from this message alone.
        match check_command(&dir, "gen-1", "schedule.run_now", &none) {
            CanvasVerdict::Deny { message, .. } => {
                assert!(message.contains("daemon.info"), "{message}");
                assert!(message.contains("callTool"), "{message}");
            }
            other => panic!("expected a denial, got {other:?}"),
        }
        assert_eq!(
            code_of(check_command(&dir, "gen-1", "remote.agent_set_tier", &none)),
            "FORBIDDEN_COMMAND"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pack_panel_gets_exactly_what_it_declared() {
        let dir = tmp("cmd-pack");
        write_command_pack(&dir, "\"schedule.run_now\"");
        let none = serde_json::json!({});
        assert_eq!(
            check_command(&dir, "okf-dashboard", "schedule.run_now", &none),
            CanvasVerdict::Allow
        );
        assert_eq!(
            code_of(check_command(&dir, "okf-dashboard", "brain.put", &none)),
            "COMMAND_NOT_DECLARED"
        );
        // A pack panel did not declare daemon.info, so the generated-canvas
        // set does not apply to it: a pack says what it needs.
        assert_eq!(
            code_of(check_command(&dir, "okf-dashboard", "daemon.info", &none)),
            "COMMAND_NOT_DECLARED"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_declared_forbidden_command_is_still_refused() {
        let dir = tmp("cmd-forbid");
        write_command_pack(&dir, "\"remote.unpair\", \"config.file.write\"");
        let none = serde_json::json!({});
        assert_eq!(
            code_of(check_command(&dir, "okf-dashboard", "remote.unpair", &none)),
            "FORBIDDEN_COMMAND"
        );
        assert_eq!(
            code_of(check_command(&dir, "okf-dashboard", "config.file.write", &none)),
            "FORBIDDEN_COMMAND"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_get_reaches_only_the_callers_own_artifact() {
        let dir = tmp("cmd-art");
        assert_eq!(
            check_command(&dir, "gen-1", "artifact.get", &serde_json::json!({"artifact_id": "gen-1"})),
            CanvasVerdict::Allow
        );
        assert_eq!(
            code_of(check_command(&dir, "gen-1", "artifact.get", &serde_json::json!({"artifact_id": "other"}))),
            "NOT_OWN_ARTIFACT"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_get_without_an_id_is_refused() {
        let dir = tmp("cmd-art-none");
        for p in [serde_json::json!({}), serde_json::json!({"artifact_id": 7})] {
            assert_eq!(code_of(check_command(&dir, "gen-1", "artifact.get", &p)), "NOT_OWN_ARTIFACT");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_verdict_json_always_echoes_the_kind() {
        let dir = tmp("cmd-json");
        let ok = command_verdict_json(
            &dir,
            &serde_json::json!({"kind": "command", "artifact_id": "gen-1", "command": "daemon.info", "command_params": {}}),
        );
        assert_eq!(ok, serde_json::json!({"kind": "command", "allow": true}));

        let denied = command_verdict_json(
            &dir,
            &serde_json::json!({"kind": "command", "artifact_id": "gen-1", "command": "account.status"}),
        );
        assert_eq!(denied["kind"], "command");
        assert_eq!(denied["allow"], false);
        assert_eq!(denied["code"], "FORBIDDEN_COMMAND");

        let malformed = command_verdict_json(&dir, &serde_json::json!({"kind": "command", "command": "daemon.info"}));
        assert_eq!(malformed["kind"], "command");
        assert_eq!(malformed["code"], "MALFORMED_REQUEST");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
