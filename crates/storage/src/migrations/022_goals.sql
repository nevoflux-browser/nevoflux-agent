-- 022_goals.sql — session-scoped goal conditions evaluated after each turn.
CREATE TABLE goals (
    id                  TEXT    PRIMARY KEY,
    session_id          TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    condition           TEXT    NOT NULL,
    evaluator_provider  TEXT,
    evaluator_model     TEXT,
    max_turns           INTEGER NOT NULL DEFAULT 20,
    turns_used          INTEGER NOT NULL DEFAULT 0,
    status              TEXT    NOT NULL DEFAULT 'active',
    last_reason         TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    achieved_at         INTEGER,
    CHECK (LENGTH(condition) <= 4000),
    CHECK (status IN ('active', 'achieved', 'expired', 'cleared'))
);
CREATE INDEX goals_session_idx ON goals(session_id);
CREATE UNIQUE INDEX goals_session_active_uidx ON goals(session_id) WHERE status = 'active';
