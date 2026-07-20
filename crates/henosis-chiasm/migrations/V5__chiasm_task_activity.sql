-- V5: append-only dispatcher action activity correlated to Chiasm tasks.
-- This projection deliberately does not mutate task status, summary, or update history.
CREATE TABLE chiasm_task_activity (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id      TEXT NOT NULL REFERENCES chiasm_tasks (id) ON DELETE CASCADE,
    tenant       TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    kind         TEXT NOT NULL,
    payload      TEXT NOT NULL,
    created_at   TEXT NOT NULL
);
CREATE INDEX idx_chiasm_task_activity_task
    ON chiasm_task_activity (task_id, id DESC);
CREATE INDEX idx_chiasm_task_activity_owner
    ON chiasm_task_activity (principal_id, task_id, id DESC);
