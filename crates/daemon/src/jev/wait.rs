//! The time the agent loop blocks on Jev, for the G2 latency gate (spec
//! §6: the P50 added per step, over all steps). Most Jev requests overlap
//! with tool execution, so a request's duration is not added latency; only
//! the time the loop spent blocked on one is.

use std::time::Duration;

use nevoflux_protocol::session_event::SessionEventPayload;

/// Shorter waits are not Jev: deterministic paths, early returns.
pub const MIN_MS: u64 = 1;

/// Log `elapsed` as a Jev wait at `site`, unless it is under [`MIN_MS`].
pub fn log(
    writer: Option<&crate::session_events::SessionEventWriter>,
    site: &str,
    elapsed: Duration,
) {
    let ms = elapsed.as_millis() as u64;
    if ms < MIN_MS {
        return;
    }
    if let Some(w) = writer {
        w.append(SessionEventPayload::JevWait {
            site: site.to_string(),
            ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_protocol::session_event::SessionEventPayload;
    use std::time::Duration;

    fn waits(db: &nevoflux_storage::Database) -> Vec<SessionEventPayload> {
        nevoflux_storage::repositories::SessionEventRepository::new(db)
            .list("s1")
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .filter(|p| matches!(p, SessionEventPayload::JevWait { .. }))
            .collect()
    }

    #[test]
    fn a_wait_is_logged_with_its_site() {
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let w = crate::session_events::SessionEventWriter::new(db.clone(), "s1".into());
        log(Some(&w), "signals", Duration::from_millis(312));
        assert_eq!(
            waits(&db),
            vec![SessionEventPayload::JevWait {
                site: "signals".into(),
                ms: 312
            }]
        );
    }

    #[test]
    fn a_sub_millisecond_wait_or_no_writer_logs_nothing() {
        let db = std::sync::Arc::new(nevoflux_storage::Database::open_in_memory().unwrap());
        let w = crate::session_events::SessionEventWriter::new(db.clone(), "s1".into());
        log(Some(&w), "render", Duration::from_micros(400));
        log(None, "render", Duration::from_millis(50));
        assert!(waits(&db).is_empty());
    }
}
