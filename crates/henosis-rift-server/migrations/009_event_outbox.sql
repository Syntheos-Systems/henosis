-- Durable single-process publication queue for message mutation gateway events.
CREATE TABLE event_outbox (
    event_id UUID PRIMARY KEY,
    route_kind VARCHAR(16) NOT NULL,
    route_id UUID NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attempt_count BIGINT NOT NULL DEFAULT 0,
    last_attempt_at TIMESTAMPTZ,
    delivered_at TIMESTAMPTZ,
    CONSTRAINT event_outbox_route_kind_check CHECK (route_kind = 'channel'),
    CONSTRAINT event_outbox_attempt_count_check CHECK (attempt_count >= 0),
    CONSTRAINT event_outbox_message_type_check CHECK (
        payload ->> 'type' IS NOT NULL
        AND payload ->> 'type' IN ('MessageCreate', 'MessageUpdate', 'MessageDelete')
    ),
    CONSTRAINT event_outbox_event_identity_check CHECK (
        payload #>> '{data,event_id}' IS NOT NULL
        AND (payload #>> '{data,event_id}')::UUID = event_id
    ),
    CONSTRAINT event_outbox_route_identity_check CHECK (
        payload #>> '{data,channel_id}' IS NOT NULL
        AND (payload #>> '{data,channel_id}')::UUID = route_id
    ),
    CONSTRAINT event_outbox_delivery_shape_check CHECK (
        delivered_at IS NULL
        OR (attempt_count > 0 AND last_attempt_at IS NOT NULL)
    )
);

CREATE INDEX event_outbox_pending_idx
    ON event_outbox (created_at, event_id)
    WHERE delivered_at IS NULL;

CREATE INDEX event_outbox_delivered_retention_idx
    ON event_outbox (delivered_at, event_id)
    WHERE delivered_at IS NOT NULL;
