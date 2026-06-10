-- V1: the Thymus quality store, extracted from kleos-lib onto the Henosis principal model.
-- The Kleos `user_id INTEGER` owner key and stringly `agent`/`evaluator` columns are GONE:
-- ownership is `principal_id` and the evaluated agent / evaluator are PrincipalIds, per the
-- projection convention. Rows keep INTEGER surrogate keys -- they are Thymus-internal
-- content/audit rows that nothing outside the crate references by id. Append-only migration:
-- never edit; add a V2 file for any schema change.

-- An evaluation rubric: a named, owner-scoped set of weighted criteria (JSON array of
-- {name, weight, scale_min, scale_max}).
CREATE TABLE thymus_rubrics (
    -- Thymus-internal rubric id.
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- TenantId the rubric belongs to.
    tenant       TEXT NOT NULL,
    -- Owner PrincipalId. All reads/writes scope on this.
    principal_id TEXT NOT NULL,
    -- Rubric name, unique per owner.
    name         TEXT NOT NULL,
    -- Optional description.
    description  TEXT,
    -- JSON array of Criterion.
    criteria     TEXT NOT NULL,
    -- Creation timestamp (RFC3339 UTC).
    created_at   TEXT NOT NULL,
    -- Last-modification timestamp (RFC3339 UTC).
    updated_at   TEXT NOT NULL,
    -- One rubric name per owner.
    UNIQUE (principal_id, name)
);

-- One recorded evaluation. rubric_id RESTRICTs deletion: evaluations are the audit record,
-- and deleting their rubric would orphan their scores' meaning.
CREATE TABLE thymus_evaluations (
    -- Thymus-internal evaluation id.
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The rubric scored against.
    rubric_id     INTEGER NOT NULL REFERENCES thymus_rubrics (id) ON DELETE RESTRICT,
    -- TenantId the evaluation belongs to.
    tenant        TEXT NOT NULL,
    -- Owner PrincipalId.
    principal_id  TEXT NOT NULL,
    -- The evaluated agent's PrincipalId.
    agent         TEXT NOT NULL,
    -- The evaluating PrincipalId (human reviewer, judge agent, ...).
    evaluator     TEXT NOT NULL,
    -- What was evaluated (task title, session id, artifact, ...).
    subject       TEXT NOT NULL,
    -- The work's input, for audit (JSON).
    input         TEXT NOT NULL DEFAULT '{}',
    -- The work's output, for audit (JSON).
    output        TEXT NOT NULL DEFAULT '{}',
    -- Raw per-criterion scores (JSON object keyed by criterion name).
    scores        TEXT NOT NULL,
    -- The weighted overall score in [0, 1].
    overall_score REAL NOT NULL,
    -- Optional evaluator notes.
    notes         TEXT,
    -- When the evaluation was recorded (RFC3339 UTC).
    created_at    TEXT NOT NULL
);
CREATE INDEX idx_thymus_evaluations_owner ON thymus_evaluations (principal_id, agent);
CREATE INDEX idx_thymus_evaluations_rubric ON thymus_evaluations (rubric_id);

-- Quality-metric data points (a small time series per (agent, metric)).
CREATE TABLE thymus_metrics (
    -- Thymus-internal metric id.
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- TenantId the metric belongs to.
    tenant       TEXT NOT NULL,
    -- Owner PrincipalId.
    principal_id TEXT NOT NULL,
    -- The agent the metric describes (PrincipalId).
    agent        TEXT NOT NULL,
    -- Metric name (e.g. 'latency_ms').
    metric       TEXT NOT NULL,
    -- The data point.
    value        REAL NOT NULL,
    -- Free-form dimension tags (JSON object).
    tags         TEXT NOT NULL DEFAULT '{}',
    -- When the point was recorded (RFC3339 UTC).
    recorded_at  TEXT NOT NULL
);
CREATE INDEX idx_thymus_metrics_series ON thymus_metrics (principal_id, agent, metric);

-- Behavioral-drift observations (supervision signal; EidolonGate policy input in Phase 2).
CREATE TABLE thymus_drift_events (
    -- Thymus-internal event id.
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- TenantId the event belongs to.
    tenant       TEXT NOT NULL,
    -- Owner PrincipalId.
    principal_id TEXT NOT NULL,
    -- The drifting agent's PrincipalId.
    agent        TEXT NOT NULL,
    -- The session the drift was observed in, when known.
    session      TEXT,
    -- DriftType token: 'priority','framework','interaction','meaning','safety','structural'.
    drift_type   TEXT NOT NULL,
    -- DriftSeverity token: 'low','medium','high','critical'.
    severity     TEXT NOT NULL DEFAULT 'medium',
    -- The observed signal (what tripped the detector).
    signal       TEXT NOT NULL,
    -- When the event was recorded (RFC3339 UTC).
    created_at   TEXT NOT NULL
);
CREATE INDEX idx_thymus_drift_owner ON thymus_drift_events (principal_id, agent);
