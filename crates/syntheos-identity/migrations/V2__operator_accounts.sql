-- V2: operator_account table for human admin authentication.
-- Stores argon2id PHC hashes; passwords are NEVER stored in plaintext.
-- email is the PRIMARY KEY (unique login handle, always stored lowercase).
-- disabled=1 blocks login without removing the row (audit-friendly).
-- Append-only migration: never edit; add V3 for future schema changes.
CREATE TABLE IF NOT EXISTS operator_account (
    email         TEXT PRIMARY KEY,
    password_hash TEXT NOT NULL,
    principal_id  TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    disabled      INTEGER NOT NULL DEFAULT 0
);
