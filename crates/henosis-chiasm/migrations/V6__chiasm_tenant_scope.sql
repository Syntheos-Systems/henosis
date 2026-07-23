-- V6: tenant-leading indexes for every task projection boundary.
--
-- Task, activity, claim, and dependency authorization now scopes on both tenant and principal.
-- Claims and dependencies retain their existing normalized task foreign keys; tenant identity
-- comes from the owning task so legacy rows need no destructive table rebuild.
CREATE INDEX idx_chiasm_tasks_tenant_principal_status
    ON chiasm_tasks (tenant, principal_id, status);
CREATE INDEX idx_chiasm_tasks_tenant_principal_project
    ON chiasm_tasks (tenant, principal_id, project);
CREATE INDEX idx_chiasm_task_activity_tenant_owner
    ON chiasm_task_activity (tenant, principal_id, task_id, id DESC);
