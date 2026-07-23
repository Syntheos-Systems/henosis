-- V1: the Phylax credential store, absorbed from kleos-phylax/kleos-cred onto the Henosis
-- principal model. The Kleos `user_id INTEGER` owner key is GONE; ownership is `tenant` (a
-- TenantId UUID string). Secret values are AES-256-GCM encrypted at the field level before
-- insert (see src/crypto.rs); the column holds nonce||ciphertext+tag, never plaintext.
-- Append-only migration: never edit; add a V2 file for any schema change.

CREATE TABLE phylax_secrets (
    -- Surrogate row id. The secret's identity for callers is (tenant, category, name).
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    -- TenantId (UUID string) the secret belongs to. All reads/writes scope on this.
    tenant              TEXT NOT NULL,
    -- Secret category (namespace within the tenant).
    category            TEXT NOT NULL,
    -- Secret name within the category.
    name                TEXT NOT NULL,
    -- AES-256-GCM blob: nonce(12) || ciphertext+tag over the SecretData JSON. Never plaintext.
    secret_ciphertext   BLOB NOT NULL,
    -- RFC3339 UTC creation time.
    created_at          TEXT NOT NULL,
    -- RFC3339 UTC last-update time.
    updated_at          TEXT NOT NULL,
    UNIQUE(tenant, category, name)
);

-- Resolve and policy lookups are always tenant-scoped, usually by (category, name).
CREATE INDEX idx_phylax_secrets_lookup
    ON phylax_secrets(tenant, category, name);
