-- Plutus policy authority core schema (migration 0001).
-- Applied via sqlx::migrate!() on PlutusStore::open; idempotent (IF NOT EXISTS throughout).
-- All IDs are UUID; all timestamps are stored as TEXT (RFC3339) to avoid a sqlx chrono feature dep.

-- Orgs are keyed by tenant id (the org IS the tenant in the Henosis identity model).
CREATE TABLE IF NOT EXISTS org (
    tenant_id   UUID        PRIMARY KEY,
    name        TEXT        NOT NULL,
    owner_id    UUID        NOT NULL,
    status      TEXT        NOT NULL DEFAULT 'active',   -- active | suspended | deleted
    plan_tier   TEXT        NOT NULL DEFAULT 'free',     -- free | pro | team | enterprise
    created_at  TEXT        NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- Membership: one role per (org, principal).
-- role column stores the canonical text: owner | admin | member | viewer | billing.
CREATE TABLE IF NOT EXISTS org_member (
    tenant_id    UUID        NOT NULL,
    principal_id UUID        NOT NULL,
    role         TEXT        NOT NULL,
    added_at     TEXT        NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (tenant_id, principal_id)
);

-- Per-org quota configuration (one row per org; populated from tier defaults on org creation).
-- Limits are BIGINT to accommodate Enterprise sentinels (i64::MAX) without overflow.
CREATE TABLE IF NOT EXISTS quota_config (
    tenant_id                  UUID    PRIMARY KEY,
    max_tasks_per_day          BIGINT  NOT NULL,
    max_tokens_per_day         BIGINT  NOT NULL,
    max_tool_calls_per_day     BIGINT  NOT NULL,
    max_memory_stores_per_day  BIGINT  NOT NULL,
    rate_limit_rpm             BIGINT  NOT NULL
);

-- Daily usage counters; (org, dimension, date) accumulates via atomic upsert.
-- dimension column matches QuotaDimension::as_str(): tasks | tokens | tool_calls | memory_stores.
-- day column is YYYY-MM-DD UTC.
CREATE TABLE IF NOT EXISTS usage_counter (
    tenant_id   UUID    NOT NULL,
    dimension   TEXT    NOT NULL,
    day         TEXT    NOT NULL,
    used        BIGINT  NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, dimension, day)
);

-- Token-bucket rate-limit state per org.
-- tokens is DOUBLE PRECISION (fractional tokens during refill).
-- last_refill is TEXT (RFC3339 UTC) parsed in application code; avoids sqlx chrono feature dep.
CREATE TABLE IF NOT EXISTS rate_limit_bucket (
    tenant_id   UUID                NOT NULL PRIMARY KEY,
    tokens      DOUBLE PRECISION    NOT NULL,
    last_refill TEXT                NOT NULL
);
