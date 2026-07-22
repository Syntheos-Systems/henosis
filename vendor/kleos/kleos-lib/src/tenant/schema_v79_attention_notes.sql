-- Tenant schema v79: attention_notes table for persistent sticky reminders.
-- No decay, no expiry. Agents delete explicitly when done.
CREATE TABLE IF NOT EXISTS attention_notes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id     INTEGER NOT NULL,
    content     TEXT NOT NULL,
    priority    INTEGER NOT NULL DEFAULT 5 CHECK (priority BETWEEN 1 AND 10),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_attention_notes_user_priority
    ON attention_notes(user_id, priority DESC, created_at ASC);
