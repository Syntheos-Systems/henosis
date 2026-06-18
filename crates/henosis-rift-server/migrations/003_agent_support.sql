-- Migration 003: Agent support columns for users and messages tables.
-- Adds fields required by the bridge to track AI agent users and typed messages.

ALTER TABLE users ADD COLUMN is_agent BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE users ADD COLUMN executor_type VARCHAR(64);
ALTER TABLE users ADD COLUMN agent_roster_id VARCHAR(128);

ALTER TABLE messages ADD COLUMN message_type VARCHAR(16) NOT NULL DEFAULT 'user';

-- Partial index for fast lookup of agent users (expected to be a small subset).
CREATE INDEX idx_users_is_agent ON users (is_agent) WHERE is_agent = TRUE;

-- Partial index for non-user messages (agent/stimulus/system) to avoid scanning the bulk of human messages.
CREATE INDEX idx_messages_message_type ON messages (message_type) WHERE message_type != 'user';

-- Key-value store for bridge runtime state (e.g., paused flag).
CREATE TABLE IF NOT EXISTS bridge_state (
    key VARCHAR(64) PRIMARY KEY,
    value VARCHAR(256) NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed the paused flag; do nothing if it already exists.
INSERT INTO bridge_state (key, value) VALUES ('paused', 'false') ON CONFLICT DO NOTHING;
