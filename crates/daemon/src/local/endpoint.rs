//! Global registry publishing the currently-running on-device engine's
//! connection details.
//!
//! The engine supervisor (a later task, 2.9) launches the local inference
//! process out-of-band and calls [`publish`] once it is ready to accept
//! requests; it calls `publish(None)` again when the engine is unloaded
//! (idle timeout, crash, shutdown). [`crate::wasm::local_llm`] reads
//! whatever is currently published — via [`ensure`] — to build its request,
//! rather than holding its own reference, so it always sees the latest
//! endpoint even across an engine restart mid-conversation.
//!
//! [`install_ensure`] lets that same supervisor register a cold-start hook:
//! when nothing is published yet, [`ensure`] calls it instead of failing
//! outright, so the first request after startup (or after an idle unload)
//! can launch the engine on demand instead of erroring.

use std::sync::{Arc, OnceLock, PoisonError, RwLock};

/// Connection details for the currently-running on-device inference engine.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalEndpoint {
    /// OpenAI-compatible base URL, e.g. `http://127.0.0.1:PORT/v1`.
    pub base_url: String,
    /// Bearer token the engine expects (a local-only secret, not a vendor key).
    pub api_key: String,
    /// The context size the engine was actually launched with.
    pub n_ctx: u32,
    /// The model id to send on the wire (the engine's `--model` alias).
    pub model_id: String,
    /// Whether the model's chat template is a hybrid thinking/non-thinking
    /// one, requiring `chat_template_kwargs.enable_thinking` on requests.
    pub thinking_hybrid: bool,
}

fn slot() -> &'static RwLock<Option<LocalEndpoint>> {
    static ENDPOINT: OnceLock<RwLock<Option<LocalEndpoint>>> = OnceLock::new();
    ENDPOINT.get_or_init(|| RwLock::new(None))
}

/// The currently published endpoint, or `None` if the engine isn't running.
pub fn current() -> Option<LocalEndpoint> {
    slot()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Publish the currently-running engine's endpoint, or clear it with `None`.
pub fn publish(ep: Option<LocalEndpoint>) {
    *slot().write().unwrap_or_else(PoisonError::into_inner) = ep;
}

/// Hook the supervisor installs (Task 2.9) to cold-start the engine on demand.
pub type EnsureFn = Arc<
    dyn Fn() -> futures::future::BoxFuture<'static, Result<LocalEndpoint, String>> + Send + Sync,
>;

fn ensure_hook_slot() -> &'static RwLock<Option<EnsureFn>> {
    static HOOK: OnceLock<RwLock<Option<EnsureFn>>> = OnceLock::new();
    HOOK.get_or_init(|| RwLock::new(None))
}

/// Install the cold-start hook [`ensure`] falls back to when nothing is
/// currently published.
pub fn install_ensure(f: EnsureFn) {
    *ensure_hook_slot()
        .write()
        .unwrap_or_else(PoisonError::into_inner) = Some(f);
}

/// The current endpoint, cold-starting the engine via the installed hook if
/// nothing is published yet.
///
/// Resolution order: [`current`] if `Some`, else the installed
/// [`EnsureFn`] hook, else an error — there is no engine running and
/// nothing that can start one.
pub async fn ensure() -> Result<LocalEndpoint, String> {
    if let Some(ep) = current() {
        return Ok(ep);
    }
    let hook = ensure_hook_slot()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match hook {
        Some(f) => f().await,
        None => Err("On-device engine is not running".to_string()),
    }
}

/// Async-safe test-serialization mutex for this module's shared registry
/// — the `endpoint` sibling of [`crate::local::latch::test_serial_async`],
/// deliberately a *separate* lock from that one (fix round 1 follow-up,
/// R35 tuning): a test that only touches `publish`/`current`/`ensure`
/// doesn't need to also wait its turn behind every test that instead
/// touches the real `LocalOnly` latch (a completely different resource),
/// and vice versa — sharing one lock across both domains was measured to
/// serialize the two test groups into one long chain for no correctness
/// benefit, which is exactly the kind of added contention R35 exists to
/// avoid. See [`crate::local::latch::test_serial_async`]'s own doc
/// comment for why a tokio (not std) mutex.
///
/// ## Global lock order with `latch::test_serial_async` (fix round 3)
///
/// A test that needs both this lock and [`crate::local::latch::test_serial_async`]'s
/// (because it drives the latch on and then exercises production code that
/// reads the endpoint registry to decide the resulting upstream — see
/// `local::sync::apply_gateway_upstream_for_latch_locked`) MUST acquire
/// `latch`'s guard first and this one second, for its whole body. See the
/// full explanation and rationale on `latch::test_serial_async`'s doc
/// comment; the order is documented once, there, as the single source of
/// truth, so it can't drift between the two modules.
#[cfg(test)]
pub(crate) async fn test_serial_async() -> tokio::sync::MutexGuard<'static, ()> {
    static TEST_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    TEST_MUTEX.lock().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(model: &str) -> LocalEndpoint {
        LocalEndpoint {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "k".into(),
            n_ctx: 16384,
            model_id: model.into(),
            thinking_hybrid: false,
        }
    }

    // `#[tokio::test]`, not `#[test]`, purely so this can take the async
    // `test_serial_async()` — the body itself has no real `.await` work,
    // so this costs essentially nothing, but it closes the residual race
    // a plain sync test (unable to hold the same lock as its async
    // siblings below) would otherwise have against them.
    #[tokio::test]
    async fn publish_and_current_round_trip() {
        let _g = test_serial_async().await;
        assert_eq!(current(), None);
        publish(Some(ep("a")));
        assert_eq!(current(), Some(ep("a")));
        publish(None);
        assert_eq!(current(), None);
    }

    #[tokio::test]
    async fn ensure_returns_current_without_consulting_the_hook() {
        // R35 (fix round 1 follow-up tuning): this test needs the shared
        // `endpoint` registry to stay exactly what it published for its
        // whole body (another concurrently-running endpoint test could
        // otherwise overwrite it in the gap before `ensure().await` reads
        // it back — observed empirically as a real failure, not just
        // theoretical), so it holds [`test_serial_async`] — a tokio
        // mutex scoped to JUST this module's registry (not
        // `crate::local::latch`'s), so this doesn't serialize behind
        // unrelated real-latch tests too.
        let _g = test_serial_async().await;
        publish(Some(ep("b")));
        assert_eq!(ensure().await, Ok(ep("b")));
        publish(None);
    }

    #[tokio::test]
    async fn ensure_falls_back_to_the_installed_hook_when_nothing_is_published() {
        // R35: see `ensure_returns_current_without_consulting_the_hook`'s
        // comment above.
        let _g = test_serial_async().await;
        publish(None);
        install_ensure(Arc::new(|| Box::pin(async { Ok(ep("cold-started")) })));
        assert_eq!(ensure().await, Ok(ep("cold-started")));
        publish(None);
    }
}
