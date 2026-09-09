-- Append-only session event log: the session's source of truth (design spec §3).
--
-- Deliberately has NO foreign key to sessions(id). Subagent and loop runs use
-- synthetic session ids that never get a sessions row, and a log that silently
-- refuses those writes would break invariant I1 exactly where debugging matters
-- most.
CREATE TABLE IF NOT EXISTS session_events (
    session_id TEXT    NOT NULL,
    seq        INTEGER NOT NULL,
    type       TEXT    NOT NULL,
    payload    TEXT    NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (session_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_session_events_type
    ON session_events(session_id, type, seq);
