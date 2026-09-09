//! Site rules contributed by installed packs (design spec §4.3.1, §4.4).
//!
//! # Only ever tightens
//!
//! A rule has a deny list and an ask list and no allow list. That is invariant
//! I3 made structural: a pack cannot widen permission because the manifest has
//! no field in which to say so.
//!
//! # Which URL a rule is matched against
//!
//! Spec §4.4 names three sources. Two are available before the tool runs and
//! are both checked here:
//!
//! 1. the URL of the tab the call targets, which `ToolContext.tab_url` carries —
//!    the only source a computer-use call has, since those take no URL argument;
//! 2. a URL in the call's own arguments, which is what `browser_navigate` and
//!    `browser_open_tab` carry.
//!
//! The third — the URL *after* the call ran — belongs to `tool_post` and cannot
//! stop the call that navigated. That is deliberate and it is a real gap: a
//! click that redirects into a restricted site is not caught until the next
//! call. The rules still hold from that next call onward.

use nevoflux_builtin_wasm::{ToolCall, ToolContext, ToolDenial};

use super::{Stage, Verdict};

/// One pack-contributed rule.
#[derive(Debug, Clone)]
pub struct SiteRule {
    /// The pack that contributed it, named in the refusal.
    pub pack: String,
    /// URL globs this rule applies to. Empty means every URL.
    pub urls: Vec<String>,
    /// Tool name globs refused outright.
    pub deny: Vec<String>,
    /// Tool name globs that need confirmation.
    pub ask: Vec<String>,
    /// Explanation shown to the user and to the model.
    pub message: String,
}

/// Matches a `*`-glob against a whole string.
///
/// Deliberately tiny: `*` stands for any run of characters, including `/` and
/// `.`, and everything else is literal. A pack author writing `*.bank.com/*`
/// means what it looks like, and nothing here quietly means more.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0usize;
    // A leading literal must sit at the very start.
    if let Some(first) = parts.first() {
        if !first.is_empty() {
            if !text.starts_with(first) {
                return false;
            }
            pos = first.len();
        }
    }
    // A trailing literal must sit at the very end.
    if let Some(last) = parts.last() {
        if !last.is_empty() && !text.ends_with(last) {
            return false;
        }
    }
    // Middles must appear in order after the leading literal.
    for middle in &parts[1..parts.len().saturating_sub(1)] {
        if middle.is_empty() {
            continue;
        }
        match text[pos..].find(*middle) {
            Some(off) => pos += off + middle.len(),
            None => return false,
        }
    }
    // With a trailing literal, what is left has to be long enough to hold it.
    match parts.last() {
        Some(last) if !last.is_empty() => text.len() >= pos + last.len(),
        _ => true,
    }
}

/// The URL a call should be judged against, if any.
///
/// Prefers a URL the call is carrying — `browser_navigate` is about *where it
/// is going*, not where it is — and falls back to the tab it targets.
pub fn target_url(call: &ToolCall, ctx: &ToolContext) -> Option<String> {
    if let Some(u) = call
        .arguments
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|u| !u.is_empty())
    {
        return Some(u.to_string());
    }
    ctx.tab_url.clone()
}

/// Load the `installed`-scope rules every installed pack contributes.
///
/// Only `installed` scope is read here. An `active` rule belongs to a pack the
/// session has activated, which the session — not the filesystem — knows about;
/// it arrives once pack activation lands.
///
/// A pack whose manifest will not parse is skipped rather than failing the
/// call: one broken pack must not take the browser down with it. It is traced,
/// because silently ignoring a guard pack is its own hazard.
pub fn load_installed_rules(packs_dir: &std::path::Path) -> Vec<SiteRule> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(packs_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let manifest_path = entry.path().join("pack.toml");
        let Ok(src) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let manifest = match nevoflux_pack::manifest::Manifest::parse(&src) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    pack = %entry.file_name().to_string_lossy(),
                    error = %e,
                    "site policy: skipping a pack whose manifest will not parse"
                );
                continue;
            }
        };
        for tp in &manifest.components.tool_policy {
            if tp.scope != "installed" {
                continue;
            }
            out.push(SiteRule {
                pack: manifest.pack.name.clone(),
                urls: tp.match_on.url.clone(),
                deny: tp.deny.clone(),
                ask: tp.ask.clone(),
                message: if tp.message.is_empty() {
                    format!("`{}` restricts this action here", manifest.pack.name)
                } else {
                    tp.message.clone()
                },
            });
        }
    }
    out
}

/// The `active`-scope rules of the named packs.
///
/// Separate from [`load_installed_rules`] because the two answer different
/// questions: installed rules apply to every session, active ones only while a
/// session has chosen the pack. Not cached — activation changes within a
/// session, which is exactly what a directory-mtime cache cannot see.
pub fn load_active_rules(packs_dir: &std::path::Path, active: &[String]) -> Vec<SiteRule> {
    let mut out = Vec::new();
    for name in active {
        let manifest_path = packs_dir.join(name).join("pack.toml");
        let Ok(src) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = nevoflux_pack::manifest::Manifest::parse(&src) else {
            continue;
        };
        for tp in &manifest.components.tool_policy {
            if tp.scope != "active" {
                continue;
            }
            out.push(SiteRule {
                pack: manifest.pack.name.clone(),
                urls: tp.match_on.url.clone(),
                deny: tp.deny.clone(),
                ask: tp.ask.clone(),
                message: if tp.message.is_empty() {
                    format!("`{}` restricts this action here", manifest.pack.name)
                } else {
                    tp.message.clone()
                },
            });
        }
    }
    out
}

/// Installed rules, reloaded when the packs directory changes.
///
/// Re-reading and re-parsing every manifest on every tool call would be
/// wasteful; caching forever would miss a pack installed mid-session. The
/// directory mtime moves when a pack is added or removed, which is exactly the
/// event that matters, so it serves as the cache key.
#[derive(Default)]
pub struct InstalledRules {
    cached: std::sync::Mutex<Option<(Option<std::time::SystemTime>, Vec<SiteRule>)>>,
}

impl InstalledRules {
    /// Current rules, reloading if the packs directory has changed.
    pub fn get(&self, packs_dir: &std::path::Path) -> Vec<SiteRule> {
        let stamp = std::fs::metadata(packs_dir).and_then(|m| m.modified()).ok();
        let mut guard = match self.cached.lock() {
            Ok(g) => g,
            // A poisoned lock must not stop the call; read fresh instead.
            Err(_) => return load_installed_rules(packs_dir),
        };
        if let Some((seen, rules)) = guard.as_ref() {
            if *seen == stamp {
                return rules.clone();
            }
        }
        let rules = load_installed_rules(packs_dir);
        *guard = Some((stamp, rules.clone()));
        rules
    }
}

/// Refuses or questions tools according to installed packs' site rules.
pub struct SitePolicyStage {
    rules: Vec<SiteRule>,
}

impl SitePolicyStage {
    /// Build a stage from rules already loaded.
    pub fn new(rules: Vec<SiteRule>) -> Self {
        Self { rules }
    }

    fn applies(rule: &SiteRule, url: Option<&str>) -> bool {
        if rule.urls.is_empty() {
            return true;
        }
        match url {
            Some(u) => rule.urls.iter().any(|p| glob_match(p, u)),
            // A rule scoped to URLs cannot fire on a call with no URL at all.
            None => false,
        }
    }
}

impl Stage for SitePolicyStage {
    fn name(&self) -> &'static str {
        "site_policy"
    }

    fn check(&self, call: &ToolCall, ctx: &ToolContext) -> Verdict {
        let url = target_url(call, ctx);

        // Deny wins over ask, and it wins across packs: the strictest rule
        // decides (I3). So every rule is examined before settling on an ask.
        let mut pending_ask: Option<&SiteRule> = None;

        for rule in &self.rules {
            if !Self::applies(rule, url.as_deref()) {
                continue;
            }
            if rule.deny.iter().any(|p| glob_match(p, &call.name)) {
                return Verdict::Deny(ToolDenial {
                    code: "POLICY_DENIED".into(),
                    message: rule.message.clone(),
                    rule: url.clone(),
                    pack: Some(rule.pack.clone()),
                });
            }
            if pending_ask.is_none() && rule.ask.iter().any(|p| glob_match(p, &call.name)) {
                pending_ask = Some(rule);
            }
        }

        match pending_ask {
            Some(rule) => Verdict::Ask {
                prompt: format!(
                    "{}\n\n{} wants to run `{}`{}. Allow it?",
                    rule.message,
                    rule.pack,
                    call.name,
                    url.map(|u| format!(" on {u}")).unwrap_or_default()
                ),
            },
            None => Verdict::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_with(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            call_id: None,
            name: name.into(),
            arguments: args,
            signature: None,
        }
    }

    fn call(name: &str) -> ToolCall {
        call_with(name, serde_json::json!({}))
    }

    fn ctx(tab_url: Option<&str>) -> ToolContext {
        ToolContext {
            session_id: "s1".into(),
            origin: "model".into(),
            mode: nevoflux_builtin_wasm::AgentMode::Browser,
            is_unattended: false,
            tab_url: tab_url.map(|s| s.to_string()),
            allowed_tools: None,
        }
    }

    fn bank_rule() -> SiteRule {
        SiteRule {
            pack: "bank-guard".into(),
            urls: vec!["*.bank.com/*".into()],
            deny: vec!["browser_click".into(), "browser_type".into()],
            ask: vec!["browser_get_content".into()],
            message: "no automation on banking sites".into(),
        }
    }

    #[test]
    fn glob_matches_the_shapes_a_pack_author_would_write() {
        assert!(glob_match("*.bank.com/*", "https://www.bank.com/transfer"));
        assert!(glob_match(
            "*://*/online-banking/*",
            "https://x.io/online-banking/a"
        ));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("https://exact.com/", "https://exact.com/"));

        assert!(!glob_match("*.bank.com/*", "https://bank.com.evil.io/x"));
        assert!(!glob_match("https://exact.com/", "https://exact.com/other"));
        assert!(!glob_match(
            "*.bank.com/*",
            "https://www.other.com/transfer"
        ));
    }

    #[test]
    fn a_denied_tool_on_a_matching_site_is_refused_and_names_its_pack() {
        let s = SitePolicyStage::new(vec![bank_rule()]);
        match s.check(
            &call("browser_click"),
            &ctx(Some("https://www.bank.com/transfer")),
        ) {
            Verdict::Deny(d) => {
                assert_eq!(d.code, "POLICY_DENIED");
                assert_eq!(d.pack.as_deref(), Some("bank-guard"));
                assert_eq!(d.message, "no automation on banking sites");
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn the_same_tool_elsewhere_is_untouched() {
        let s = SitePolicyStage::new(vec![bank_rule()]);
        assert!(matches!(
            s.check(&call("browser_click"), &ctx(Some("https://example.com/"))),
            Verdict::Allow
        ));
    }

    #[test]
    fn an_ask_rule_produces_a_prompt_naming_the_site_and_the_pack() {
        let s = SitePolicyStage::new(vec![bank_rule()]);
        match s.check(
            &call("browser_get_content"),
            &ctx(Some("https://www.bank.com/summary")),
        ) {
            Verdict::Ask { prompt } => {
                assert!(prompt.contains("bank-guard"), "{prompt}");
                assert!(prompt.contains("browser_get_content"), "{prompt}");
                assert!(prompt.contains("bank.com/summary"), "{prompt}");
            }
            other => panic!("expected an ask, got {other:?}"),
        }
    }

    /// `browser_navigate` is about where it is *going*, so the argument URL
    /// decides — otherwise a navigation into a restricted site would be judged
    /// against the page it is leaving.
    #[test]
    fn a_navigation_is_judged_by_where_it_is_going() {
        let s = SitePolicyStage::new(vec![SiteRule {
            deny: vec!["browser_navigate".into()],
            ..bank_rule()
        }]);
        let going = call_with(
            "browser_navigate",
            serde_json::json!({ "url": "https://www.bank.com/login" }),
        );
        assert!(matches!(
            s.check(&going, &ctx(Some("https://example.com/"))),
            Verdict::Deny(_)
        ));
    }

    /// A computer-use call carries no URL, so the tab it targets is the only
    /// source — which is exactly why ToolContext carries one.
    #[test]
    fn a_call_with_no_url_argument_is_judged_by_its_tab() {
        let s = SitePolicyStage::new(vec![SiteRule {
            deny: vec!["computer_*".into()],
            ..bank_rule()
        }]);
        assert!(matches!(
            s.check(
                &call("computer_click"),
                &ctx(Some("https://www.bank.com/x"))
            ),
            Verdict::Deny(_)
        ));
    }

    /// A URL-scoped rule cannot fire when there is no URL at all, or it would
    /// silently become a global rule.
    #[test]
    fn a_url_scoped_rule_does_not_fire_without_a_url() {
        let s = SitePolicyStage::new(vec![bank_rule()]);
        assert!(matches!(
            s.check(&call("browser_click"), &ctx(None)),
            Verdict::Allow
        ));
    }

    /// A rule with no URLs is global by construction.
    #[test]
    fn a_rule_with_no_urls_applies_everywhere() {
        let s = SitePolicyStage::new(vec![SiteRule {
            urls: vec![],
            ..bank_rule()
        }]);
        assert!(matches!(
            s.check(&call("browser_click"), &ctx(None)),
            Verdict::Deny(_)
        ));
    }

    fn write_pack(dir: &std::path::Path, name: &str, body: &str) {
        let pd = dir.join(name);
        std::fs::create_dir_all(&pd).unwrap();
        std::fs::write(pd.join("pack.toml"), body).unwrap();
    }

    fn manifest_with(scope: &str) -> String {
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
        )
    }

    #[test]
    fn only_installed_scope_rules_are_loaded_from_disk() {
        let tmp = std::env::temp_dir().join(format!("nf-sp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        write_pack(&tmp, "bank-guard", &manifest_with("installed"));
        assert_eq!(load_installed_rules(&tmp).len(), 1);

        write_pack(&tmp, "bank-guard", &manifest_with("active"));
        assert!(
            load_installed_rules(&tmp).is_empty(),
            "an active rule belongs to a session, not to the filesystem"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// One broken pack must not take the browser down with it.
    #[test]
    fn a_pack_that_will_not_parse_is_skipped_not_fatal() {
        let tmp = std::env::temp_dir().join(format!("nf-sp-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        write_pack(&tmp, "broken", "this is not toml {{{");
        write_pack(&tmp, "bank-guard", &manifest_with("installed"));

        let rules = load_installed_rules(&tmp);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].pack, "bank-guard");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An active rule only applies while the session has chosen the pack, so it
    /// must not be picked up by the installed loader and must be picked up by
    /// the active one.
    #[test]
    fn an_active_rule_loads_only_for_a_pack_the_session_activated() {
        let tmp = std::env::temp_dir().join(format!("nf-sp-act-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write_pack(&tmp, "bank-guard", &manifest_with("active"));

        assert!(
            load_installed_rules(&tmp).is_empty(),
            "an active rule is not an installed rule"
        );
        assert!(
            load_active_rules(&tmp, &[]).is_empty(),
            "no pack activated means no active rules"
        );
        assert_eq!(
            load_active_rules(&tmp, &["bank-guard".to_string()]).len(),
            1
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Activating a pack that contributes only installed rules adds nothing
    /// twice: the installed loader already has them.
    #[test]
    fn activating_a_pack_does_not_duplicate_its_installed_rules() {
        let tmp = std::env::temp_dir().join(format!("nf-sp-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write_pack(&tmp, "bank-guard", &manifest_with("installed"));

        assert_eq!(load_installed_rules(&tmp).len(), 1);
        assert!(load_active_rules(&tmp, &["bank-guard".to_string()]).is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_missing_packs_directory_yields_no_rules() {
        let missing = std::env::temp_dir().join("nf-sp-definitely-not-here");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(load_installed_rules(&missing).is_empty());
    }

    /// The strictest rule decides, whichever pack contributed it and whatever
    /// order the packs happen to load in (I3).
    #[test]
    fn a_deny_from_any_pack_beats_an_ask_from_another() {
        let asker = SiteRule {
            pack: "polite".into(),
            urls: vec![],
            deny: vec![],
            ask: vec!["browser_click".into()],
            message: "just checking".into(),
        };
        let denier = SiteRule {
            pack: "strict".into(),
            urls: vec![],
            deny: vec!["browser_click".into()],
            ask: vec![],
            message: "absolutely not".into(),
        };

        for order in [vec![asker.clone(), denier.clone()], vec![denier, asker]] {
            let s = SitePolicyStage::new(order);
            match s.check(&call("browser_click"), &ctx(None)) {
                Verdict::Deny(d) => assert_eq!(d.pack.as_deref(), Some("strict")),
                other => panic!("the strictest rule must win, got {other:?}"),
            }
        }
    }
}
