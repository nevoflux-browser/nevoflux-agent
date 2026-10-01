-- Per-call LLM and Jev usage (design v1.6 §9 M4, minimal): the raw rows the
-- cost breakdown that fixes G2's X is computed from.
CREATE TABLE IF NOT EXISTS llm_calls (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    session_id  TEXT    NOT NULL,
    role        TEXT    NOT NULL,
    model       TEXT    NOT NULL DEFAULT '',
    input       INTEGER NOT NULL DEFAULT 0,
    output      INTEGER NOT NULL DEFAULT 0,
    cache_read  INTEGER,
    cache_write INTEGER,
    estimated   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_llm_calls_ts ON llm_calls(ts);
