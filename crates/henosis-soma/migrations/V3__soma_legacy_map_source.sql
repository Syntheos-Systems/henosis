-- V3: the legacy agent map gains a SOURCE dimension. The live Kleos deployment holds soma
-- data in TWO databases with independent AUTOINCREMENT id spaces (the shared monolith and the
-- per-tenant shards); keying on the bare legacy id made a two-source import collide and made
-- the idempotency check silently skip the second source's overlapping ids. The key is now
-- (source, legacy_agent_id), with source an operator-chosen label per imported database.
--
-- principal_id is NOT unique anymore: the same logical agent (same name) typically exists in
-- both source databases, and the backfill reuses the already-minted Agent principal for a
-- same-(tenant, name) presence rather than minting a duplicate -- so two map rows (one per
-- source) may legitimately point at one principal.
--
-- soma_legacy_user_id_map is deliberately NOT source-scoped: Kleos user ids are
-- registry-global, and the same legacy key MUST map to the same Human principal regardless of
-- source (convention 3.4). Rows written before this migration (none exist in any production
-- target) carry the source 'unlabeled'. Append-only migration: never edit; add a V4 file for
-- any schema change.
CREATE TABLE soma_legacy_agent_map_v2 (
    -- Operator-chosen label of the source database this row was absorbed from.
    source          TEXT    NOT NULL,
    -- The legacy i64 soma_agents.id within that source's id space.
    legacy_agent_id INTEGER NOT NULL,
    -- The Agent PrincipalId this row resolved to (= soma_presence.principal_id; shared when
    -- the same-named agent was absorbed from more than one source).
    principal_id    TEXT    NOT NULL REFERENCES soma_presence (principal_id) ON DELETE CASCADE,
    -- The legacy agent label at absorption time.
    legacy_name     TEXT    NOT NULL,
    PRIMARY KEY (source, legacy_agent_id)
);
INSERT INTO soma_legacy_agent_map_v2 (source, legacy_agent_id, principal_id, legacy_name)
    SELECT 'unlabeled', legacy_agent_id, principal_id, legacy_name FROM soma_legacy_agent_map;
DROP TABLE soma_legacy_agent_map;
ALTER TABLE soma_legacy_agent_map_v2 RENAME TO soma_legacy_agent_map;
