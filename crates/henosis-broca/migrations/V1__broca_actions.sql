-- V1: the Broca action-narration log, extracted from kleos-lib onto the Henosis principal
-- model. The Kleos `broca_actions` row carried a stringly `agent` plus a `user_id INTEGER`
-- owner; here the actor is the agent's own PrincipalId and reads scope by tenant. The Kleos
-- `axon_event_id` back-reference is GONE: the in-process bus is deliberately ephemeral (no
-- durable event ids); durable correlation arrives with syntheos-axon-durable (Phase 2.4).
-- Append-only migration: never edit; add a V2 file for any schema change.
CREATE TABLE broca_actions (
    -- Append-only log id (an audit log, not a principal projection, so a surrogate key fits).
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    -- TenantId the action belongs to. The feed and stats scope on this.
    tenant        TEXT NOT NULL,
    -- PrincipalId of the acting agent (replaces the Kleos `agent` string + `user_id` owner).
    principal_id  TEXT NOT NULL,
    -- Originating service name (e.g. 'chiasm', 'soma', 'henosis').
    service       TEXT NOT NULL,
    -- Action type token (e.g. 'task.started').
    action        TEXT NOT NULL,
    -- Structured payload, stored as JSON text.
    payload       TEXT NOT NULL DEFAULT '{}',
    -- Human-readable sentence; NULL when no template matched and no narrator produced one.
    narrative     TEXT,
    -- Insertion timestamp (RFC3339 UTC), for humans.
    created_at    TEXT NOT NULL,
    -- Insertion timestamp as integer Unix nanoseconds, for `since` filtering and ordering
    -- (nanosecond RFC3339 strings do not order reliably as text; the chiasm/soma precedent
    -- computes comparisons outside SQL -- a feed query needs them IN SQL, hence this column).
    created_at_ns INTEGER NOT NULL
);
CREATE INDEX idx_broca_actions_tenant ON broca_actions (tenant, id);
CREATE INDEX idx_broca_actions_principal ON broca_actions (tenant, principal_id);
CREATE INDEX idx_broca_actions_action ON broca_actions (tenant, action);
