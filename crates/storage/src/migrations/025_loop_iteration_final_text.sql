-- Migration 025: per-iteration result summary + token spend (loop "logs").
-- Nullable, additive — existing rows keep NULL. final_text is capped at 4096
-- chars by the writer (LoopRepository::finish_iteration, via the shared
-- repositories::truncate_final_text helper), NOT enforced here.
ALTER TABLE loop_iterations ADD COLUMN final_text  TEXT;
ALTER TABLE loop_iterations ADD COLUMN tokens_used INTEGER;
