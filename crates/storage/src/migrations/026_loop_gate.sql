-- Migration 026: optional deterministic gate on a loop (W3).
ALTER TABLE loops ADD COLUMN gate_kind       TEXT NOT NULL DEFAULT 'none';
ALTER TABLE loops ADD COLUMN gate_spec       TEXT;   -- JSON, kind-specific
ALTER TABLE loops ADD COLUMN gate_last_value TEXT;   -- value-diff cursor
