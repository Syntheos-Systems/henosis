CREATE TABLE IF NOT EXISTS local_usage_counter (
    tenant_id TEXT NOT NULL,
    dimension TEXT NOT NULL,
    day TEXT NOT NULL,
    used INTEGER NOT NULL CHECK (used >= 0),
    PRIMARY KEY (tenant_id, dimension, day)
) STRICT;

CREATE TABLE IF NOT EXISTS local_rate_bucket (
    tenant_id TEXT PRIMARY KEY,
    tokens REAL NOT NULL,
    last_refill_ms INTEGER NOT NULL
) STRICT;
