//! Running a pack's WebAssembly hooks (design spec §6).
//!
//! # This interface did not exist before
//!
//! The spec assumed hooks could reuse a "Pack WASM Code Interface". There was
//! none: `pack::capability` is an install-time path sandbox, and the ptr/len ABI
//! in `builtin-wasm` belongs to NevoFlux's own agent module, not to packs. What
//! did already exist is the sandbox substrate — `WasmConfig` carries fuel,
//! memory and epoch limits — so this builds the pack-facing layer on top of it
//! rather than from nothing.
//!
//! # The ABI
//!
//! A hook module exports:
//!
//! - `nf_alloc(len: u32) -> u32` — reserve `len` bytes and return the offset.
//!   The host writes the request JSON there.
//! - `nf_<point>(ptr: u32, len: u32) -> u64` — run the hook. The return value
//!   packs the response as `(offset << 32) | length`, pointing at response JSON
//!   in the same memory.
//!
//! Both sides speak JSON. It is not the tightest encoding, but a hook is called
//! once per tool call rather than in a hot loop, and a format a pack author can
//! print and read is worth more here than the bytes it costs.
//!
//! # What a failure means depends on scope
//!
//! An `installed` hook is a guard, so a trap, a timeout or a malformed answer
//! counts as a refusal: a guard that cannot run must not wave things through.
//! An `active` hook contributes context, so the same failures count as
//! abstention — losing an injection should not take down the turn. That is
//! invariant I6, and it is why [`HookOutcome::Failed`] is resolved by the
//! caller that knows the scope rather than here.

use std::path::Path;
use std::time::{Duration, Instant};

use nevoflux_pack::manifest::HookBudget;
use wasmtime::{Engine, Instance, Module, Store};

/// What a hook answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// The hook has no objection.
    Allow,
    /// The hook refuses, with a reason.
    Deny {
        /// Machine-readable code.
        code: String,
        /// Explanation for the model and the user.
        message: String,
    },
    /// The hook wants the user asked.
    Ask {
        /// The question.
        prompt: String,
    },
    /// The hook declined to decide.
    Abstain,
    /// The hook could not be run, or answered nonsense.
    ///
    /// Deliberately distinct from `Abstain`: whether this becomes a refusal or
    /// a shrug depends on the hook's scope, which this module does not know.
    Failed {
        /// What went wrong, for the log.
        reason: String,
    },
}

/// A loaded hook module, ready to be called.
///
/// `Debug` prints the pack and its budget, not the compiled module: a hook
/// appearing in a log line should identify itself without dumping bytecode.
pub struct PackHookModule {
    engine: Engine,
    module: Module,
    budget: HookBudget,
    /// Pack that supplied it, for logs and refusals.
    pub pack: String,
}

impl std::fmt::Debug for PackHookModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackHookModule")
            .field("pack", &self.pack)
            .field("budget_ms", &self.budget.ms)
            .field("budget_fuel", &self.budget.fuel)
            .finish_non_exhaustive()
    }
}

impl PackHookModule {
    /// Compile a pack's hook module.
    ///
    /// Compilation happens once; each call gets a fresh `Store`, so one call
    /// cannot leave state behind for the next. A hook is a decision function,
    /// not a service.
    pub fn load(pack: &str, wasm_path: &Path, budget: HookBudget) -> Result<Self, String> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);

        let engine = Engine::new(&config).map_err(|e| format!("hook engine for `{pack}`: {e}"))?;
        let bytes = std::fs::read(wasm_path)
            .map_err(|e| format!("hook module for `{pack}` at {}: {e}", wasm_path.display()))?;
        let module = Module::new(&engine, &bytes)
            .map_err(|e| format!("hook module for `{pack}` will not compile: {e}"))?;

        Ok(Self {
            engine,
            module,
            budget,
            pack: pack.to_string(),
        })
    }

    /// Call one hook point with a JSON request.
    ///
    /// Never returns an error: a hook that misbehaves produces
    /// [`HookOutcome::Failed`], which the caller resolves according to scope.
    /// Propagating an error here would let a broken pack fail a tool call in a
    /// way the scope rules were written to prevent.
    pub fn call(&self, point: &str, request: &serde_json::Value) -> HookOutcome {
        match self.try_call(point, request) {
            Ok(outcome) => outcome,
            Err(reason) => {
                tracing::warn!(pack = %self.pack, point, %reason, "pack hook failed");
                HookOutcome::Failed { reason }
            }
        }
    }

    fn try_call(&self, point: &str, request: &serde_json::Value) -> Result<HookOutcome, String> {
        let payload = serde_json::to_vec(request).map_err(|e| e.to_string())?;

        let mut store = Store::new(&self.engine, ());
        store
            .set_fuel(self.budget.fuel)
            .map_err(|e| format!("fuel: {e}"))?;

        // The wall clock catches what fuel cannot: a module blocked rather than
        // spinning. A watchdog thread bumps the epoch once the budget is spent,
        // which traps the call.
        let engine = self.engine.clone();
        let deadline = Duration::from_millis(self.budget.ms);
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_watch = done.clone();
        let watchdog = std::thread::spawn(move || {
            let started = Instant::now();
            while !done_watch.load(std::sync::atomic::Ordering::Relaxed) {
                if started.elapsed() >= deadline {
                    engine.increment_epoch();
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        store.set_epoch_deadline(1);

        let result = self.invoke(&mut store, point, &payload);

        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = watchdog.join();

        let body = result?;
        parse_outcome(&body)
    }

    fn invoke(
        &self,
        store: &mut Store<()>,
        point: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, String> {
        let instance = Instance::new(&mut *store, &self.module, &[])
            .map_err(|e| format!("instantiate: {e}"))?;

        let memory = instance
            .get_memory(&mut *store, "memory")
            .ok_or_else(|| "module exports no `memory`".to_string())?;

        let alloc = instance
            .get_typed_func::<u32, u32>(&mut *store, "nf_alloc")
            .map_err(|_| "module exports no `nf_alloc(len) -> ptr`".to_string())?;

        let ptr = alloc
            .call(&mut *store, payload.len() as u32)
            .map_err(|e| format!("nf_alloc trapped: {e}"))?;

        memory
            .write(&mut *store, ptr as usize, payload)
            .map_err(|e| format!("writing the request into guest memory: {e}"))?;

        let export = format!("nf_{point}");
        let hook = instance
            .get_typed_func::<(u32, u32), u64>(&mut *store, &export)
            .map_err(|_| format!("module exports no `{export}(ptr, len) -> packed`"))?;

        let packed = hook
            .call(&mut *store, (ptr, payload.len() as u32))
            .map_err(|e| format!("{export} trapped: {e}"))?;

        let out_ptr = (packed >> 32) as usize;
        let out_len = (packed & 0xffff_ffff) as usize;

        // A hook that answers nothing is abstaining, which is a legitimate
        // answer and not a failure.
        if out_len == 0 {
            return Ok(b"{\"verdict\":\"abstain\"}".to_vec());
        }

        // Guard the read: a module could return a length past the end of its
        // own memory, and a host that trusted it would read whatever follows.
        let data = memory.data(&*store);
        let end = out_ptr
            .checked_add(out_len)
            .ok_or_else(|| "response length overflows".to_string())?;
        if end > data.len() {
            return Err(format!(
                "response runs past the end of guest memory ({end} > {})",
                data.len()
            ));
        }

        Ok(data[out_ptr..end].to_vec())
    }
}

/// Parse a hook's JSON answer.
///
/// Unknown verdicts are a failure rather than a shrug: a pack that answers
/// something this kernel does not understand may believe it refused.
pub fn parse_outcome(body: &[u8]) -> Result<HookOutcome, String> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("hook answer is not JSON: {e}"))?;

    let verdict = v
        .get("verdict")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "hook answer has no `verdict`".to_string())?;

    match verdict {
        "allow" => Ok(HookOutcome::Allow),
        "abstain" => Ok(HookOutcome::Abstain),
        "ask" => Ok(HookOutcome::Ask {
            prompt: v
                .get("prompt")
                .and_then(|s| s.as_str())
                .unwrap_or("This pack wants to confirm an action.")
                .to_string(),
        }),
        "deny" => Ok(HookOutcome::Deny {
            code: v
                .get("code")
                .and_then(|s| s.as_str())
                .unwrap_or("PACK_HOOK_DENIED")
                .to_string(),
            message: v
                .get("message")
                .and_then(|s| s.as_str())
                .unwrap_or("a pack hook refused this action")
                .to_string(),
        }),
        other => Err(format!("hook answered an unknown verdict `{other}`")),
    }
}

/// Resolve a [`HookOutcome::Failed`] according to the hook's scope
/// (invariant I6).
///
/// A guard that cannot run refuses; an injection that cannot run is skipped.
pub fn resolve_failure(scope: &str, reason: &str, pack: &str) -> HookOutcome {
    if scope == "installed" {
        HookOutcome::Deny {
            code: "PACK_HOOK_UNAVAILABLE".into(),
            message: format!("`{pack}` guards this action and its hook could not run: {reason}"),
        }
    } else {
        HookOutcome::Abstain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_verdicts_parse() {
        assert_eq!(
            parse_outcome(br#"{"verdict":"allow"}"#).unwrap(),
            HookOutcome::Allow
        );
        assert_eq!(
            parse_outcome(br#"{"verdict":"abstain"}"#).unwrap(),
            HookOutcome::Abstain
        );
        assert_eq!(
            parse_outcome(br#"{"verdict":"ask","prompt":"ok?"}"#).unwrap(),
            HookOutcome::Ask {
                prompt: "ok?".into()
            }
        );
        assert_eq!(
            parse_outcome(br#"{"verdict":"deny","code":"NOPE","message":"no"}"#).unwrap(),
            HookOutcome::Deny {
                code: "NOPE".into(),
                message: "no".into()
            }
        );
    }

    /// A deny with no detail still refuses, and still says something a model
    /// can act on.
    #[test]
    fn a_bare_deny_gets_a_usable_default() {
        match parse_outcome(br#"{"verdict":"deny"}"#).unwrap() {
            HookOutcome::Deny { code, message } => {
                assert_eq!(code, "PACK_HOOK_DENIED");
                assert!(!message.is_empty());
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    /// A pack answering something this kernel does not understand may believe
    /// it refused, so an unknown verdict is a failure rather than a shrug.
    #[test]
    fn an_unknown_verdict_is_a_failure_not_an_abstention() {
        assert!(parse_outcome(br#"{"verdict":"maybe"}"#).is_err());
        assert!(parse_outcome(br#"{}"#).is_err());
        assert!(parse_outcome(b"not json").is_err());
    }

    /// Invariant I6: a guard that cannot run refuses; an injection that cannot
    /// run is skipped.
    #[test]
    fn a_failure_resolves_by_scope() {
        match resolve_failure("installed", "timed out", "bank-guard") {
            HookOutcome::Deny { code, message } => {
                assert_eq!(code, "PACK_HOOK_UNAVAILABLE");
                assert!(message.contains("bank-guard"));
                assert!(message.contains("timed out"));
            }
            other => panic!("an installed hook must refuse, got {other:?}"),
        }
        assert_eq!(
            resolve_failure("active", "timed out", "jobhunt"),
            HookOutcome::Abstain
        );
    }

    #[test]
    fn a_module_that_will_not_compile_is_reported_by_pack_name() {
        let dir = std::env::temp_dir().join(format!("nf-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.wasm");
        std::fs::write(&path, b"this is not wasm").unwrap();

        let err = PackHookModule::load("bad-pack", &path, HookBudget::default()).unwrap_err();
        assert!(err.contains("bad-pack"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_module_is_reported_by_path() {
        let missing = std::env::temp_dir().join("nf-hook-definitely-absent.wasm");
        let _ = std::fs::remove_file(&missing);
        let err = PackHookModule::load("p", &missing, HookBudget::default()).unwrap_err();
        assert!(err.contains("nf-hook-definitely-absent"), "{err}");
    }
}
