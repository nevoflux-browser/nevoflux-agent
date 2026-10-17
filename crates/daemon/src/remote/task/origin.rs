//! Which sessions a paired agent started, and the tier they run at (M3 §4.1).
//!
//! A delegated session is marked in its own metadata,
//! `origin = {"kind": "muse", "pairing": "<control_channel_id>"}` (contract
//! §3), written when the daemon creates it for `nevoflux_task_start`. Every
//! tier decision for such a session reads the pairing's tier from
//! `agent-pairings.json`, on every call: the session's own key and the global
//! default cannot raise it, and lowering the pairing's tier on the desktop
//! takes effect on the next tool call. A pairing that is gone reads as
//! read-only.

use std::collections::HashMap;
use std::sync::Arc;

use nevoflux_protocol::ExecutionTier;
use nevoflux_storage::models::{CreateSessionParams, SessionMode};
use nevoflux_storage::{Database, SessionRepository};
use serde_json::{json, Value};

use crate::remote::agent_backend::{StoredTier, TierSource};
use crate::remote::pairing::PairingStore;

/// The session-metadata key that says who started a session.
pub const ORIGIN_KEY: &str = "origin";
/// `origin.kind` of a session a paired agent started.
pub const MUSE_KIND: &str = "muse";

/// The `origin` object of a session `pairing` started.
pub fn muse_origin(pairing: &str) -> Value {
    json!({ "kind": MUSE_KIND, "pairing": pairing })
}

/// What `session.list` shows as `origin` (contract §3): exactly
/// `{"kind":"muse","pairing":…}`, or `null` for anything else, malformed
/// included. A session is delegated only if this is not `null`.
pub fn origin_value(metadata: Option<&HashMap<String, Value>>) -> Value {
    let Some(origin) = metadata.and_then(|m| m.get(ORIGIN_KEY)) else {
        return Value::Null;
    };
    let pairing = origin
        .get("pairing")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    match (origin.get("kind").and_then(Value::as_str), pairing) {
        (Some(MUSE_KIND), Some(pairing)) => muse_origin(pairing),
        _ => Value::Null,
    }
}

/// Who started a session, as far as the store can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delegation {
    /// Not a paired agent's session (or no such session yet).
    Local,
    /// Started by this pairing.
    Delegated(String),
    /// The session could not be read.
    Unknown,
}

/// Who started `session_id`.
pub fn delegation(db: &Database, session_id: &str) -> Delegation {
    if session_id.is_empty() {
        return Delegation::Local;
    }
    let session = match SessionRepository::new(db).get(session_id) {
        Ok(Some(session)) => session,
        Ok(None) => return Delegation::Local,
        Err(e) => {
            tracing::warn!(target: "remote", "session origin unreadable, treating it as delegated: {e}");
            return Delegation::Unknown;
        }
    };
    match origin_value(session.metadata.as_ref())
        .get("pairing")
        .and_then(Value::as_str)
    {
        Some(pairing) => Delegation::Delegated(pairing.to_string()),
        None => Delegation::Local,
    }
}

/// The pairing that started `session_id`, if a paired agent did.
///
/// Fails closed: a session the store cannot read is treated as delegated, to
/// the empty pairing id — which no pairing has, so it runs read-only
/// ([`pairing_tier`]) and is never any pairing's own session.
pub fn delegated_pairing(db: &Database, session_id: &str) -> Option<String> {
    match delegation(db, session_id) {
        Delegation::Delegated(pairing) => Some(pairing),
        Delegation::Unknown => Some(String::new()),
        Delegation::Local => None,
    }
}

/// Create the session a new delegated task runs in: Agent mode, marked as
/// `pairing`'s. Written before the turn is injected, so the very first tool
/// call is already judged as delegated.
pub fn create_delegated(db: &Database, session_id: &str, pairing: &str) -> Result<(), String> {
    let mut metadata = HashMap::new();
    metadata.insert(ORIGIN_KEY.to_string(), muse_origin(pairing));
    SessionRepository::new(db)
        .create(
            CreateSessionParams::new()
                .with_id(session_id)
                .with_mode(SessionMode::Agent)
                .with_metadata(metadata),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The tier `pairing` runs at now: read from the store on every call
/// ([`StoredTier`]). No store, no such pairing, or an unreadable file all
/// read as read-only — a delegated turn is narrowed, never unlocked.
pub fn pairing_tier(store: Option<Arc<PairingStore>>, pairing: &str) -> ExecutionTier {
    match store {
        Some(store) => StoredTier::new(store, pairing).tier(),
        None => ExecutionTier::ReadOnly,
    }
}

/// The daemon's agent-pairing store, once startup has published it.
pub fn production_store() -> Option<Arc<PairingStore>> {
    crate::remote::start::control_deps().map(|deps| deps.agent_pairings.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::agent_status::testing::agent;

    fn db() -> Arc<Database> {
        Arc::new(Database::open_in_memory().unwrap())
    }

    fn store_with(
        pairings: &[crate::remote::pairing::Pairing],
    ) -> (Arc<PairingStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(PairingStore::new(dir.path().join("agent-pairings.json")));
        for p in pairings {
            store.add(p.clone()).unwrap();
        }
        (store, dir)
    }

    #[test]
    fn a_session_a_pairing_started_names_that_pairing() {
        let db = db();
        create_delegated(&db, "s1", "chan-1").unwrap();
        assert_eq!(delegated_pairing(&db, "s1").as_deref(), Some("chan-1"));
        let session = SessionRepository::new(&db).get("s1").unwrap().unwrap();
        assert_eq!(session.mode.as_str(), "agent");
        assert_eq!(
            session.metadata.unwrap()["origin"],
            json!({"kind": "muse", "pairing": "chan-1"})
        );
    }

    #[test]
    fn an_ordinary_session_is_not_delegated() {
        let db = db();
        SessionRepository::new(&db)
            .create(CreateSessionParams::new().with_id("s2"))
            .unwrap();
        assert_eq!(delegated_pairing(&db, "s2"), None);
        assert_eq!(delegated_pairing(&db, "no-such-session"), None);
        assert_eq!(delegated_pairing(&db, ""), None);
    }

    #[test]
    fn an_unreadable_session_fails_closed_as_delegated_to_no_pairing() {
        let db = db();
        create_delegated(&db, "s1", "chan-1").unwrap();
        db.with_connection(|c| {
            c.execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE sessions;")
                .map_err(Into::into)
        })
        .unwrap();
        assert_eq!(delegation(&db, "s1"), Delegation::Unknown);
        let pairing = delegated_pairing(&db, "s1").expect("treated as delegated");
        let (store, _dir) = store_with(&[agent("chan-1", 1)]);
        store.set_tier("chan-1", ExecutionTier::FullAuto).unwrap();
        assert_eq!(
            pairing_tier(Some(store), &pairing),
            ExecutionTier::ReadOnly,
            "no pairing's tier: read-only"
        );
        assert_eq!(delegation(&db, ""), Delegation::Local);
    }

    #[test]
    fn only_a_well_formed_muse_origin_counts() {
        for (stored, want) in [
            (
                json!({"kind": "muse", "pairing": "c"}),
                json!({"kind": "muse", "pairing": "c"}),
            ),
            (
                json!({"kind": "muse", "pairing": "c", "extra": 1}),
                json!({"kind": "muse", "pairing": "c"}),
            ),
            (json!({"kind": "phone", "pairing": "c"}), Value::Null),
            (json!({"kind": "muse"}), Value::Null),
            (json!({"kind": "muse", "pairing": ""}), Value::Null),
            (json!("muse"), Value::Null),
        ] {
            let mut m = HashMap::new();
            m.insert(ORIGIN_KEY.to_string(), stored.clone());
            assert_eq!(origin_value(Some(&m)), want, "{stored}");
        }
        assert_eq!(origin_value(None), Value::Null);
    }

    #[test]
    fn the_pairing_tier_is_read_from_the_store_every_time() {
        let (store, _dir) = store_with(&[agent("chan-1", 1)]);
        store.set_tier("chan-1", ExecutionTier::FullAuto).unwrap();
        assert_eq!(
            pairing_tier(Some(store.clone()), "chan-1"),
            ExecutionTier::FullAuto
        );
        store.set_tier("chan-1", ExecutionTier::ReadOnly).unwrap();
        assert_eq!(
            pairing_tier(Some(store), "chan-1"),
            ExecutionTier::ReadOnly,
            "lowered on the desktop, read at the next call"
        );
    }

    #[test]
    fn a_pairing_that_is_gone_reads_as_read_only() {
        let (store, _dir) = store_with(&[]);
        assert_eq!(pairing_tier(Some(store), "chan-1"), ExecutionTier::ReadOnly);
        assert_eq!(pairing_tier(None, "chan-1"), ExecutionTier::ReadOnly);
    }
}
