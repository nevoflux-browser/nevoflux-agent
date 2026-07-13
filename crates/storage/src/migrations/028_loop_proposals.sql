-- Migration 028: loop self-improvement proposals (W4).
CREATE TABLE loop_proposals (
    id                    TEXT PRIMARY KEY,
    loop_id               TEXT NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
    created_at            INTEGER NOT NULL,
    rationale             TEXT NOT NULL,
    proposed_prompt_text  TEXT,
    proposed_gate_spec    TEXT,
    status                TEXT NOT NULL DEFAULT 'pending'
);
CREATE INDEX loop_proposals_loop_idx ON loop_proposals(loop_id, status);
