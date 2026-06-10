-- V3: legacy-absorption map tables (projection convention 3.1-3.2). These are MIGRATION
-- ARTIFACTS, not live projections: they exist so the one-time Kleos backfill is idempotent
-- and auditable, and they are scheduled to be DROPPED one release cycle after the absorption
-- completes (as a future migration). Append-only migration: never edit; add a V4+ file for
-- any schema change.

-- Maps each distinct legacy Kleos owner key to the PrincipalId minted for it (convention 3.2:
-- one PrincipalKind::Human principal per distinct legacy key, minted by the backfill ONLY --
-- never on demand in a request path). The `user_id` column below is the ONE sanctioned
-- occurrence of the legacy key in this crate (convention 6.1, check 1): it maps FROM that key
-- and is never a projection column. Soma's extraction (Story 1.2) cross-reads this table once,
-- during its own backfill, so the same legacy key maps to the same principal (convention 3.4).
CREATE TABLE chiasm_legacy_user_id_map (
    -- The legacy Kleos i64 owner key being retired.
    user_id      INTEGER NOT NULL PRIMARY KEY,
    -- The PrincipalId (UUID) minted for it.
    principal_id TEXT    NOT NULL UNIQUE
);

-- Maps each absorbed legacy task row to its minted TaskId, carrying the legacy stringly
-- `agent` label. Legacy agent strings are NOT minted as principals here -- agent identity is
-- Soma's domain (Story 1.2); minting them in Chiasm would create the cross-service divergence
-- the projection convention exists to prevent. Once Soma has enrolled agent principals, a
-- follow-up pass can resolve `legacy_agent` labels into `chiasm_tasks.assignee` values.
CREATE TABLE chiasm_legacy_task_id_map (
    -- The legacy i64 chiasm_tasks.id.
    legacy_task_id INTEGER NOT NULL PRIMARY KEY,
    -- The TaskId (UUID v8) the absorbed row now lives under.
    task_id        TEXT    NOT NULL UNIQUE REFERENCES chiasm_tasks (id) ON DELETE CASCADE,
    -- The legacy agent label, kept for the later Soma-driven assignee resolution.
    legacy_agent   TEXT
);
