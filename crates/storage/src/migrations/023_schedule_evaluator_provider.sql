-- 023_schedule_evaluator_provider.sql — add the evaluator provider column to
-- schedules, mirroring the goals table's (evaluator_provider, evaluator_model)
-- pair. Migration 021 stored only evaluator_model; goal-wrapped scheduled runs
-- (Task 3.2) resolve and persist BOTH the direct-API provider and model at
-- create time so the run-time evaluator call needs no re-resolution guesswork.
ALTER TABLE schedules ADD COLUMN evaluator_provider TEXT;
