-- Bridge metadata: links Rift entities to their Discord origins.
-- Designed so the core schema stays clean — bridge columns are all nullable
-- and only populated for bridged entities.

-- Users: track which users are Discord puppets
ALTER TABLE users ADD COLUMN IF NOT EXISTS bridge_source VARCHAR(20);       -- 'discord' | NULL (native)
ALTER TABLE users ADD COLUMN IF NOT EXISTS bridge_remote_id VARCHAR(64);    -- Discord snowflake
ALTER TABLE users ADD COLUMN IF NOT EXISTS bridge_is_puppet BOOLEAN NOT NULL DEFAULT FALSE;

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_bridge ON users(bridge_source, bridge_remote_id)
    WHERE bridge_source IS NOT NULL;

-- Servers: track which servers are Discord mirrors
ALTER TABLE servers ADD COLUMN IF NOT EXISTS bridge_source VARCHAR(20);
ALTER TABLE servers ADD COLUMN IF NOT EXISTS bridge_remote_id VARCHAR(64);

CREATE UNIQUE INDEX IF NOT EXISTS idx_servers_bridge ON servers(bridge_source, bridge_remote_id)
    WHERE bridge_source IS NOT NULL;

-- Channels: track which channels are Discord mirrors
ALTER TABLE channels ADD COLUMN IF NOT EXISTS bridge_source VARCHAR(20);
ALTER TABLE channels ADD COLUMN IF NOT EXISTS bridge_remote_id VARCHAR(64);

CREATE UNIQUE INDEX IF NOT EXISTS idx_channels_bridge ON channels(bridge_source, bridge_remote_id)
    WHERE bridge_source IS NOT NULL;

-- Messages: track which messages originated from Discord
ALTER TABLE messages ADD COLUMN IF NOT EXISTS bridge_source VARCHAR(20);
ALTER TABLE messages ADD COLUMN IF NOT EXISTS bridge_remote_id VARCHAR(64);

CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_bridge ON messages(bridge_source, bridge_remote_id)
    WHERE bridge_source IS NOT NULL;

-- DM messages: same treatment
ALTER TABLE dm_messages ADD COLUMN IF NOT EXISTS bridge_source VARCHAR(20);
ALTER TABLE dm_messages ADD COLUMN IF NOT EXISTS bridge_remote_id VARCHAR(64);

CREATE UNIQUE INDEX IF NOT EXISTS idx_dm_messages_bridge ON dm_messages(bridge_source, bridge_remote_id)
    WHERE bridge_source IS NOT NULL;

-- DM channels: track bridged DM channels
ALTER TABLE dm_channels ADD COLUMN IF NOT EXISTS bridge_source VARCHAR(20);
ALTER TABLE dm_channels ADD COLUMN IF NOT EXISTS bridge_remote_id VARCHAR(64);

CREATE UNIQUE INDEX IF NOT EXISTS idx_dm_channels_bridge ON dm_channels(bridge_source, bridge_remote_id)
    WHERE bridge_source IS NOT NULL;

-- Attachments: bridge tracking
ALTER TABLE attachments ADD COLUMN IF NOT EXISTS bridge_source VARCHAR(20);
ALTER TABLE attachments ADD COLUMN IF NOT EXISTS bridge_remote_id VARCHAR(64);

-- Bridge state table: tracks sync cursors, connection state, config
CREATE TABLE IF NOT EXISTS bridge_connections (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source VARCHAR(20) NOT NULL,               -- 'discord'
    remote_user_id VARCHAR(64) NOT NULL,        -- the Discord user ID we're bridging as
    local_user_id UUID NOT NULL REFERENCES users(id),
    token_encrypted TEXT NOT NULL,              -- Discord token (encrypted at rest)
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    last_connected_at TIMESTAMPTZ,
    last_sync_at TIMESTAMPTZ,
    sync_cursor JSONB,                         -- per-guild/channel sync state
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(source, remote_user_id)
);

-- Edit/delete audit log: Discord hides these, we don't
CREATE TABLE IF NOT EXISTS message_audit_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    message_id UUID NOT NULL,
    channel_id UUID NOT NULL,
    action VARCHAR(20) NOT NULL,               -- 'edit' | 'delete'
    old_content TEXT,
    new_content TEXT,
    actor_id UUID,
    bridge_source VARCHAR(20),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_message_audit ON message_audit_log(message_id);
CREATE INDEX IF NOT EXISTS idx_message_audit_channel ON message_audit_log(channel_id, created_at DESC);

-- Reactions table (new feature, needed for bridge)
CREATE TABLE IF NOT EXISTS reactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    message_id UUID NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    emoji VARCHAR(64) NOT NULL,                -- unicode emoji or custom :name:id
    bridge_source VARCHAR(20),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(message_id, user_id, emoji)
);

CREATE INDEX IF NOT EXISTS idx_reactions_message ON reactions(message_id);

-- Full-text search index on messages (bonus: enables search across ALL messages including bridged)
ALTER TABLE messages ADD COLUMN IF NOT EXISTS search_tsv TSVECTOR;

CREATE INDEX IF NOT EXISTS idx_messages_fts ON messages USING GIN(search_tsv);

-- Trigger to auto-populate search vector
CREATE OR REPLACE FUNCTION messages_search_trigger() RETURNS trigger AS $$
BEGIN
    NEW.search_tsv := to_tsvector('english', COALESCE(NEW.content, ''));
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS messages_search_update ON messages;
CREATE TRIGGER messages_search_update
    BEFORE INSERT OR UPDATE OF content ON messages
    FOR EACH ROW EXECUTE FUNCTION messages_search_trigger();

-- Backfill existing messages
UPDATE messages SET search_tsv = to_tsvector('english', COALESCE(content, ''))
    WHERE search_tsv IS NULL;
