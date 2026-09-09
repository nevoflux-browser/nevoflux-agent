//! Turning a manifest into the risk checklist shown at install time
//! (design spec §4.3.3).
//!
//! # Why the defaults are graded rather than uniform
//!
//! Install prompts get clicked through. A checklist where everything is
//! pre-ticked teaches people to click Install without reading, which makes the
//! dangerous entries invisible precisely because they sit next to harmless
//! ones. So reading is listed and pre-ticked, writing is listed and pre-ticked
//! but marked, and anything with consequences outside the session has to be
//! ticked by hand.
//!
//! The two entries that can never be pre-ticked are the ones a user cannot
//! undo by uninstalling later: a full prompt replacement, and unattended
//! side effects.

use crate::manifest::Manifest;

/// How alarming an entry is, which decides how it is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Reads something. Listed for completeness.
    Read,
    /// Writes something the user can inspect and undo.
    Write,
    /// Has consequences outside the session — submitting, sending, paying,
    /// deleting.
    SideEffect,
    /// Applies to every session, not just the ones that activate the pack.
    AlwaysOn,
    /// Can rewrite what the assistant is told to do.
    PromptControl,
}

/// One line of the install checklist.
#[derive(Debug, Clone)]
pub struct ChecklistItem {
    /// How alarming it is.
    pub level: Level,
    /// Short label.
    pub label: String,
    /// The specific capability or rule behind it.
    pub detail: String,
    /// Whether the box starts ticked.
    pub default_checked: bool,
    /// Whether install is refused unless the user ticks it themselves.
    pub requires_explicit: bool,
}

/// Build the checklist for a manifest.
///
/// `signed` relaxes exactly one default: a signed pack may pre-tick its
/// unattended side effects, because someone with a name attached vouched for
/// it. Everything else grades the same either way — signing says who wrote it,
/// not that the user wanted it.
pub fn build(manifest: &Manifest, signed: bool) -> Vec<ChecklistItem> {
    let mut items = Vec::new();
    let p = &manifest.permissions;

    for cap in &p.tools {
        let level = if cap.ends_with(".read") || cap.contains("read") {
            Level::Read
        } else {
            Level::Write
        };
        items.push(ChecklistItem {
            level,
            label: match level {
                Level::Read => "Read".to_string(),
                _ => "Write".to_string(),
            },
            detail: cap.clone(),
            // Reads and writes are undoable by uninstalling; they start ticked
            // so the list does not train people to tick everything.
            default_checked: true,
            requires_explicit: false,
        });
    }

    for effect in &p.side_effects {
        items.push(ChecklistItem {
            level: Level::SideEffect,
            label: "Acts outside this session".to_string(),
            detail: effect.clone(),
            // Unattended means nobody is watching when it fires, so it is not
            // pre-ticked unless a signature vouches for the pack.
            default_checked: signed && p.unattended,
            requires_explicit: p.unattended && !signed,
        });
    }

    if p.unattended {
        items.push(ChecklistItem {
            level: Level::SideEffect,
            label: "Runs with nobody watching".to_string(),
            detail: "may run inside loops and schedules".to_string(),
            default_checked: signed,
            requires_explicit: !signed,
        });
    }

    // `installed` scope survives deactivation, so it is called out separately
    // from anything the user can switch off by not activating the pack.
    let always_on = manifest
        .components
        .tool_policy
        .iter()
        .filter(|tp| tp.scope == "installed")
        .count();
    if always_on > 0 {
        items.push(ChecklistItem {
            level: Level::AlwaysOn,
            label: "Applies to every session".to_string(),
            detail: format!("{always_on} rule(s) active whether or not you use this pack"),
            // These only ever tighten, so they are safe to pre-tick — the
            // point of listing them is that the user knows they are there.
            default_checked: true,
            requires_explicit: false,
        });
    }

    if let Some(prompt) = &manifest.components.prompt {
        match prompt.replace.as_str() {
            "keep_kernel" => items.push(ChecklistItem {
                level: Level::PromptControl,
                label: "Can replace the assistant's task instructions".to_string(),
                detail: "keeps the kernel sections".to_string(),
                default_checked: false,
                requires_explicit: true,
            }),
            "full" => items.push(ChecklistItem {
                level: Level::PromptControl,
                label: "Can replace the assistant's whole system prompt".to_string(),
                detail: "including the tool protocol; privacy protection is enforced in code and is not affected".to_string(),
                // Never pre-ticked, signed or not. A signature says who wrote
                // it, not that this user wanted to hand over the prompt.
                default_checked: false,
                requires_explicit: true,
            }),
            _ => {}
        }
    }

    items
}

/// Whether a pack asks for nothing at all, and so needs no checklist.
pub fn is_empty_ask(manifest: &Manifest) -> bool {
    build(manifest, false).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(extra: &str) -> Manifest {
        let src = format!(
            r#"
[pack]
name = "p"
version = "1.0.0"
protocol = "pack-protocol/0.2"
min_nevoflux = "0.3.0"
{extra}
"#
        );
        Manifest::parse(&src).unwrap()
    }

    #[test]
    fn a_pack_that_asks_for_nothing_needs_no_checklist() {
        assert!(is_empty_ask(&parse("")));
    }

    #[test]
    fn reads_and_writes_are_graded_apart() {
        let m = parse("[permissions]\ntools = [\"browser.read\", \"fs.session\"]\n");
        let items = build(&m, false);
        assert_eq!(items[0].level, Level::Read);
        assert_eq!(items[1].level, Level::Write);
        assert!(items.iter().all(|i| i.default_checked));
        assert!(items.iter().all(|i| !i.requires_explicit));
    }

    /// The two entries a user cannot undo by uninstalling later are never
    /// pre-ticked, whoever signed the pack.
    #[test]
    fn a_full_prompt_replacement_is_never_preticked() {
        for signed in [false, true] {
            let m = parse("[components.prompt]\nreplace = \"full\"\n");
            let item = build(&m, signed)
                .into_iter()
                .find(|i| i.level == Level::PromptControl)
                .unwrap();
            assert!(!item.default_checked, "signed={signed}");
            assert!(item.requires_explicit, "signed={signed}");
            assert!(
                item.detail.contains("privacy"),
                "the user should be told the privacy guarantee still holds"
            );
        }
    }

    #[test]
    fn keep_kernel_also_has_to_be_ticked_by_hand() {
        let m = parse("[components.prompt]\nreplace = \"keep_kernel\"\n");
        let item = build(&m, false)
            .into_iter()
            .find(|i| i.level == Level::PromptControl)
            .unwrap();
        assert!(!item.default_checked);
        assert!(item.requires_explicit);
    }

    /// Signing relaxes exactly one default: unattended side effects. It says
    /// who wrote the pack, not that this user wanted it.
    #[test]
    fn signing_relaxes_only_the_unattended_default() {
        let src = "[permissions]\nunattended = true\nside_effects = [\"browser_submit\"]\n";

        let unsigned = build(&parse(src), false);
        assert!(unsigned.iter().all(|i| !i.default_checked));
        assert!(unsigned.iter().all(|i| i.requires_explicit));

        let signed = build(&parse(src), true);
        assert!(signed.iter().all(|i| i.default_checked));
        assert!(signed.iter().all(|i| !i.requires_explicit));
    }

    /// A side effect in an attended pack still gets listed, but the user is
    /// there when it fires, so it does not have to be pre-authorised.
    #[test]
    fn an_attended_side_effect_is_listed_without_demanding_a_tick() {
        let m = parse("[permissions]\nside_effects = [\"browser_submit\"]\n");
        let item = &build(&m, false)[0];
        assert_eq!(item.level, Level::SideEffect);
        assert!(!item.requires_explicit);
    }

    #[test]
    fn installed_scope_rules_are_called_out_separately() {
        let m = parse(
            "[[components.tool_policy]]\nscope = \"installed\"\ndeny = [\"browser_click\"]\n",
        );
        let item = build(&m, false)
            .into_iter()
            .find(|i| i.level == Level::AlwaysOn)
            .unwrap();
        assert!(item.detail.contains("whether or not"));
        // They only ever tighten, so listing is the point, not consent.
        assert!(item.default_checked);
    }
}
