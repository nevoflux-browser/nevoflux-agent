-- Goal programmatic check (spec §4.3 route A): an optional machine-verifiable
-- assertion over recent tool results, serialized as JSON. NULL = no check
-- (goal is judged by an evaluator model as before).
ALTER TABLE goals ADD COLUMN check_json TEXT;
