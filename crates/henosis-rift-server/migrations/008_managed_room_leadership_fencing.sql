-- Fence stale managed-room leaders at the Rift state authority.
ALTER TABLE bridge_server_state
    ADD COLUMN fencing_required BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN fencing_epoch BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN fencing_lease_id UUID,
    ADD CONSTRAINT bridge_server_fencing_shape_check CHECK (
        (fencing_epoch = 0 AND fencing_lease_id IS NULL)
        OR (fencing_epoch > 0 AND fencing_lease_id IS NOT NULL)
    );
