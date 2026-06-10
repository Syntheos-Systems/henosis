-- V1: the Loom workflow store, extracted from kleos-lib onto the Henosis principal model.
-- The Kleos `user_id INTEGER` owner key is GONE (it was hardcoded to 1 in row mapping anyway);
-- workflows and runs are owner-scoped on `principal_id` per the projection convention, keyed
-- by WorkflowId/RunId (UUID v8) because they are referenced across services from Phase 5 on.
-- Steps and logs keep INTEGER surrogate keys: run-internal audit rows, the chiasm_task_updates
-- precedent. Append-only migration: never edit; add a V2 file for any schema change.

-- A workflow definition: a named, owner-scoped DAG of step definitions (validated at write
-- time: unique names, known dependency targets, acyclic).
CREATE TABLE loom_workflows (
    -- WorkflowId (UUID v8) in hyphenated string form.
    id           TEXT PRIMARY KEY NOT NULL,
    -- TenantId the workflow belongs to.
    tenant       TEXT NOT NULL,
    -- Owner PrincipalId. All reads/writes scope on this.
    principal_id TEXT NOT NULL,
    -- Workflow name, unique per owner.
    name         TEXT NOT NULL,
    -- Optional human-readable description.
    description  TEXT,
    -- JSON array of StepDef.
    steps        TEXT NOT NULL,
    -- Creation timestamp (RFC3339 UTC).
    created_at   TEXT NOT NULL,
    -- Last-modification timestamp (RFC3339 UTC).
    updated_at   TEXT NOT NULL,
    -- One workflow name per owner.
    UNIQUE (principal_id, name)
);

-- One run of a workflow.
CREATE TABLE loom_runs (
    -- RunId (UUID v8) in hyphenated string form.
    id           TEXT PRIMARY KEY NOT NULL,
    -- The workflow this run executes.
    workflow_id  TEXT NOT NULL REFERENCES loom_workflows (id) ON DELETE CASCADE,
    -- TenantId the run belongs to.
    tenant       TEXT NOT NULL,
    -- Owner PrincipalId (the runner).
    principal_id TEXT NOT NULL,
    -- RunStatus token: 'pending', 'running', 'completed', 'failed', 'cancelled'.
    status       TEXT NOT NULL DEFAULT 'pending',
    -- The run input object (JSON).
    input        TEXT NOT NULL DEFAULT '{}',
    -- Merged step outputs once completed (JSON).
    output       TEXT NOT NULL DEFAULT '{}',
    -- Failure reason, when failed.
    error        TEXT,
    -- When the first advance pass started the run (RFC3339 UTC).
    started_at   TEXT,
    -- When the run reached a terminal state (RFC3339 UTC).
    completed_at TEXT,
    -- Creation timestamp (RFC3339 UTC).
    created_at   TEXT NOT NULL,
    -- Last-modification timestamp (RFC3339 UTC).
    updated_at   TEXT NOT NULL
);
CREATE INDEX idx_loom_runs_principal ON loom_runs (principal_id, status);
CREATE INDEX idx_loom_runs_workflow ON loom_runs (workflow_id);

-- One step instance within a run (instantiated from the definition at run creation).
CREATE TABLE loom_steps (
    -- Run-internal step id (audit-style surrogate key).
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The run this step belongs to.
    run_id       TEXT NOT NULL REFERENCES loom_runs (id) ON DELETE CASCADE,
    -- Step name from the definition (depends_on refers to these).
    name         TEXT NOT NULL,
    -- StepType token: 'action', 'decision', 'parallel', 'wait', 'webhook', 'llm', 'transform'.
    step_type    TEXT NOT NULL,
    -- Executor-specific configuration (JSON).
    config       TEXT NOT NULL DEFAULT '{}',
    -- StepStatus token: 'pending', 'running', 'completed', 'failed', 'skipped'.
    status       TEXT NOT NULL DEFAULT 'pending',
    -- The merged input handed to the step when it started (JSON).
    input        TEXT NOT NULL DEFAULT '{}',
    -- The step output once completed (JSON).
    output       TEXT NOT NULL DEFAULT '{}',
    -- Last failure message, if any (survives a retry reset).
    error        TEXT,
    -- JSON array of dependency step names.
    depends_on   TEXT NOT NULL DEFAULT '[]',
    -- How many times the step has been retried.
    retry_count  INTEGER NOT NULL DEFAULT 0,
    -- Retry budget before the step fails the run.
    max_retries  INTEGER NOT NULL DEFAULT 3,
    -- Per-attempt timeout in milliseconds (enforced by the timeout sweep).
    timeout_ms   INTEGER NOT NULL DEFAULT 30000,
    -- When the current attempt started (RFC3339 UTC).
    started_at   TEXT,
    -- When the step reached a terminal state (RFC3339 UTC).
    completed_at TEXT,
    -- Creation timestamp (RFC3339 UTC).
    created_at   TEXT NOT NULL,
    -- One step name per run.
    UNIQUE (run_id, name)
);
CREATE INDEX idx_loom_steps_run ON loom_steps (run_id, status);

-- The run execution log (append-only).
CREATE TABLE loom_logs (
    -- Append-only log id.
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The run this line belongs to.
    run_id     TEXT NOT NULL REFERENCES loom_runs (id) ON DELETE CASCADE,
    -- The step it concerns, when step-scoped.
    step_id    INTEGER,
    -- LogLevel token: 'info', 'warn', 'error'.
    level      TEXT NOT NULL,
    -- The log message.
    message    TEXT NOT NULL,
    -- Structured detail (JSON).
    data       TEXT NOT NULL DEFAULT '{}',
    -- When the line was recorded (RFC3339 UTC).
    created_at TEXT NOT NULL
);
CREATE INDEX idx_loom_logs_run ON loom_logs (run_id);
