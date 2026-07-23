-- V3: server-owned authority credentials. Secret material is SHA-256 hashed before insertion;
-- plaintext machine-token and refresh-token values never enter persistent storage.
CREATE TABLE machine_token (
    id            TEXT PRIMARY KEY NOT NULL CHECK(length(id) = 36),
    secret_hash   BLOB NOT NULL CHECK(length(secret_hash) = 32),
    tenant_id     TEXT NOT NULL CHECK(length(tenant_id) = 36),
    principal_id  TEXT NOT NULL CHECK(length(principal_id) = 36),
    label         TEXT NOT NULL CHECK(length(CAST(label AS BLOB)) BETWEEN 1 AND 128),
    scopes_json   TEXT NOT NULL CHECK(length(CAST(scopes_json AS BLOB)) <= 25000),
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER,
    revoked_at    INTEGER,
    last_used_at  INTEGER
);
CREATE INDEX machine_token_tenant_idx ON machine_token (tenant_id, principal_id, created_at);

CREATE TABLE operator_refresh_family (
    id            TEXT PRIMARY KEY NOT NULL CHECK(length(id) = 36),
    tenant_id     TEXT NOT NULL CHECK(length(tenant_id) = 36),
    principal_id  TEXT NOT NULL CHECK(length(principal_id) = 36),
    created_at    INTEGER NOT NULL,
    revoked_at    INTEGER
);
CREATE INDEX operator_refresh_family_tenant_idx
    ON operator_refresh_family (tenant_id, principal_id, created_at);

CREATE TABLE operator_refresh_session (
    id            TEXT PRIMARY KEY NOT NULL CHECK(length(id) = 36),
    family_id     TEXT NOT NULL CHECK(length(family_id) = 36),
    secret_hash   BLOB NOT NULL UNIQUE CHECK(length(secret_hash) = 32),
    tenant_id     TEXT NOT NULL CHECK(length(tenant_id) = 36),
    principal_id  TEXT NOT NULL CHECK(length(principal_id) = 36),
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER,
    revoked_at    INTEGER,
    last_used_at  INTEGER
);
CREATE INDEX operator_refresh_session_tenant_idx
    ON operator_refresh_session (tenant_id, principal_id, created_at);
CREATE INDEX operator_refresh_session_family_idx
    ON operator_refresh_session (family_id, tenant_id, principal_id);
