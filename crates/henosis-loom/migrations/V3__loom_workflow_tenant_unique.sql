-- V3: scope workflow-name uniqueness to the tenant, not just the owner.
--
-- V1 declared `UNIQUE (principal_id, name)`. One principal can belong to many
-- tenants (henosis-plutus keys org_member on `(tenant_id, principal_id)`), and
-- every authorization predicate in this crate scopes on `(tenant, principal_id)`,
-- so the V1 constraint let a write in one tenant reserve a name in all of them.
--
-- SQLite cannot drop an inline UNIQUE, so the table is rebuilt. `from_database`
-- applies migrations before enabling `PRAGMA foreign_keys`, so the DROP below
-- does not fire `loom_runs`' ON DELETE CASCADE. Append-only: never edit.

CREATE TABLE loom_workflows_v3 (
    -- WorkflowId (UUID v8) in hyphenated string form.
    id           TEXT PRIMARY KEY NOT NULL,
    -- TenantId the workflow belongs to.
    tenant       TEXT NOT NULL,
    -- Owner PrincipalId. All reads/writes scope on this.
    principal_id TEXT NOT NULL,
    -- Workflow name, unique per owner within one tenant.
    name         TEXT NOT NULL,
    -- Optional human-readable description.
    description  TEXT,
    -- JSON array of StepDef.
    steps        TEXT NOT NULL,
    -- Creation timestamp (RFC3339 UTC).
    created_at   TEXT NOT NULL,
    -- Last-modification timestamp (RFC3339 UTC).
    updated_at   TEXT NOT NULL,
    -- One workflow name per owner, per tenant.
    UNIQUE (tenant, principal_id, name)
);

INSERT INTO loom_workflows_v3 (
    id, tenant, principal_id, name, description, steps, created_at, updated_at
)
SELECT id, tenant, principal_id, name, description, steps, created_at, updated_at
FROM loom_workflows;

DROP TABLE loom_workflows;

ALTER TABLE loom_workflows_v3 RENAME TO loom_workflows;

-- Recreate the V2 index; it was dropped with the old table.
CREATE INDEX idx_loom_workflows_tenant_principal_updated
    ON loom_workflows (tenant, principal_id, updated_at DESC);
