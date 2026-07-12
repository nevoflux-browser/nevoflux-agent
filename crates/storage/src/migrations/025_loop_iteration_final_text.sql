-- Migration 025: per-iteration result summary + token spend (loop "logs").
-- Nullable, additive — existing rows keep NULL. final_text is capped by the
-- writer (daemon loops::events cap), NOT enforced here.
ALTER TABLE loop_iterations ADD COLUMN final_text  TEXT;
ALTER TABLE loop_iterations ADD COLUMN tokens_used INTEGER;
