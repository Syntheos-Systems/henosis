-- Backfill message_type for agent-authored rows.
--
-- Migration 003 added the column with DEFAULT 'user', but create_message
-- never stamped a value, so every message -- including agent posts -- landed
-- as 'user'. The server now stamps the resolved type on insert; this brings
-- historic rows in line. Bridge machinery always carried a text prefix
-- ('[STIMULUS] ', '[SYSTEM] ', '[EXEC] '), so those rows can be classified
-- precisely rather than lumped in as agent conversation. Derived entirely
-- from users.is_agent plus content, touches only rows still carrying the
-- default, and is idempotent.
--
-- Accounts whose is_agent flag is still FALSE when this runs (legacy agents
-- promoted by a later bridge boot) are converged at promotion time instead:
-- see db::retype_agent_messages, called from the provisioning route.
UPDATE messages m
SET message_type = CASE
    WHEN m.content LIKE '[STIMULUS] %' THEN 'stimulus'
    WHEN m.content LIKE '[SYSTEM] %' THEN 'system'
    WHEN m.content LIKE '[EXEC] %' THEN 'system'
    ELSE 'agent'
END
FROM users u
WHERE m.author_id = u.id
  AND u.is_agent
  AND m.message_type = 'user';
