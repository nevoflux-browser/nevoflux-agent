//! `pack.toml` model, parsing, and parse-time field validation.

use semver::Version;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub pack: PackMeta,
    #[serde(default)]
    pub components: Components,
    /// Install-time capability request (0.2). Absent means it asks for nothing.
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackMeta {
    pub name: String,
    pub version: Version,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    pub protocol: String,
    pub min_nevoflux: Version,
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Components {
    pub skills: Option<SkillsComponent>,
    pub canvas_tools: Option<CanvasToolsComponent>,
    #[serde(default)]
    pub seed: Vec<SeedComponent>,
    pub knowledge: Option<KnowledgeComponent>,
    pub dashboard: Option<DashboardComponent>,
    pub protected: Option<ProtectedComponent>,
    /// How far this pack may go when replacing the system prompt (0.2).
    pub prompt: Option<PromptComponent>,
    /// Site rules this pack contributes (0.2). Can only ever tighten.
    #[serde(default)]
    pub tool_policy: Vec<ToolPolicyComponent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillsComponent {
    pub dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CanvasToolsComponent {
    pub files: Vec<String>,
    #[serde(default)]
    pub external_binaries: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeedComponent {
    pub slug: String,
    pub from: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KnowledgeComponent {
    pub from: String,
    pub source_name: Option<String>,
    pub trust: String,
    pub unlock: UnlockSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UnlockSpec {
    Key { key: String },
    Password { password: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashboardComponent {
    pub artifact_id: String,
    pub content_type: String,
    pub files_from: String,
    pub entry: String,
    /// What the panel SDK is allowed to reach (0.2).
    ///
    /// Empty means the panel declares nothing, which P1c treats as "no direct
    /// tool access" rather than "everything" — a panel should have to say what
    /// it needs.
    #[serde(default)]
    pub capabilities: DashboardCapabilities,
}

/// The tools a dashboard panel declares it needs (`pack-protocol/0.2`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DashboardCapabilities {
    /// Browser actions reachable through `NevofluxSDK.callTool`.
    #[serde(default)]
    pub call_tool: Vec<String>,
    /// Whitelisted canvas tools reachable through `NevofluxSDK.tool.invoke`.
    #[serde(default)]
    pub invoke: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProtectedComponent {
    #[serde(default)]
    pub slugs: Vec<String>,
    #[serde(default)]
    pub prefixes: Vec<String>,
}

/// How far a pack may go when replacing the system prompt (`pack-protocol/0.2`).
///
/// Declared in the manifest so the installer can show the ceiling before
/// anything runs; `system_prompt_replace` then refuses a mode above it.
#[derive(Debug, Clone, Deserialize)]
pub struct PromptComponent {
    /// `none` (default), `keep_kernel`, or `full`.
    #[serde(default = "default_prompt_replace")]
    pub replace: String,
}

fn default_prompt_replace() -> String {
    "none".to_string()
}

/// URL patterns a tool policy applies to.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolPolicyMatch {
    /// Glob patterns matched against the target tab URL.
    #[serde(default)]
    pub url: Vec<String>,
}

/// A site rule contributed by a pack (`pack-protocol/0.2`).
///
/// Only ever tightens: there is a `deny` list and an `ask` list and no `allow`,
/// because a pack that could widen permission would break invariant I3.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolPolicyComponent {
    /// `active` (session-scoped, the default) or `installed` (always on).
    #[serde(default = "default_policy_scope")]
    pub scope: String,
    /// Where the rule applies.
    #[serde(default, rename = "match")]
    pub match_on: ToolPolicyMatch,
    /// Tools refused outright.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Tools that require confirmation.
    #[serde(default)]
    pub ask: Vec<String>,
    /// Shown to the user and to the model when the rule fires.
    #[serde(default)]
    pub message: String,
}

fn default_policy_scope() -> String {
    "active".to_string()
}

/// What a pack asks for at install time (`pack-protocol/0.2`).
///
/// Rendered as the install checklist: capabilities are granted here, individual
/// actions are still confirmed at run time, and unattended runs pre-authorise
/// their side effects separately (design spec 4.3.3).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Permissions {
    /// Capability groups, e.g. `browser.read`, `fs.session`.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Whether this pack may run with nobody watching.
    #[serde(default)]
    pub unattended: bool,
    /// Actions with consequences outside the session, ticked one by one.
    #[serde(default)]
    pub side_effects: Vec<String>,
}

pub const SUPPORTED_PROTOCOLS: &[&str] = &["pack-protocol/0.1", "pack-protocol/0.2"];

/// Components and fields that only exist from `pack-protocol/0.2` onward.
const PROTOCOL_0_2: &str = "pack-protocol/0.2";

impl Manifest {
    /// Parse and run parse-time field validation. Capability/namespace checks
    /// live in `capability::validate` and run later against `ResolvedPaths`.
    pub fn parse(toml_src: &str) -> Result<Manifest, String> {
        let m: Manifest = toml::from_str(toml_src).map_err(|e| e.to_string())?;
        m.validate_fields()?;
        Ok(m)
    }

    /// The GBrain namespace prefix: explicit override, else pack name.
    pub fn namespace(&self) -> &str {
        self.pack.namespace.as_deref().unwrap_or(&self.pack.name)
    }

    fn validate_fields(&self) -> Result<(), String> {
        // name: [a-z0-9-]+
        if self.pack.name.is_empty()
            || !self
                .pack
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!(
                "invalid pack.name '{}': must match [a-z0-9-]+",
                self.pack.name
            ));
        }
        // protocol supported
        if !SUPPORTED_PROTOCOLS.contains(&self.pack.protocol.as_str()) {
            return Err(format!("unsupported protocol '{}'", self.pack.protocol));
        }
        // 0.2 features need the 0.2 protocol. Declaring one under 0.1 is a
        // manifest bug rather than something to accept quietly: the install
        // checklist is built from these fields, so a pack whose protocol says
        // they cannot be there would show the user the wrong risks.
        if self.pack.protocol != PROTOCOL_0_2 {
            let mut only_in_0_2 = Vec::new();
            if self.components.prompt.is_some() {
                only_in_0_2.push("components.prompt");
            }
            if !self.components.tool_policy.is_empty() {
                only_in_0_2.push("components.tool_policy");
            }
            if self.components.dashboard.as_ref().is_some_and(|d| {
                !d.capabilities.call_tool.is_empty() || !d.capabilities.invoke.is_empty()
            }) {
                only_in_0_2.push("components.dashboard.capabilities");
            }
            if !self.permissions.tools.is_empty()
                || self.permissions.unattended
                || !self.permissions.side_effects.is_empty()
            {
                only_in_0_2.push("permissions");
            }
            if !only_in_0_2.is_empty() {
                return Err(format!(
                    "{} requires protocol '{}', but this manifest declares '{}'",
                    only_in_0_2.join(", "),
                    PROTOCOL_0_2,
                    self.pack.protocol
                ));
            }
        }

        // prompt.replace must name a mode we know
        if let Some(pc) = &self.components.prompt {
            if !matches!(pc.replace.as_str(), "none" | "keep_kernel" | "full") {
                return Err(format!(
                    "prompt.replace '{}' unsupported (none | keep_kernel | full)",
                    pc.replace
                ));
            }
        }

        // tool_policy: known scope, and it must actually tighten something.
        for tp in &self.components.tool_policy {
            if !matches!(tp.scope.as_str(), "active" | "installed") {
                return Err(format!(
                    "tool_policy.scope '{}' unsupported (active | installed)",
                    tp.scope
                ));
            }
            if tp.deny.is_empty() && tp.ask.is_empty() {
                return Err(
                    "a tool_policy rule must deny or ask something; it cannot only match".into(),
                );
            }
        }

        // knowledge.trust must be read-only in v1
        if let Some(k) = &self.components.knowledge {
            if k.trust != "read-only" {
                return Err(format!(
                    "knowledge.trust '{}' unsupported in v1 (only 'read-only')",
                    k.trust
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [pack]
        name = "hello-pack"
        version = "0.1.0"
        protocol = "pack-protocol/0.1"
        min_nevoflux = "0.3.0"

        [components.skills]
        dir = "skills"
    "#;

    #[test]
    fn parses_minimal_manifest() {
        let m = Manifest::parse(MINIMAL).unwrap();
        assert_eq!(m.pack.name, "hello-pack");
        assert_eq!(m.namespace(), "hello-pack");
        assert_eq!(m.components.skills.unwrap().dir, "skills");
    }

    #[test]
    fn namespace_override_wins() {
        let src = MINIMAL.replace(
            "name = \"hello-pack\"",
            "name = \"career-pack\"\nnamespace = \"career\"",
        );
        let m = Manifest::parse(&src).unwrap();
        assert_eq!(m.namespace(), "career");
    }

    #[test]
    fn rejects_bad_name() {
        let src = MINIMAL.replace("hello-pack", "Hello_Pack");
        assert!(Manifest::parse(&src).unwrap_err().contains("pack.name"));
    }

    #[test]
    fn rejects_unsupported_protocol() {
        let src = MINIMAL.replace("pack-protocol/0.1", "pack-protocol/9.9");
        assert!(Manifest::parse(&src)
            .unwrap_err()
            .contains("unsupported protocol"));
    }

    const V02: &str = r#"
        [pack]
        name = "bank-guard"
        version = "1.0.0"
        protocol = "pack-protocol/0.2"
        min_nevoflux = "0.3.0"

        [components.prompt]
        replace = "keep_kernel"

        [[components.tool_policy]]
        scope   = "installed"
        match   = { url = ["*.bank.com/*"] }
        deny    = ["browser_click", "browser_type"]
        ask     = ["browser_get_content"]
        message = "no automation on banking sites"

        [permissions]
        tools        = ["browser.read"]
        unattended   = true
        side_effects = ["browser_submit"]
    "#;

    #[test]
    fn a_0_2_manifest_parses_its_new_components() {
        let m = Manifest::parse(V02).unwrap();
        assert_eq!(m.components.prompt.unwrap().replace, "keep_kernel");

        let tp = &m.components.tool_policy[0];
        assert_eq!(tp.scope, "installed");
        assert_eq!(tp.match_on.url, vec!["*.bank.com/*"]);
        assert_eq!(tp.deny, vec!["browser_click", "browser_type"]);
        assert_eq!(tp.ask, vec!["browser_get_content"]);

        assert!(m.permissions.unattended);
        assert_eq!(m.permissions.side_effects, vec!["browser_submit"]);
    }

    /// The frozen-contract requirement: a 0.1 pack must keep working untouched,
    /// and every 0.2 field must default to asking for nothing.
    #[test]
    fn a_0_1_manifest_still_parses_and_asks_for_nothing() {
        let m = Manifest::parse(MINIMAL).unwrap();
        assert!(m.components.prompt.is_none());
        assert!(m.components.tool_policy.is_empty());
        assert!(m.permissions.tools.is_empty());
        assert!(!m.permissions.unattended);
        assert!(m.permissions.side_effects.is_empty());
    }

    /// Declaring a 0.2 feature under 0.1 is a manifest bug, not something to
    /// accept quietly: the install checklist is built from these fields, so a
    /// pack whose protocol says they cannot be there would show the wrong risks.
    #[test]
    fn a_0_1_manifest_may_not_declare_0_2_components() {
        let src = V02.replace("pack-protocol/0.2", "pack-protocol/0.1");
        let err = Manifest::parse(&src).unwrap_err();
        assert!(err.contains("requires protocol"), "{err}");
        assert!(err.contains("components.prompt"), "{err}");
        assert!(err.contains("permissions"), "{err}");
    }

    #[test]
    fn an_unknown_prompt_replace_mode_is_rejected() {
        let src = V02.replace("keep_kernel", "everything");
        assert!(Manifest::parse(&src)
            .unwrap_err()
            .contains("prompt.replace"));
    }

    #[test]
    fn an_unknown_tool_policy_scope_is_rejected() {
        let src = V02.replace("scope   = \"installed\"", "scope   = \"global\"");
        assert!(Manifest::parse(&src)
            .unwrap_err()
            .contains("tool_policy.scope"));
    }

    /// A rule that matches but neither denies nor asks tightens nothing, so it
    /// is a mistake worth naming rather than a no-op worth keeping.
    #[test]
    fn a_tool_policy_that_tightens_nothing_is_rejected() {
        let src = V02
            .replace(
                "deny    = [\"browser_click\", \"browser_type\"]",
                "deny    = []",
            )
            .replace("ask     = [\"browser_get_content\"]", "ask     = []");
        assert!(Manifest::parse(&src)
            .unwrap_err()
            .contains("must deny or ask"));
    }

    /// Invariant I3 is structural here: there is no `allow` list to write, so a
    /// pack has no way to widen permission even if its author wanted to.
    #[test]
    fn a_tool_policy_has_no_way_to_widen_permission() {
        let src = V02.replace(
            "message = \"no automation on banking sites\"",
            "allow   = [\"browser_eval_js\"]\n        message = \"x\"",
        );
        let m = Manifest::parse(&src).unwrap();
        let tp = &m.components.tool_policy[0];
        assert!(tp.deny.contains(&"browser_click".to_string()));
        // The stray key parses as nothing: ToolPolicyComponent has no allow.
        assert_eq!(tp.ask, vec!["browser_get_content"]);
    }

    #[test]
    fn dashboard_capabilities_default_to_declaring_nothing() {
        let src = format!(
            "{V02}\n[components.dashboard]\nartifact_id=\"bank/panel\"\ncontent_type=\"text/html\"\nfiles_from=\"panel\"\nentry=\"index.html\"\n"
        );
        let m = Manifest::parse(&src).unwrap();
        let d = m.components.dashboard.unwrap();
        assert!(d.capabilities.call_tool.is_empty());
        assert!(d.capabilities.invoke.is_empty());
    }

    #[test]
    fn rejects_non_readonly_knowledge_trust() {
        let src = format!(
            "{MINIMAL}\n[components.knowledge]\nfrom=\"kb.nbrain\"\ntrust=\"full-merge\"\nunlock={{ password = \"x\" }}\n"
        );
        assert!(Manifest::parse(&src).unwrap_err().contains("read-only"));
    }
}
