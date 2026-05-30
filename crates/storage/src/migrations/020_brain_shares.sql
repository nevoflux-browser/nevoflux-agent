-- Brain Share: local record of `.nbrain` shares created by this user.
-- Standalone (brain shares are not artifacts). Credentials encrypted at rest.

CREATE TABLE IF NOT EXISTS brain_shares (
    share_id              TEXT NOT NULL PRIMARY KEY,
    share_url             TEXT NOT NULL,
    encrypted_owner_token TEXT NOT NULL,
    encrypted_key         TEXT NOT NULL,
    title                 TEXT NOT NULL DEFAULT '',
    size_bytes            INTEGER NOT NULL DEFAULT 0,
    expires_at            INTEGER NOT NULL,
    created_at            INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_brain_shares_expires_at ON brain_shares(expires_at);
CREATE INDEX IF NOT EXISTS idx_brain_shares_created_at ON brain_shares(created_at);

CREATE TABLE IF NOT EXISTS brain_share_history (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    share_id   TEXT NOT NULL,
    reason     TEXT NOT NULL,
    expired_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);
