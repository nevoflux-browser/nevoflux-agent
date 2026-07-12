-- Migration 027: optional per-loop verify (reuses /goal GoalCheck) + per-iteration verdict.
ALTER TABLE loops           ADD COLUMN verify_check   TEXT;    -- JSON GoalCheck {tool?, matches, negate?}
ALTER TABLE loop_iterations ADD COLUMN verify_passed  INTEGER; -- 0/1/NULL (NULL = no verify)
ALTER TABLE loop_iterations ADD COLUMN verify_reason  TEXT;
