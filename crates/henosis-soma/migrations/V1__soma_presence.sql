-- V1: the Soma agent-presence projection, extracted from kleos-lib onto the Henosis principal
-- model. The Kleos `soma_agents` row WAS the agent identity (i64 id + name + user_id owner);
-- here an agent IS a canonical principal (PrincipalKind::Agent in syntheos-identity) and this
-- table is Soma's presence PROJECTION of it: principal_id is the PRIMARY KEY, one row per
-- agent, exactly the worked example in 2026-06-09-principal-projection-convention.md section 2.
-- Registration never mints a principal (convention section 1); it verifies one exists.
--
-- Divergence from the convention's worked example: a `tenant` column is carried because Axon
-- event envelopes require a TenantId (the chiasm precedent). Whether projections shard by
-- tenant remains a Plutus (Phase 5+) concern; here it only scopes reads and name uniqueness.
-- Kleos `soma_groups` / `soma_agent_logs` are NOT in this slice.
-- Append-only migration: never edit; add a V2 file for any schema change.
CREATE TABLE soma_presence (
    -- The agent's own canonical PrincipalId (NOT an owner key). One presence row per agent.
    principal_id  TEXT NOT NULL PRIMARY KEY,
    -- TenantId the registration belongs to (Axon envelope scope).
    tenant        TEXT NOT NULL,
    -- Working label for the agent (e.g. 'claude-code'). Unique per tenant.
    name          TEXT NOT NULL,
    -- Coarse category (e.g. 'coding', 'cli').
    agent_type    TEXT NOT NULL,
    -- Optional human-readable description.
    description   TEXT,
    -- JSON array of capability strings.
    capabilities  TEXT NOT NULL DEFAULT '[]',
    -- PresenceStatus token: 'pending', 'online', 'offline', 'error'.
    status        TEXT NOT NULL DEFAULT 'pending',
    -- JSON object of agent-specific configuration.
    config        TEXT NOT NULL DEFAULT '{}',
    -- Last heartbeat timestamp (RFC3339 UTC), or NULL if never beaten.
    heartbeat_at  TEXT,
    -- Latest quality score from Thymus evaluation, if any.
    quality_score REAL,
    -- JSON array of drift-flag strings.
    drift_flags   TEXT NOT NULL DEFAULT '[]',
    -- Creation timestamp (RFC3339 UTC).
    created_at    TEXT NOT NULL,
    -- Last-modification timestamp (RFC3339 UTC).
    updated_at    TEXT NOT NULL,
    -- One agent label per tenant.
    UNIQUE (tenant, name)
);
CREATE INDEX idx_soma_presence_tenant_type ON soma_presence (tenant, agent_type);
CREATE INDEX idx_soma_presence_tenant_status ON soma_presence (tenant, status);
