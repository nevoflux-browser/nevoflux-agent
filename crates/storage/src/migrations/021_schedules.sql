-- 021_schedules.sql — routines-style scheduled jobs (independent from loops).
-- Schedules survive daemon restarts: next_fire_at is persisted and re-armed at boot.

CREATE TABLE schedules (
    id                    TEXT    PRIMARY KEY,
    creator_session_id    TEXT    REFERENCES sessions(id) ON DELETE SET NULL,
    name                  TEXT    NOT NULL,
    cron_expr             TEXT,
    at_ts                 INTEGER,
    prompt_text           TEXT,
    wrapped_skill         TEXT,
    mode                  TEXT    NOT NULL DEFAULT 'chat',
    browser_policy        TEXT    NOT NULL DEFAULT 'none',
    on_unavailable        TEXT,
    headless_profile      TEXT,
    catch_up              INTEGER NOT NULL DEFAULT 0,
    goal_condition        TEXT,
    goal_max_turns        INTEGER,
    max_tokens_per_run    INTEGER,
    evaluator_model       TEXT,
    status                TEXT    NOT NULL DEFAULT 'active',
    next_fire_at          INTEGER,
    last_run_status       TEXT,
    last_run_at           INTEGER,
    consecutive_failures  INTEGER NOT NULL DEFAULT 0,
    run_count             INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    CHECK ((cron_expr IS NOT NULL) <> (at_ts IS NOT NULL)),
    CHECK ((prompt_text IS NOT NULL) <> (wrapped_skill IS NOT NULL)),
    CHECK (mode IN ('chat', 'browser', 'agent')),
    CHECK (browser_policy IN ('none', 'live', 'headless')),
    CHECK (on_unavailable IS NULL OR on_unavailable IN ('defer', 'skip')),
    CHECK (status IN ('active', 'paused', 'ran', 'cancelled')),
    CHECK (goal_condition IS NULL OR LENGTH(goal_condition) <= 4000)
);
CREATE INDEX schedules_status_idx ON schedules(status);
CREATE INDEX schedules_next_fire_idx ON schedules(next_fire_at);

CREATE TABLE schedule_runs (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    schedule_id    TEXT    NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER,
    status         TEXT    NOT NULL,
    fire_kind      TEXT    NOT NULL DEFAULT 'scheduled',
    error_message  TEXT,
    final_text     TEXT,
    tokens_used    INTEGER,
    goal_turns     INTEGER,
    CHECK (status IN ('running', 'ok', 'error', 'missed', 'skipped', 'deferred', 'cancelled')),
    CHECK (fire_kind IN ('scheduled', 'manual', 'catchup'))
);
CREATE INDEX schedule_runs_schedule_idx ON schedule_runs(schedule_id);
