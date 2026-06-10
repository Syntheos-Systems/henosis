-- V1: the canonical principal registry. Exactly one row per PrincipalId (PRIMARY KEY enforces
-- the uniqueness invariant PrincipalProjection relies on, the same guarantee the in-memory
-- directory enforces via entry()). Append-only migration: never edit; add a new V2 file instead.
CREATE TABLE principals (
    -- Canonical actor id: a UUID v8 in hyphenated string form (PrincipalId's Display).
    id      TEXT PRIMARY KEY NOT NULL,
    -- PrincipalKind in its serde string form (e.g. "Agent", "Human").
    kind    TEXT NOT NULL,
    -- Optional human-readable display name (NULL when absent).
    display TEXT
);
