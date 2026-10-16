//! The pairing's tier as a ceiling on the ACP path (M3 §4.1).
//!
//! An ACP provider's tool calls are approved by `McpToolBridge::request_permission`,
//! which knows only the tier set when the turn began and its own always-allow
//! cache — shared by every session on that provider. For a delegated turn both
//! are wrong: the tier must be read per call, and a person's "always allow"
//! from their own chat must not lift a paired agent's ceiling. So the bridge's
//! tightener sends everything above the ceiling to the permission handler
//! (`tighten`), and the handler refuses it there without asking
//! (`wasm::mcp_tool_executor::run_permission_handler`).

use std::sync::Arc;

use nevoflux_llm::providers::acp::mcp_bridge::Tightener;
use nevoflux_storage::Database;

use crate::remote::pairing::PairingStore;

/// Whether a tool is within the delegated turn's tier, read at the call.
pub type Ceiling = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The ceiling for `session_id` if a paired agent started it, against `store`.
pub fn ceiling_with(
    db: Arc<Database>,
    session_id: &str,
    store: Option<Arc<PairingStore>>,
) -> Option<Ceiling> {
    let pairing = super::origin::delegated_pairing(&db, session_id)?;
    Some(Arc::new(move |tool: &str| {
        nevoflux_protocol::tier_auto_approves(
            tool,
            super::origin::pairing_tier(store.clone(), &pairing),
        )
    }))
}

/// [`ceiling_with`] against the daemon's own pairing store.
pub fn ceiling_for(db: Arc<Database>, session_id: &str) -> Option<Ceiling> {
    ceiling_with(db, session_id, super::origin::production_store())
}

/// Why a delegated call above its pairing's tier is refused (contract R25.1).
pub fn refusal(tool: &str) -> String {
    format!(
        "{} needs the {} tier, which this paired agent does not have. The person can raise the agent's tier in NevoFlux.",
        tool,
        nevoflux_protocol::minimum_tier(tool).as_setting()
    )
}

/// A bridge tightener that sends every call above `ceiling` to the handler
/// (past the bridge's turn-start tier and its always-allow cache) and
/// otherwise defers to `inner` (Jev), if any.
pub fn tighten(inner: Option<Tightener>, ceiling: Ceiling) -> Tightener {
    Arc::new(move |tool: String, args: String, would_auto: bool| {
        let (inner, ceiling) = (inner.clone(), ceiling.clone());
        Box::pin(async move {
            if !ceiling(&tool) {
                return true;
            }
            match inner {
                Some(jev) => jev(tool, args, would_auto).await,
                None => false,
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::agent_status::testing::agent;
    use nevoflux_protocol::ExecutionTier;

    #[test]
    fn the_ceiling_follows_the_pairing_live() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        super::super::origin::create_delegated(&db, "s1", "chan-1").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(PairingStore::new(dir.path().join("agent-pairings.json")));
        store.add(agent("chan-1", 1)).unwrap();
        let within = ceiling_with(db, "s1", Some(store.clone())).expect("a delegated session");
        assert!(within("browser_navigate"));
        assert!(!within("run_command"));
        store.set_tier("chan-1", ExecutionTier::FullAuto).unwrap();
        assert!(
            within("run_command"),
            "raised on the desktop, read at the next call"
        );
        store.set_tier("chan-1", ExecutionTier::ReadOnly).unwrap();
        assert!(!within("run_command"), "and lowered again");
    }

    #[test]
    fn an_ordinary_session_has_no_ceiling() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        assert!(ceiling_with(db, "s-own", None).is_none());
    }

    #[tokio::test]
    async fn above_the_ceiling_always_reaches_the_handler() {
        let within: Ceiling = Arc::new(|tool: &str| tool == "browser_navigate");
        let tightened = tighten(None, within.clone());
        assert!(
            tightened("run_command".into(), "{}".into(), true).await,
            "even when the bridge's own tier or cache would have run it"
        );
        assert!(!tightened("browser_navigate".into(), "{}".into(), false).await);
        let jev: Tightener = Arc::new(|_, _, _| Box::pin(async { true }));
        assert!(
            tighten(Some(jev), within)("browser_navigate".into(), "{}".into(), true).await,
            "Jev still flags inside the ceiling"
        );
    }
}
