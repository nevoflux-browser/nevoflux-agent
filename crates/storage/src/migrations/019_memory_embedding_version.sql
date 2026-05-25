-- M1 #009: Track embedding version on memory_chunks so the daemon can
-- reindex legacy no-prefix vectors after the EmbedKind-aware embedding
-- API was introduced in M1 #001/#002.
--
-- Version semantics:
--   0 = legacy, computed before the e5 "passage: " prefix existed.
--       Retrieval against query-side "query: " vectors is degraded.
--   1 = current, computed via EmbedKind::Passage (prefix-aware).
--
-- Existing rows default to 0 so the background reindex task can pick
-- them up. New inserts written by the prefix-aware code path should
-- write version = 1 explicitly (handled in repository code).
ALTER TABLE memory_chunks ADD COLUMN embedding_version INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_memory_chunks_embedding_version
    ON memory_chunks(embedding_version);
