-- V2: scope rubric-name uniqueness to the tenant, not just the owner.
--
-- V1 declared `UNIQUE (principal_id, name)`. One principal can belong to many
-- tenants (henosis-plutus keys org_member on `(tenant_id, principal_id)`), and
-- every authorization predicate in this crate scopes on `(tenant, principal_id)`,
-- so the V1 constraint let a write in one tenant reserve a name in all of them.
--
-- SQLite cannot drop an inline UNIQUE, so the table is rebuilt. `from_database`
-- applies migrations before enabling `PRAGMA foreign_keys`, so the DROP below is
-- not blocked by `thymus_evaluations`' ON DELETE RESTRICT. Surrogate ids are
-- carried across unchanged because evaluations reference them. Append-only.

CREATE TABLE thymus_rubrics_v2 (
    -- Thymus-internal rubric id.
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- TenantId the rubric belongs to.
    tenant       TEXT NOT NULL,
    -- Owner PrincipalId. All reads/writes scope on this.
    principal_id TEXT NOT NULL,
    -- Rubric name, unique per owner within one tenant.
    name         TEXT NOT NULL,
    -- Optional description.
    description  TEXT,
    -- JSON array of Criterion.
    criteria     TEXT NOT NULL,
    -- Creation timestamp (RFC3339 UTC).
    created_at   TEXT NOT NULL,
    -- Last-modification timestamp (RFC3339 UTC).
    updated_at   TEXT NOT NULL,
    -- One rubric name per owner, per tenant.
    UNIQUE (tenant, principal_id, name)
);

INSERT INTO thymus_rubrics_v2 (
    id, tenant, principal_id, name, description, criteria, created_at, updated_at
)
SELECT id, tenant, principal_id, name, description, criteria, created_at, updated_at
FROM thymus_rubrics;

DROP TABLE thymus_rubrics;

ALTER TABLE thymus_rubrics_v2 RENAME TO thymus_rubrics;
