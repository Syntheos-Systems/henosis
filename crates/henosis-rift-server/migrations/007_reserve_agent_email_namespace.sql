-- Reserve the bridge-only agent email namespace at the database boundary.
-- NOT VALID preserves legacy rows for explicit operator review while enforcing
-- the constraint on every new or updated human identity.
ALTER TABLE users
    ADD CONSTRAINT users_reserve_agent_email_namespace
    CHECK (is_agent OR lower(btrim(email)) !~ '@agent[.]local$')
    NOT VALID;
