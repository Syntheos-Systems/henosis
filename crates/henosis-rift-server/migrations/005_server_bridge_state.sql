-- Scope bridge runtime state to the server whose agents it controls.
CREATE TABLE IF NOT EXISTS bridge_server_state (
    server_id UUID PRIMARY KEY REFERENCES servers(id) ON DELETE CASCADE,
    paused BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Preserve the legacy instance-wide pause value for every server that exists at upgrade time.
INSERT INTO bridge_server_state (server_id, paused)
SELECT
    servers.id,
    COALESCE(
        (SELECT value = 'true' FROM bridge_state WHERE key = 'paused'),
        FALSE
    )
FROM servers
ON CONFLICT (server_id) DO NOTHING;
