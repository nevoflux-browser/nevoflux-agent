//! The LocalOnly latch and its egress guard.
//!
//! When on-device inference is the active, enabled provider, the latch is
//! "on" and every non-loopback network request an LLM call would otherwise
//! make is refused before it leaves the process — see [`egress_guard`].
//! [`refresh_from_config`] is the only writer during normal operation; it is
//! called whenever the config changes (startup, `config.set`, config-file
//! watch reload) so the latch always tracks the current `[llm]` section
//! rather than a snapshot taken at startup.
//!
//! ## Test isolation (R26)
//!
//! In production [`is_on`] / [`set`] read and write one process-global
//! [`LATCH`]. Under `cfg(test)`, dozens of call sites across the daemon
//! crate now read [`is_on`] (goal/loop/schedule dispatch, summarization,
//! model-override validation, …) — far more than when [`LATCH`] had only its
//! own two direct tests — so a bare global would make any test that flips it
//! race every *other*, unrelated test running concurrently in `cargo test`'s
//! default multi-threaded harness, even one that never touches this module.
//!
//! The fix: in test builds, [`set`] writes a **thread-local** override
//! ([`TEST_LATCH`]) instead of the real global, and [`is_on`] prefers that
//! override when present, falling back to the real global only when the
//! calling thread has never called [`set`]. `#[tokio::test]`'s default
//! `current_thread` flavor runs a test function and everything it
//! `tokio::spawn`s on the SAME OS thread, so a dispatcher/tick task spawned
//! inside a latch test still observes that test's override — do not switch
//! a latch-sensitive test to `flavor = "multi_thread"`, which would move
//! spawned work to a different thread with no override of its own.
//!
//! [`refresh_from_config`] is the one production caller whose effect must be
//! visible to every thread (a config change), so it always writes the real
//! global via [`set_global`], bypassing the thread-local even in test
//! builds. The handful of tests that must exercise that real-global path
//! (currently just this module's own `refresh_tracks_*` tests) use
//! [`set_global_for_test`] instead of [`set`], and keep holding
//! [`test_serial`] for the duration exactly as before thread-local isolation
//! existed — every other latch-touching test in the crate no longer needs
//! to worry about racing them.

use std::sync::atomic::{AtomicBool, Ordering};

use nevoflux_llm::ProviderType;

/// EventBus topic a latch *transition* (not every refresh — see
/// [`refresh_from_config`]'s `Some`/`None` return) is published on, sticky,
/// by [`crate::local::on_config_changed`]. Payload:
/// `{on, paused_loops, paused_schedules, paused_goals}`.
///
/// Defined here (R2) rather than in `crate::local::sync` so a later task
/// that needs the topic string doesn't have to know which submodule
/// implements the publish. Reachable as `crate::local::latch::TOPIC_LATCH`;
/// no `crate::local`-level re-export exists yet (left for whichever task
/// actually needs one — see `local/mod.rs`'s module doc).
pub const TOPIC_LATCH: &str = "system:local:latch_changed";

/// Global on/off state. `SeqCst` throughout: this is touched rarely (config
/// changes) and read on every LLM call, so there is no throughput reason to
/// weaken the ordering, and a stray egress check racing a `refresh` is
/// exactly the kind of bug a stronger-than-necessary ordering is cheap
/// insurance against.
static LATCH: AtomicBool = AtomicBool::new(false);

/// Test-only per-thread override for [`is_on`]/[`set`] — see the module
/// docs' "Test isolation" section. `None` means "no override on this
/// thread": fall through to the real [`LATCH`].
#[cfg(test)]
thread_local! {
    static TEST_LATCH: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn test_override() -> Option<bool> {
    TEST_LATCH.with(|c| c.get())
}

#[cfg(not(test))]
fn test_override() -> Option<bool> {
    None
}

/// Whether the LocalOnly latch is currently on: the calling thread's test
/// override if one is set (test builds only), else the real global.
pub fn is_on() -> bool {
    if let Some(v) = test_override() {
        return v;
    }
    LATCH.load(Ordering::SeqCst)
}

/// The raw global swap, bypassing the test thread-local override. The only
/// production caller is [`refresh_from_config`], whose effect must be
/// visible to every thread; [`set_global_for_test`] exposes this to the
/// handful of tests that need the same.
fn set_global(on: bool) -> bool {
    LATCH.swap(on, Ordering::SeqCst)
}

/// Set the latch, returning its previous EFFECTIVE value (what [`is_on`]
/// would have returned just before this call).
///
/// In test builds this writes ONLY the calling thread's override — see the
/// module docs — never the real global, so unrelated concurrently-running
/// tests can't observe it. Production builds swap the real global directly.
#[cfg(not(test))]
pub fn set(on: bool) -> bool {
    set_global(on)
}

#[cfg(test)]
pub fn set(on: bool) -> bool {
    let prev = is_on();
    TEST_LATCH.with(|c| c.set(Some(on)));
    prev
}

/// Test-only escape hatch that drives the REAL global latch rather than the
/// calling thread's override — for the few tests (see module docs) that
/// specifically exercise global-visibility behavior, e.g.
/// [`refresh_from_config`]. Callers must hold [`test_serial`] (purely
/// synchronous tests) or [`test_serial_async`] (`#[tokio::test]`s that
/// need the real global to stay stable across their own `.await`s — see
/// R35) for the duration.
#[cfg(test)]
pub fn set_global_for_test(on: bool) -> bool {
    set_global(on)
}

/// Recompute the latch from `cfg` and apply it if it changed.
///
/// The latch is on iff the active provider resolves to
/// [`ProviderType::Local`] (which also accepts the `"on-device"` /
/// `"ondevice"` aliases, since resolution goes through
/// [`nevoflux_llm::ProviderType::from_str`] rather than a literal string
/// compare) **and** `[llm.local].enabled` is true. Returns `Some(new)` iff
/// the latch's value changed, `None` if it was already at the computed
/// value — so callers can log/broadcast only on an actual transition.
///
/// Always writes the real global (via [`set_global`]) even in test builds —
/// a config change must be visible process-wide, not just to the calling
/// thread. See the module docs' "Test isolation" section.
pub fn refresh_from_config(cfg: &crate::config::AgentConfig) -> Option<bool> {
    let new_on = cfg
        .llm
        .active_provider()
        .and_then(|p| cfg.llm.resolve_wire(p))
        == Some(ProviderType::Local)
        && cfg.llm.local.enabled;
    let prev = set_global(new_on);
    (prev != new_on).then_some(new_on)
}

/// A network request was refused because the LocalOnly latch is on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("On-device mode is on: requests to `{provider}` are blocked so conversation content stays on this device")]
pub struct LocalOnlyRefused {
    pub provider: String,
}

/// Providers that shell out to their own CLI rather than have this process
/// make the HTTP request.
///
/// A subprocess is never loopback-exempt: the daemon does not control what
/// host the CLI itself talks to (and for `ClaudeCode` / `GeminiCli` /
/// `OpenClaw` / `Antigravity` it is always the vendor's cloud), so a
/// `base_url` pointing at loopback proves nothing about where the CLI's own
/// traffic goes.
fn is_subprocess_provider(provider: ProviderType) -> bool {
    use ProviderType::*;
    matches!(
        provider,
        ClaudeCode | GeminiCli | KimiAgent | OpenClaw | Antigravity
    )
}

/// Decide whether a request to `provider` is allowed given latch state
/// `latched`. Pure — no global state — so it is exhaustively unit-testable;
/// [`egress_guard`] is the thin wrapper that reads the actual latch.
pub fn egress_decision(
    latched: bool,
    provider: ProviderType,
    base_url: Option<&str>,
) -> Result<(), LocalOnlyRefused> {
    if !latched || provider == ProviderType::Local {
        return Ok(());
    }
    if !is_subprocess_provider(provider) {
        if let Some(url) = base_url {
            if is_loopback_url(url) {
                return Ok(());
            }
        }
    }
    Err(LocalOnlyRefused {
        provider: format!("{provider:?}"),
    })
}

/// Refuse a request to `provider` if the LocalOnly latch is on and the
/// request isn't exempt (see [`egress_decision`]).
pub fn egress_guard(
    provider: ProviderType,
    base_url: Option<&str>,
) -> Result<(), LocalOnlyRefused> {
    egress_decision(is_on(), provider, base_url)
}

/// Whether `url`'s host is loopback: `127.0.0.0/8`, `[::1]`, or `localhost`.
///
/// An unparseable URL or missing host is treated as not loopback (the
/// conservative answer — [`egress_decision`] then refuses it).
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // `Url::host_str` brackets an IPv6 literal (e.g. "[::1]"); strip that
    // before handing it to `IpAddr::parse`, which doesn't accept brackets.
    let candidate = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    candidate
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Serialize tests that touch the process-global [`LATCH`] directly (via
/// [`set_global_for_test`] / [`refresh_from_config`]) rather than through the
/// per-thread [`set`]/[`is_on`] override — currently just this module's own
/// `refresh_tracks_*` tests. `cargo test` runs `#[test]` functions on
/// multiple threads by default, so without this they would race each
/// other's global writes.
///
/// **R35: never hold this guard across an `.await`.** It wraps a plain
/// `std::sync::Mutex`; holding it across an await point in an async test
/// blocks the OS thread the test harness assigned that test for however
/// long the awaited work takes, which starves unrelated timing-sensitive
/// tests elsewhere in the binary (observed in practice: `/loop`'s
/// dispatcher tests, which use real sleeps, flaked whenever a
/// `#[tokio::test]` here held this guard across an `.await`). A purely
/// synchronous `#[test]` (this module's own `refresh_tracks_*` tests) is
/// fine — there is no await to hold it across. An async test that needs
/// exclusivity for its whole body wants [`test_serial_async`] instead;
/// one that only needs a single synchronous mutation (e.g.
/// `set_global_for_test`) should scope this guard to a `{ }` block around
/// just that call.
#[cfg(test)]
pub fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    TEST_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Async-safe sibling of [`test_serial`], for `#[tokio::test]` functions
/// that need exclusive access to shared test-only global state — the
/// real [`LATCH`], the `endpoint` registry, ... — for their WHOLE async
/// body, not just a single synchronous mutation (R35).
///
/// A `tokio::sync::Mutex`, unlike [`test_serial`]'s `std::sync::Mutex`,
/// is designed to be held across `.await` points: a contended lock
/// suspends (yields) the current task back to the runtime's scheduler
/// instead of blocking the underlying OS thread, so holding it for a
/// test's whole duration can't starve unrelated tests the way holding a
/// std mutex that long would.
///
/// Deliberately a *separate* mutex from [`test_serial`]'s, not a
/// std/tokio-flavored view onto the same one (the two primitives can't
/// share a single lock object) — tests that need whole-body exclusivity
/// should use this one consistently rather than mixing it with
/// [`test_serial`] for the same resource, or the two groups won't
/// actually exclude each other.
///
/// ## Global lock order when a test needs this AND `endpoint`'s (fix round 3)
///
/// This mutex and `crate::local::endpoint::test_serial_async`'s guard the
/// latch and the `endpoint` registry respectively — two independent
/// resources, split apart (see that function's doc comment) so tests that
/// only touch one don't serialize behind tests that only touch the other.
/// But some tests legitimately touch *both* in one body: anything that
/// drives the latch on and then lets production code read the endpoint
/// registry to decide the resulting upstream (`local::sync`'s
/// `apply_gateway_upstream_for_latch_locked` calls
/// `crate::llm_gateway::upstream_for_local(endpoint::current().as_ref())`
/// while latched) needs both locks held for its whole body, or a
/// concurrently-running endpoint-only test can publish/clear an endpoint
/// in the gap and flip that read out from under it.
///
/// Every such test MUST acquire **this lock first, then `endpoint`'s**
/// (`local::sync`'s `on_config_changed_with_wires_gateway_upstream_and_publishes_on_transitions`
/// and `concurrent_alternating_latch_toggles_leave_upstream_matching_final_latch_state`
/// do this). Acquiring both is fine — they're independent mutexes with no
/// cyclic wait — as long as *every* test that needs both follows the same
/// order; picking a single global order here rules out a deadlock between
/// two such tests by construction. Do not introduce a test that takes
/// `endpoint`'s guard first and this one second.
#[cfg(test)]
pub async fn test_serial_async() -> tokio::sync::MutexGuard<'static, ()> {
    static TEST_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    TEST_MUTEX.lock().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_llm::ProviderType::*;

    /// Keeps [`ProviderType`] exhaustive here: a new variant fails this
    /// function to compile until `only_local_or_loopback_passes_when_latched`
    /// below is updated to cover it too.
    #[allow(dead_code)]
    fn _exhaustive(p: nevoflux_llm::ProviderType) {
        match p {
            Anthropic | OpenAi | OpenRouter | DeepSeek | Qwen | Gemini | Groq | Ollama
            | Mistral | XAi | Cohere | Perplexity | Together | ClaudeCode | GeminiCli
            | KimiAgent | OpenClaw | Antigravity | Local => {}
        }
    }

    #[test]
    fn only_local_or_loopback_passes_when_latched() {
        use nevoflux_llm::ProviderType::*;
        let all = [
            Anthropic,
            OpenAi,
            OpenRouter,
            DeepSeek,
            Qwen,
            Gemini,
            Groq,
            Ollama,
            Mistral,
            XAi,
            Cohere,
            Perplexity,
            Together,
            ClaudeCode,
            GeminiCli,
            KimiAgent,
            OpenClaw,
            Antigravity,
            Local,
        ];
        for p in all {
            let r = egress_decision(true, p, None);
            assert_eq!(r.is_ok(), p == Local, "{p:?} with no base_url");
            assert!(egress_decision(false, p, None).is_ok());
        }
        // OpenAI-wire custom endpoint on loopback (user's own Ollama) is allowed
        assert!(egress_decision(true, OpenAi, Some("http://127.0.0.1:11434/v1")).is_ok());
        assert!(egress_decision(true, OpenAi, Some("http://localhost:8080")).is_ok());
        // ACP providers are never loopback-exempt: their CLI talks to the cloud itself
        assert!(egress_decision(true, ClaudeCode, Some("http://127.0.0.1:1")).is_err());
        assert!(egress_decision(true, OpenAi, Some("https://api.openai.com/v1")).is_err());
        assert!(egress_decision(true, OpenAi, Some("http://127.0.0.1.evil.com/v1")).is_err());
    }

    /// Exercises `refresh_from_config`'s real-global write path — uses
    /// `set_global_for_test` (never the thread-local `set`) throughout so
    /// `is_on()` reads through to the same global `refresh_from_config`
    /// touches, under `test_serial()` per the module docs.
    #[test]
    fn refresh_tracks_active_provider_and_enabled() {
        let _g = test_serial();
        let mut cfg = crate::config::AgentConfig::default();
        set_global_for_test(false);
        cfg.llm.provider = Some("local".into());
        assert_eq!(refresh_from_config(&cfg), None); // not enabled -> stays off
        cfg.llm.local.enabled = true;
        assert_eq!(refresh_from_config(&cfg), Some(true));
        assert!(is_on());
        cfg.llm.provider = Some("anthropic".into());
        assert_eq!(refresh_from_config(&cfg), Some(false));
        // "on-device" is an accepted alias for "local" (ProviderType::from_str)
        cfg.llm.provider = Some("on-device".into());
        assert_eq!(refresh_from_config(&cfg), Some(true));
        set_global_for_test(false);
    }

    /// The thread-local override ([`set`]/[`is_on`]) is invisible to
    /// [`set_global_for_test`]/[`refresh_from_config`]'s real-global path —
    /// this is exactly what makes the two safe to run concurrently with
    /// every other latch-touching test in the crate (R26).
    #[test]
    fn thread_local_override_does_not_leak_into_the_real_global() {
        let _g = test_serial();
        set_global_for_test(false);

        set(true); // thread-local only
        assert!(is_on());
        assert!(
            !LATCH.load(Ordering::SeqCst),
            "the real global must be untouched by the thread-local override"
        );

        set(false);
        set_global_for_test(false);
    }
}
