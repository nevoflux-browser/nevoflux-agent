//! Repository for the append-only session event log (design spec §3).

use nevoflux_protocol::session_event::{SessionEvent, SessionEventPayload};
use rusqlite::params;

use crate::connection::Database;
use crate::error::Result;

/// Append-only access to `session_events`.
pub struct SessionEventRepository<'a> {
    db: &'a Database,
}

impl<'a> SessionEventRepository<'a> {
    /// Create a new repository over the given database handle.
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Append one event and return the sequence number it was given.
    ///
    /// The sequence is allocated inside the same transaction as the insert, so
    /// two concurrent appends to one session cannot collide on `seq`.
    pub fn append(&self, session_id: &str, payload: &SessionEventPayload) -> Result<i64> {
        let type_str = payload.type_str();
        let body = serde_json::to_string(payload)?;
        let ts = now_millis();

        self.db.with_connection_mut(|conn| {
            let tx = conn.transaction()?;
            let seq: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM session_events WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO session_events (session_id, seq, type, payload, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, seq, type_str, body, ts],
            )?;
            tx.commit()?;
            Ok(seq)
        })
    }

    /// Every event of a session, in sequence order.
    pub fn list(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        self.query(
            "SELECT seq, ts, payload FROM session_events
             WHERE session_id = ?1 ORDER BY seq ASC",
            params![session_id],
        )
    }

    /// Events up to and including `until_seq`, in sequence order.
    ///
    /// Used by `session replay --until` to reconstruct a prefix of a run.
    pub fn list_until(&self, session_id: &str, until_seq: i64) -> Result<Vec<SessionEvent>> {
        self.query(
            "SELECT seq, ts, payload FROM session_events
             WHERE session_id = ?1 AND seq <= ?2 ORDER BY seq ASC",
            params![session_id, until_seq],
        )
    }

    /// The most recent event of a given wire type, if any.
    ///
    /// The writer uses this to decide whether the system prompt actually changed
    /// before emitting another `system/message`.
    pub fn last_of_type(&self, session_id: &str, type_str: &str) -> Result<Option<SessionEvent>> {
        let mut rows = self.query(
            "SELECT seq, ts, payload FROM session_events
             WHERE session_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            params![session_id, type_str],
        )?;
        Ok(rows.pop())
    }

    fn query<P: rusqlite::Params>(&self, sql: &str, p: P) -> Result<Vec<SessionEvent>> {
        self.db.with_connection(|conn| {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(p, |row| {
                let seq: i64 = row.get(0)?;
                let ts: i64 = row.get(1)?;
                let body: String = row.get(2)?;
                Ok((seq, ts, body))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (seq, ts, body) = r?;
                let payload: SessionEventPayload = serde_json::from_str(&body)?;
                out.push(SessionEvent { seq, ts, payload });
            }
            Ok(out)
        })
    }
}

/// Current unix time in milliseconds, saturating to 0 before the epoch.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_protocol::session_event::{SessionEventPayload, ToolOrigin};

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn append_assigns_monotonic_seq_per_session() {
        let d = db();
        let r = SessionEventRepository::new(&d);
        assert_eq!(
            r.append("s1", &SessionEventPayload::TurnStart { turn: 1 })
                .unwrap(),
            1
        );
        assert_eq!(
            r.append("s1", &SessionEventPayload::TurnEnd { turn: 1 })
                .unwrap(),
            2
        );
        // A different session starts its own count at 1.
        assert_eq!(
            r.append("s2", &SessionEventPayload::TurnStart { turn: 1 })
                .unwrap(),
            1
        );
        assert_eq!(
            r.append("s1", &SessionEventPayload::TurnStart { turn: 2 })
                .unwrap(),
            3
        );
    }

    #[test]
    fn list_returns_events_in_seq_order_with_payloads_intact() {
        let d = db();
        let r = SessionEventRepository::new(&d);
        r.append("s1", &SessionEventPayload::TurnStart { turn: 1 })
            .unwrap();
        r.append(
            "s1",
            &SessionEventPayload::ToolCall {
                id: "t1".into(),
                name: "read_file".into(),
                args: serde_json::json!({ "path": "a.txt" }),
                origin: ToolOrigin::model(),
                tab_url: None,
            },
        )
        .unwrap();

        let evs = r.list("s1").unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].seq, 1);
        assert_eq!(evs[1].seq, 2);
        match &evs[1].payload {
            SessionEventPayload::ToolCall { name, args, .. } => {
                assert_eq!(name, "read_file");
                assert_eq!(args["path"], "a.txt");
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn list_until_stops_at_the_requested_seq_inclusive() {
        let d = db();
        let r = SessionEventRepository::new(&d);
        for i in 1..=5 {
            r.append("s1", &SessionEventPayload::TurnStart { turn: i })
                .unwrap();
        }
        let evs = r.list_until("s1", 3).unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs.last().unwrap().seq, 3);
    }

    #[test]
    fn last_of_type_finds_the_most_recent_match_only() {
        let d = db();
        let r = SessionEventRepository::new(&d);
        r.append(
            "s1",
            &SessionEventPayload::SystemMessage {
                content: "first".into(),
                sections: vec![],
                origin: "kernel".into(),
            },
        )
        .unwrap();
        r.append("s1", &SessionEventPayload::TurnStart { turn: 1 })
            .unwrap();
        r.append(
            "s1",
            &SessionEventPayload::SystemMessage {
                content: "second".into(),
                sections: vec![],
                origin: "kernel".into(),
            },
        )
        .unwrap();

        let found = r.last_of_type("s1", "system/message").unwrap().unwrap();
        match found.payload {
            SessionEventPayload::SystemMessage { content, .. } => assert_eq!(content, "second"),
            other => panic!("wrong payload: {other:?}"),
        }
        assert!(r.last_of_type("s1", "tool/call").unwrap().is_none());
    }

    #[test]
    fn events_survive_for_a_session_id_that_has_no_sessions_row() {
        // Subagent and loop runs use synthetic session ids with no sessions row;
        // the log must not silently refuse those writes.
        let d = db();
        let r = SessionEventRepository::new(&d);
        r.append(
            "synthetic_subagent_42",
            &SessionEventPayload::TurnStart { turn: 1 },
        )
        .unwrap();
        assert_eq!(r.list("synthetic_subagent_42").unwrap().len(), 1);
    }
}
