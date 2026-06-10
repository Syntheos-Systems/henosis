-- V2: legacy-absorption map tables (projection convention 3.1-3.2, 3.4). MIGRATION ARTIFACTS,
-- not live projections: they make the one-time Kleos backfill idempotent and auditable, and are
-- scheduled to be DROPPED one release cycle after the absorption completes (a future migration).
-- Append-only migration: never edit; add a V3 file for any schema change.

-- Maps each distinct legacy Kleos owner key to the Human PrincipalId it stands for. Per
-- convention 3.4 the soma backfill REUSES the principal chiasm minted for the same legacy key
-- (the one sanctioned cross-service migration-table read); only keys chiasm never saw mint a
-- fresh principal here. The `user_id` column is the ONE sanctioned occurrence of the legacy
-- key in this crate (convention 6.1, check 1). Presence rows do NOT carry this owner -- the
-- map preserves the linkage for later owner attribution (Pistis grants), nothing else.
CREATE TABLE soma_legacy_user_id_map (
    -- The legacy Kleos i64 owner key being retired.
    user_id      INTEGER NOT NULL PRIMARY KEY,
    -- The Human PrincipalId (UUID) it maps to (chiasm's mint, reused, or freshly minted here).
    principal_id TEXT    NOT NULL UNIQUE
);

-- Maps each absorbed legacy soma_agents row to the Agent principal minted for it. The agent's
-- own principal IS the presence identity (the soma_presence PRIMARY KEY), so unlike chiasm this
-- map's principal_id doubles as the live row key. legacy_name is kept so chiasm's
-- chiasm_legacy_task_id_map.legacy_agent labels can later resolve to assignee principals.
CREATE TABLE soma_legacy_agent_map (
    -- The legacy i64 soma_agents.id.
    legacy_agent_id INTEGER NOT NULL PRIMARY KEY,
    -- The Agent PrincipalId minted for this row (= soma_presence.principal_id).
    principal_id    TEXT    NOT NULL UNIQUE REFERENCES soma_presence (principal_id) ON DELETE CASCADE,
    -- The legacy agent label at absorption time.
    legacy_name     TEXT    NOT NULL
);
