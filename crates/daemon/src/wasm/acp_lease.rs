//! One kind of turn per ACP provider at a time (M3 contract v1.3 R38).
//!
//! The provider's `McpToolBridge` is shared and re-pointed by every turn
//! (see Task 14b's root cause), so a delegated turn and a local turn must
//! never run on the same provider at once. Each `stream_acp_completion`
//! call holds a [`Lease`] for its provider; dropping it releases it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

struct Holder {
    session_id: String,
    delegated: bool,
}

fn leases() -> &'static Mutex<HashMap<String, Vec<Holder>>> {
    static LEASES: OnceLock<Mutex<HashMap<String, Vec<Holder>>>> = OnceLock::new();
    LEASES.get_or_init(Default::default)
}

/// Held for one `stream_acp_completion` call; dropping it (normal return,
/// error, cancel, unwind) releases it.
#[derive(Debug)]
pub struct Lease {
    provider: String,
    session_id: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut map = leases().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(holders) = map.get_mut(&self.provider) {
            if let Some(i) = holders.iter().position(|h| h.session_id == self.session_id) {
                holders.remove(i);
            }
            if holders.is_empty() {
                map.remove(&self.provider);
            }
        }
    }
}

/// The error a local turn gets when a delegated turn holds the provider.
pub fn local_refused(provider: &str) -> String {
    format!("{provider} is running a task delegated by a paired agent. Try again when it finishes, or stop the task in the status bar.")
}

/// The error a delegated turn gets when another turn holds the provider.
pub fn delegated_refused(provider: &str) -> String {
    format!("{provider} is busy with another conversation, so the delegated task cannot use it. Try again when it finishes.")
}

/// Take `provider` for `session_id`. Refused (with the R38 copy) when another
/// session holds it and either side is a delegated turn.
pub fn acquire(provider: &str, session_id: &str, delegated: bool) -> Result<Lease, String> {
    let mut map = leases().lock().unwrap_or_else(|e| e.into_inner());
    let holders = map.entry(provider.to_string()).or_default();
    if let Some(other) = holders
        .iter()
        .find(|h| h.session_id != session_id && (h.delegated || delegated))
    {
        return Err(if other.delegated {
            local_refused(provider)
        } else {
            delegated_refused(provider)
        });
    }
    holders.push(Holder {
        session_id: session_id.to_string(),
        delegated,
    });
    Ok(Lease {
        provider: provider.to_string(),
        session_id: session_id.to_string(),
    })
}

/// How many turns hold `provider` now (`task_start`'s pre-check).
pub fn holder_count(provider: &str) -> usize {
    leases()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(provider)
        .map_or(0, Vec::len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_turns_share_a_provider_as_before() {
        let a = acquire("T1", "s-a", false).unwrap();
        let b = acquire("T1", "s-b", false).unwrap();
        assert_eq!(holder_count("T1"), 2);
        drop((a, b));
        assert_eq!(holder_count("T1"), 0);
    }

    #[test]
    fn a_local_turn_is_refused_while_a_delegated_turn_holds_the_provider() {
        let _d = acquire("T2", "s-del", true).unwrap();
        assert_eq!(
            acquire("T2", "s-own", false).unwrap_err(),
            "T2 is running a task delegated by a paired agent. Try again when it finishes, or stop the task in the status bar."
        );
    }

    #[test]
    fn a_delegated_turn_is_refused_while_another_turn_holds_the_provider() {
        let _l = acquire("T3", "s-own", false).unwrap();
        assert_eq!(
            acquire("T3", "s-del", true).unwrap_err(),
            "T3 is busy with another conversation, so the delegated task cannot use it. Try again when it finishes."
        );
    }

    #[test]
    fn the_same_session_may_hold_it_again_and_release_frees_it() {
        let first = acquire("T4", "s-del", true).unwrap();
        let again = acquire("T4", "s-del", true).unwrap();
        drop(first);
        assert!(acquire("T4", "s-own", false).is_err());
        drop(again);
        assert!(acquire("T4", "s-own", false).is_ok());
    }

    #[test]
    fn providers_do_not_block_each_other() {
        let _d = acquire("T5", "s-del", true).unwrap();
        assert!(acquire("T6", "s-own", false).is_ok());
    }

    #[test]
    fn a_panic_releases_the_lease() {
        let r = std::panic::catch_unwind(|| {
            let _d = acquire("T7", "s-del", true).unwrap();
            panic!("turn blew up");
        });
        assert!(r.is_err());
        assert_eq!(holder_count("T7"), 0);
        assert!(acquire("T7", "s-own", false).is_ok());
    }
}
