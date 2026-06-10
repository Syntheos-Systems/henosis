-- V2: path claims (TTL leases) + the task dependency DAG, ported from kleos-lib chiasm
-- claims.rs / dependencies.rs onto the Henosis principal model. The Kleos `agent TEXT` claim
-- holder is GONE; a claim is held by its task and scoped to the task's owner principal_id,
-- per 2026-06-09-principal-projection-convention.md. Append-only migration: never edit; add
-- a V3 file for any schema change.

-- A path claim: a TTL lease a task holds on a file path while an agent works it. A claim is
-- ACTIVE when released = 0 AND expires_at > now; expiry is compared in Rust, not SQL, because
-- stored timestamps are nanosecond-precision RFC3339 (same decision as the stale sweep).
CREATE TABLE chiasm_path_claims (
    -- Lease log id (an audit-log-style surrogate key, not a principal projection key).
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The task that holds this claim.
    task_id      TEXT NOT NULL REFERENCES chiasm_tasks (id) ON DELETE CASCADE,
    -- Owner PrincipalId of the claiming task; every claim read/write scopes on this.
    principal_id TEXT NOT NULL,
    -- Project the claimed path belongs to (denormalized from the task for conflict lookups).
    project      TEXT NOT NULL,
    -- The file path being claimed.
    path         TEXT NOT NULL,
    -- When the claim was created (RFC3339 UTC).
    claimed_at   TEXT NOT NULL,
    -- When the lease expires (RFC3339 UTC); heartbeats push this forward.
    expires_at   TEXT NOT NULL,
    -- 1 once explicitly released (release, stale sweep, or task completion flows).
    released     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_chiasm_path_claims_task ON chiasm_path_claims (task_id);
CREATE INDEX idx_chiasm_path_claims_lookup ON chiasm_path_claims (principal_id, project, path);

-- A dependency edge: task_id depends on depends_on completing first. BFS cycle detection at
-- insert time keeps this a DAG; both endpoints must be owned by the same principal (enforced
-- in the store -- cross-principal edges are forbidden by the projection convention).
CREATE TABLE chiasm_task_dependencies (
    -- Edge log id (audit-log-style surrogate key).
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The dependent (downstream) task.
    task_id    TEXT NOT NULL REFERENCES chiasm_tasks (id) ON DELETE CASCADE,
    -- The task that must complete first.
    depends_on TEXT NOT NULL REFERENCES chiasm_tasks (id) ON DELETE CASCADE,
    -- When the edge was created (RFC3339 UTC).
    created_at TEXT NOT NULL,
    -- At most one edge per (dependent, dependency) pair; duplicate inserts are ignored.
    UNIQUE (task_id, depends_on)
);
CREATE INDEX idx_chiasm_task_deps_depends_on ON chiasm_task_dependencies (depends_on);
