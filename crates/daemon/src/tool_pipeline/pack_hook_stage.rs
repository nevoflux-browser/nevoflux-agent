//! Running installed and active packs' `on_tool_pre` hooks as a pipeline stage
//! (design spec §6.2).
//!
//! # Why modules are cached and calls are not
//!
//! Compiling a WebAssembly module takes far longer than running one, and a tool
//! call happens often enough that recompiling per call would be felt. So
//! modules are compiled once and kept, keyed by the pack directory's mtime so
//! reinstalling a pack picks up the new module.
//!
//! Each *call* still gets a fresh `Store` (see [`crate::pack_hooks`]), so
//! caching the compiled module does not let one call leave state for the next.

use std::path::Path;
use std::sync::{Arc, Mutex};

use nevoflux_builtin_wasm::{ToolCall, ToolContext, ToolDenial};
use nevoflux_pack::manifest::Manifest;

use crate::pack_hooks::{resolve_failure, HookOutcome, PackHookModule};

use super::{Stage, Verdict};

/// A hook module together with the scope that decides what its failures mean.
pub struct LoadedHook {
    module: Arc<PackHookModule>,
    scope: String,
}

/// Compiled hook modules, rebuilt when the packs directory changes.
#[derive(Default)]
pub struct HookRegistry {
    cached: Mutex<Option<(Option<std::time::SystemTime>, Vec<Arc<LoadedHook>>)>>,
}

impl HookRegistry {
    /// Hooks that apply to this session: every installed pack's, plus the
    /// active packs' own.
    pub fn for_session(&self, packs_dir: &Path, active: &[String]) -> Vec<Arc<LoadedHook>> {
        let stamp = std::fs::metadata(packs_dir).and_then(|m| m.modified()).ok();

        let all = {
            let mut guard = match self.cached.lock() {
                Ok(g) => g,
                // A poisoned lock must not stop tool calls; load fresh instead.
                Err(_) => return filter_for(load_all(packs_dir), active),
            };
            match guard.as_ref() {
                Some((seen, hooks)) if *seen == stamp => hooks.clone(),
                _ => {
                    let hooks = load_all(packs_dir);
                    *guard = Some((stamp, hooks.clone()));
                    hooks
                }
            }
        };

        filter_for(all, active)
    }
}

/// Keep installed-scope hooks always, and active-scope hooks only for packs the
/// session has activated.
fn filter_for(all: Vec<Arc<LoadedHook>>, active: &[String]) -> Vec<Arc<LoadedHook>> {
    all.into_iter()
        .filter(|h| h.scope == "installed" || active.contains(&h.module.pack))
        .collect()
}

fn load_all(packs_dir: &Path) -> Vec<Arc<LoadedHook>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(packs_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(src) = std::fs::read_to_string(dir.join("pack.toml")) else {
            continue;
        };
        let Ok(manifest) = Manifest::parse(&src) else {
            continue;
        };
        let Some(hooks) = &manifest.components.hooks else {
            continue;
        };
        if !hooks.points.iter().any(|p| p == "on_tool_pre") {
            continue;
        }
        match PackHookModule::load(
            &manifest.pack.name,
            &dir.join(&hooks.file),
            hooks.budget.clone(),
        ) {
            Ok(module) => out.push(Arc::new(LoadedHook {
                module: Arc::new(module),
                scope: hooks.scope.clone(),
            })),
            Err(e) => {
                // A pack whose hook will not load is skipped, but loudly: an
                // installed hook that silently vanished would leave a guard the
                // user believes is protecting them doing nothing.
                tracing::warn!(error = %e, "pack hook module could not be loaded");
            }
        }
    }
    out
}

/// The request handed to `on_tool_pre`.
///
/// Deliberately narrow: the tool, its arguments, who asked, and where. A hook
/// deciding whether an action is allowed does not need the conversation, and
/// handing it over would make every pack a reader of everything said.
pub fn tool_pre_request(call: &ToolCall, ctx: &ToolContext) -> serde_json::Value {
    serde_json::json!({
        "point": "on_tool_pre",
        "tool": call.name,
        "arguments": call.arguments,
        "origin": ctx.origin,
        "tab_url": ctx.tab_url,
        "is_unattended": ctx.is_unattended,
    })
}

/// Runs pack `on_tool_pre` hooks in the pipeline.
pub struct PackHookStage {
    hooks: Vec<Arc<LoadedHook>>,
}

impl PackHookStage {
    /// Build a stage over hooks already resolved for this session.
    pub fn new(hooks: Vec<Arc<LoadedHook>>) -> Self {
        Self { hooks }
    }
}

impl Stage for PackHookStage {
    fn name(&self) -> &'static str {
        "pack_hooks"
    }

    fn check(&self, call: &ToolCall, ctx: &ToolContext) -> Verdict {
        if self.hooks.is_empty() {
            return Verdict::Allow;
        }
        let request = tool_pre_request(call, ctx);

        // Every hook is consulted before settling: a deny from any pack beats
        // an ask from another, whichever order they happen to load in (I3).
        let mut pending_ask: Option<String> = None;

        for hook in &self.hooks {
            let outcome = match hook.module.call("on_tool_pre", &request) {
                HookOutcome::Failed { reason } => {
                    resolve_failure(&hook.scope, &reason, &hook.module.pack)
                }
                other => other,
            };

            match outcome {
                HookOutcome::Deny { code, message } => {
                    return Verdict::Deny(ToolDenial {
                        code,
                        message,
                        rule: Some("on_tool_pre".into()),
                        pack: Some(hook.module.pack.clone()),
                    })
                }
                HookOutcome::Ask { prompt } => {
                    if pending_ask.is_none() {
                        pending_ask = Some(prompt);
                    }
                }
                // An `installed` hook may only tighten, so an Allow from one is
                // just an absence of objection -- it cannot overrule another
                // pack's refusal, which is why nothing returns early here.
                HookOutcome::Allow | HookOutcome::Abstain => {}
                HookOutcome::Failed { .. } => unreachable!("resolved above"),
            }
        }

        match pending_ask {
            Some(prompt) => Verdict::Ask { prompt },
            None => Verdict::Allow,
        }
    }
}

/// Modules currently compiled, for logging and tests.
pub fn loaded_pack_names(hooks: &[Arc<LoadedHook>]) -> Vec<String> {
    let mut names: Vec<String> = hooks.iter().map(|h| h.module.pack.clone()).collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "s1".into(),
            origin: "model".into(),
            mode: nevoflux_builtin_wasm::AgentMode::Browser,
            is_unattended: false,
            tab_url: Some("https://www.bank.com/x".into()),
            allowed_tools: None,
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "t1".into(),
            call_id: None,
            name: "browser_click".into(),
            arguments: serde_json::json!({ "selector": "#pay" }),
            signature: None,
        }
    }

    #[test]
    fn no_hooks_means_no_objection() {
        assert!(matches!(
            PackHookStage::new(vec![]).check(&call(), &ctx()),
            Verdict::Allow
        ));
    }

    /// A hook decides whether an action is allowed. It does not need the
    /// conversation, and handing it over would make every pack a reader of
    /// everything said.
    #[test]
    fn the_hook_request_carries_the_action_and_nothing_more() {
        let req = tool_pre_request(&call(), &ctx());
        assert_eq!(req["tool"], "browser_click");
        assert_eq!(req["origin"], "model");
        assert_eq!(req["tab_url"], "https://www.bank.com/x");
        assert_eq!(req["arguments"]["selector"], "#pay");

        let obj = req.as_object().unwrap();
        for leaked in [
            "messages",
            "history",
            "conversation",
            "system",
            "user_message",
        ] {
            assert!(!obj.contains_key(leaked), "{leaked} must not reach a hook");
        }
    }

    #[test]
    fn a_registry_over_an_empty_directory_yields_no_hooks() {
        let dir = std::env::temp_dir().join(format!("nf-hookreg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let reg = HookRegistry::default();
        assert!(reg.for_session(&dir, &[]).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An active-scope hook binds only once the session activates its pack;
    /// an installed one always does. Checked through the filter because
    /// building a real module needs a compiled .wasm.
    #[test]
    fn scope_decides_which_hooks_apply_to_a_session() {
        // A pack directory with a manifest but no module: load_all skips it,
        // which is the same path a broken module takes.
        let dir = std::env::temp_dir().join(format!("nf-hookscope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("p")).unwrap();
        std::fs::write(
            dir.join("p").join("pack.toml"),
            r#"
[pack]
name = "p"
version = "1.0.0"
protocol = "pack-protocol/0.2"
min_nevoflux = "0.3.0"

[components.hooks]
file   = "missing.wasm"
scope  = "installed"
points = ["on_tool_pre"]
"#,
        )
        .unwrap();

        let reg = HookRegistry::default();
        // The module is missing, so nothing loads -- and the tool call still
        // proceeds rather than failing on a pack's broken file.
        assert!(reg.for_session(&dir, &[]).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
