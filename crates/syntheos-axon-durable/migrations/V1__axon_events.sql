-- Durable Axon sidecar schema (Story 2.4).
--
-- axon_events is the audit log: every write-through publish appends one row, in publish order.
-- seq is the replay/cursor ordinate (monotonic AUTOINCREMENT, never reused after prune);
-- event_id is the envelope's own identity and stays globally unique.
CREATE TABLE axon_events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id    TEXT NOT NULL UNIQUE,
    channel     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    tenant      TEXT NOT NULL,
    principal   TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    payload     TEXT NOT NULL
);

-- Replay and consume both scan (tenant, channel) ranges in seq order.
CREATE INDEX idx_axon_events_tenant_channel_seq ON axon_events (tenant, channel, seq);

-- One cursor per (consumer, tenant, channel): last_seq is the highest seq already delivered to
-- that consumer (0 = nothing consumed yet).
CREATE TABLE axon_cursors (
    consumer   TEXT NOT NULL,
    tenant     TEXT NOT NULL,
    channel    TEXT NOT NULL,
    last_seq   INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (consumer, tenant, channel)
);
