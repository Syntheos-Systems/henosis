-- Persistent agent ownership and immutable room execution configuration.
CREATE TABLE agent_ownership (
    agent_user_id UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT agent_ownership_distinct_users CHECK (agent_user_id <> owner_user_id)
);

CREATE INDEX agent_ownership_owner_idx ON agent_ownership (owner_user_id);

CREATE TABLE room_agent_config_revisions (
    server_id UUID NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    revision BIGINT NOT NULL CHECK (revision > 0),
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (server_id, revision)
);

CREATE TABLE room_agent_seats (
    server_id UUID NOT NULL,
    revision BIGINT NOT NULL,
    seat_id UUID NOT NULL,
    agent_user_id UUID NOT NULL REFERENCES users(id),
    harness_id VARCHAR(64) NOT NULL,
    model_id VARCHAR(128) NOT NULL,
    settings JSONB NOT NULL DEFAULT '{}'::jsonb,
    credential_binding_id UUID,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    position INTEGER NOT NULL CHECK (position >= 0),
    PRIMARY KEY (server_id, revision, seat_id),
    UNIQUE (server_id, revision, agent_user_id),
    FOREIGN KEY (server_id, revision)
        REFERENCES room_agent_config_revisions(server_id, revision)
        ON DELETE CASCADE
);

ALTER TABLE bridge_server_state
    ADD COLUMN desired_revision BIGINT,
    ADD COLUMN active_revision BIGINT,
    ADD COLUMN last_good_revision BIGINT,
    ADD COLUMN apply_state VARCHAR(16) NOT NULL DEFAULT 'idle',
    ADD COLUMN apply_error_code VARCHAR(64),
    ADD COLUMN apply_error_message VARCHAR(512),
    ADD COLUMN apply_updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ADD CONSTRAINT bridge_server_apply_state_check
        CHECK (apply_state IN ('idle', 'pending', 'active', 'failed'));
