-- V1: the Chiasm task store, extracted from kleos-lib onto the Henosis principal model.
-- The Kleos `user_id INTEGER` tenant/owner key is GONE; ownership is `principal_id` (a
-- PrincipalId UUID), per 2026-06-09-principal-projection-convention.md. The task's own identity
-- is a TaskId (UUID v8) primary key, not an i64 autoincrement. Append-only migration: never edit;
-- add a V2 file for any schema change.
CREATE TABLE chiasm_tasks (
    -- TaskId (UUID v8) in hyphenated string form.
    id                      TEXT PRIMARY KEY NOT NULL,
    -- TenantId (UUID) the task belongs to.
    tenant                  TEXT NOT NULL,
    -- Owner PrincipalId (replaces Kleos user_id). All reads/writes scope on this.
    principal_id            TEXT NOT NULL,
    -- Assignee PrincipalId, or NULL when unassigned (replaces Kleos `agent` string sentinel).
    assignee                TEXT,
    -- Project identifier the task groups under.
    project                 TEXT NOT NULL,
    -- Human-readable task title.
    title                   TEXT NOT NULL,
    -- TaskStatus in its serde snake_case form (e.g. 'active', 'blocked_on_human').
    status                  TEXT NOT NULL DEFAULT 'active',
    -- Optional progress note / description.
    summary                 TEXT,
    -- Optional description of the expected output.
    expected_output         TEXT,
    -- Output format hint (e.g. 'raw', 'json', 'markdown').
    output_format           TEXT NOT NULL DEFAULT 'raw',
    -- Submitted output, once produced.
    output                  TEXT,
    -- Plan text (LLM generation is deferred to the Broca extraction; this column stores it).
    plan                    TEXT,
    -- Reviewer feedback.
    feedback                TEXT,
    -- Last heartbeat timestamp (RFC3339 UTC), or NULL if never beaten.
    last_heartbeat          TEXT,
    -- Seconds between expected heartbeats before the task is considered stale.
    heartbeat_interval_secs INTEGER NOT NULL DEFAULT 300,
    -- Creation timestamp (RFC3339 UTC).
    created_at              TEXT NOT NULL,
    -- Last-modification timestamp (RFC3339 UTC).
    updated_at              TEXT NOT NULL
);
CREATE INDEX idx_chiasm_tasks_principal ON chiasm_tasks (principal_id, status);
CREATE INDEX idx_chiasm_tasks_project ON chiasm_tasks (project);

-- Append-only history of task status/summary changes. Keyed by an autoincrement log id (this is
-- an audit log, not a principal projection, so a surrogate numeric id is appropriate here).
CREATE TABLE chiasm_task_updates (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT NOT NULL REFERENCES chiasm_tasks (id) ON DELETE CASCADE,
    status     TEXT NOT NULL,
    summary    TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_chiasm_task_updates_task ON chiasm_task_updates (task_id);
