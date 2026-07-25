-- V2: support tenant-and-principal authorization predicates without rewriting V1 tables.

CREATE INDEX idx_loom_workflows_tenant_principal_updated
    ON loom_workflows (tenant, principal_id, updated_at DESC);

CREATE INDEX idx_loom_runs_tenant_principal_status
    ON loom_runs (tenant, principal_id, status);
