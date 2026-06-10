-- V4: the legacy task-id map gains a SOURCE dimension. The live Kleos deployment holds
-- chiasm data in TWO databases with independent AUTOINCREMENT id spaces (the shared monolith,
-- live through 2026-04-28, and the per-tenant shards that took over when sharding was
-- enabled). Keying the map on the bare legacy id made a two-source import collide -- and,
-- worse, made the idempotency check silently skip the second source's overlapping ids
-- (a convention-3.3 violation). The key is now (source, legacy_task_id), where source is an
-- operator-chosen label per imported database (e.g. 'monolith', 'tenant-1').
--
-- chiasm_legacy_user_id_map is deliberately NOT source-scoped: Kleos user ids are
-- registry-global across the monolith and every shard, and the same legacy key MUST map to
-- the same Human principal regardless of which database a row came from (convention 3.4).
--
-- SQLite cannot alter a primary key in place; rebuild the table. Rows written before this
-- migration (none exist in any production target; the tools shipped unreleased) carry the
-- source 'unlabeled'. Append-only migration: never edit; add a V5 file for any schema change.
CREATE TABLE chiasm_legacy_task_id_map_v2 (
    -- Operator-chosen label of the source database this row was absorbed from.
    source         TEXT    NOT NULL,
    -- The legacy i64 chiasm_tasks.id within that source's id space.
    legacy_task_id INTEGER NOT NULL,
    -- The TaskId (UUID v8) the absorbed row now lives under (one source row per task).
    task_id        TEXT    NOT NULL UNIQUE REFERENCES chiasm_tasks (id) ON DELETE CASCADE,
    -- The legacy agent label, kept for the later Soma-driven assignee resolution.
    legacy_agent   TEXT,
    PRIMARY KEY (source, legacy_task_id)
);
INSERT INTO chiasm_legacy_task_id_map_v2 (source, legacy_task_id, task_id, legacy_agent)
    SELECT 'unlabeled', legacy_task_id, task_id, legacy_agent FROM chiasm_legacy_task_id_map;
DROP TABLE chiasm_legacy_task_id_map;
ALTER TABLE chiasm_legacy_task_id_map_v2 RENAME TO chiasm_legacy_task_id_map;
