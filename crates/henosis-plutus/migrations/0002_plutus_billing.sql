-- Plutus billing schema (migration 0002).
-- Extends 0001 with Stripe-facing billing tables: customer id mapping, entitlements
-- (the tier grant backing org.plan_tier), the Stripe price -> tier map, and an
-- idempotent webhook event log.
-- Applied via sqlx::migrate!() on PlutusStore::open; idempotent (IF NOT EXISTS throughout).
-- All timestamps are stored as TEXT (RFC3339), matching 0001 -- the workspace sqlx has no
-- chrono feature, so timestamps are produced in SQL via to_char(now() AT TIME ZONE 'UTC', ...)
-- rather than bound from Rust DateTime values.

-- Maps a tenant to its Stripe customer id. One Stripe customer per tenant.
CREATE TABLE IF NOT EXISTS billing_customer (
    tenant_id           UUID    PRIMARY KEY,
    stripe_customer_id  TEXT    NOT NULL UNIQUE,
    created_at          TEXT    NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- Entitlement: the tier grant backing a tenant's org.plan_tier. Sourced either from a
-- live Stripe subscription (source = 'stripe') or an operator-issued manual grant
-- (source = 'manual'). stripe_subscription_id is UNIQUE but nullable: Postgres UNIQUE
-- constraints permit any number of NULLs, so every manual grant (which has no Stripe
-- subscription id) leaves stripe_subscription_id NULL and never collides with another
-- manual grant or with a Stripe-sourced row -- only actual duplicate subscription ids
-- conflict, which is exactly the idempotency behavior the webhook handler needs.
CREATE TABLE IF NOT EXISTS entitlement (
    id                      BIGSERIAL   PRIMARY KEY,
    tenant_id               UUID        NOT NULL,
    tier                    TEXT        NOT NULL,
    source                  TEXT        NOT NULL,   -- stripe | manual
    stripe_subscription_id  TEXT        UNIQUE,     -- NULL for manual grants
    status                  TEXT        NOT NULL,   -- active | past_due | canceled
    current_period_end     TEXT,                    -- RFC3339, NULL for manual grants
    created_at             TEXT        NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    updated_at              TEXT        NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- Lookups by tenant (e.g. "what is this org's current entitlement") are the hot path.
CREATE INDEX IF NOT EXISTS entitlement_tenant_id_idx ON entitlement (tenant_id);

-- Maps a Stripe price id to the quota tier it grants. Populated by the operator ahead of
-- time; read by the webhook handler when translating a subscription event's price into a
-- tier to apply via PlutusStore::apply_tier.
CREATE TABLE IF NOT EXISTS billing_price_map (
    stripe_price_id  TEXT  PRIMARY KEY,
    tier             TEXT  NOT NULL
);

-- Idempotency log for processed Stripe webhook events. event_id is the Stripe event id
-- (evt_...); the primary key enforces at-most-once processing via ON CONFLICT DO NOTHING,
-- so a redelivered webhook is recognized and skipped rather than double-applied.
-- payload is JSONB even though the workspace sqlx has no `json` feature enabled -- callers
-- bind the payload as a `&str` and cast it explicitly in the query text (`$n::jsonb`); a
-- caller that needs to read it back selects `payload::text` rather than decoding JSONB in Rust.
CREATE TABLE IF NOT EXISTS billing_event (
    event_id      TEXT    PRIMARY KEY,
    event_type    TEXT    NOT NULL,
    payload       JSONB   NOT NULL,
    received_at   TEXT    NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    processed_at  TEXT,
    outcome       TEXT
);
