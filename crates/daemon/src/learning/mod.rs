pub mod buffer;
pub mod collector;
pub mod conflict;
pub mod consolidator;
pub mod crypto;
pub mod decay;
pub mod export;
pub mod pipeline;
pub mod retriever;
pub mod session_extractor;
pub mod soul;
pub mod source;
pub mod sources;
pub mod types;

use std::sync::atomic::{AtomicBool, Ordering};

/// Process-wide gate for the learning system. Eval mode sets this true
/// during daemon boot (§7.2 feature 3) to prevent eval-generated
/// "facts" from being absorbed into SOUL.md / USER.md.
static DISABLED: AtomicBool = AtomicBool::new(false);

pub fn disable() {
    DISABLED.store(true, Ordering::SeqCst);
}

pub fn is_disabled() -> bool {
    DISABLED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enabled_then_disable_sticks() {
        // Process-global gate; this test must run alone (--test-threads=1).
        let prev = is_disabled();
        disable();
        assert!(is_disabled());
        // Restore for downstream tests (test runner may reuse process).
        DISABLED.store(prev, Ordering::SeqCst);
    }
}
