-- V2: capability policies governing the use-without-holding resolve modes.
-- The phylax_* identifiers remain unchanged for compatibility with existing databases.
-- Scope is `tenant` plus an optional `principal_id`.
-- The four resolve modes (sign/verify/derive/exec) are DENY-BY-DEFAULT: a request is
-- permitted only when a matching policy names the mode in allowed_modes (and, for exec, the
-- argv[0] is on exec_allowlist). Append-only migration: never edit; add a V3 for any change.

CREATE TABLE phylax_policies (
    -- Surrogate row id.
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    -- TenantId (UUID string) the policy belongs to. All matching scopes on this.
    tenant          TEXT NOT NULL,
    -- PrincipalId (UUID string) the policy is scoped to, or NULL for any principal in the
    -- tenant. A principal-specific policy is more specific than a tenant-wide (NULL) one.
    principal_id    TEXT,
    -- Category filter, or NULL to match every category.
    category        TEXT,
    -- Secret-name filter, or NULL to match every name in the category.
    secret_name     TEXT,
    -- JSON array: subset of ["sign","verify","derive","exec"] this policy permits.
    allowed_modes   TEXT NOT NULL,
    -- JSON array of absolute argv[0] paths exec may spawn, or NULL = exec never allowed by
    -- this policy even if "exec" is in allowed_modes. The allowlist is the capability.
    exec_allowlist  TEXT,
    -- RFC3339 UTC creation time.
    created_at      TEXT NOT NULL,
    UNIQUE(tenant, principal_id, category, secret_name)
);

CREATE INDEX idx_phylax_policies_match
    ON phylax_policies(tenant, principal_id, category, secret_name);
