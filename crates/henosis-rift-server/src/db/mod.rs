//! Postgres data access for the rift server: users, refresh tokens,
//! servers, members, channels, messages, attachments, roles, invites,
//! DMs, and bridge state.

/// Persistent ownership and revisioned room agent configuration operations.
pub mod agent_control;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::attachment::Attachment;
use crate::models::channel::Channel;
use crate::models::leadership::RoomFence;
use crate::models::message::{MessageQuery, MessageWithAuthor};
use crate::models::permissions::perms;
use crate::models::role::Role;
use crate::models::server::{Invite, Member, Server};
use crate::models::user::User;
use crate::outbox;
use crate::ws::gateway::GatewayEvent;

/// Canonical email suffix reserved for identities created by an agent-only path.
pub(crate) const CLAIMABLE_AGENT_EMAIL_SUFFIX: &str = "@agent.local";

/// Failure while converging one bridge-managed agent inside a fenced transaction.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionAgentError {
    /// An existing human owns the requested username and cannot be promoted.
    #[error("requested agent username belongs to a human account")]
    HumanUsername,
    /// The presented managed-room generation is no longer current.
    #[error("managed room leadership fence is stale")]
    StaleLeadership,
    /// PostgreSQL could not complete or authorize the convergence transaction.
    #[error("agent provisioning database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Return whether a public email attempts to use the reserved agent namespace.
pub(crate) fn is_reserved_agent_email(email: &str) -> bool {
    email
        .trim()
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domain.eq_ignore_ascii_case("agent.local"))
}

// ───── Users ─────

/// Insert a human user account and return the created row.
pub async fn create_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    password_hash: &str,
    display_name: Option<&str>,
) -> Result<User, sqlx::Error> {
    sqlx::query_as::<_, User>(
        r#"INSERT INTO users (username, email, password_hash, display_name)
           VALUES ($1, $2, $3, $4)
           RETURNING *"#,
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .bind(display_name)
    .fetch_one(pool)
    .await
}

/// Fetch a user by primary key.
pub async fn get_user_by_id(pool: &PgPool, id: Uuid) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Fetch a user by exact username.
pub async fn get_user_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = $1")
        .bind(username)
        .fetch_optional(pool)
        .await
}

/// Fetch a user by exact email.
pub async fn get_user_by_email(pool: &PgPool, email: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1")
        .bind(email)
        .fetch_optional(pool)
        .await
}

/// Patch display name, about, and avatar; absent fields keep their values.
pub async fn update_user_profile(
    pool: &PgPool,
    user_id: Uuid,
    display_name: Option<&str>,
    about: Option<&str>,
    avatar_url: Option<&str>,
) -> Result<User, sqlx::Error> {
    sqlx::query_as::<_, User>(
        r#"UPDATE users
           SET display_name = COALESCE($2, display_name),
               about = COALESCE($3, about),
               avatar_url = COALESCE($4, avatar_url),
               updated_at = NOW()
           WHERE id = $1
           RETURNING *"#,
    )
    .bind(user_id)
    .bind(display_name)
    .bind(about)
    .bind(avatar_url)
    .fetch_one(pool)
    .await
}

/// Replace a user's email address.
pub async fn update_user_email(
    pool: &PgPool,
    user_id: Uuid,
    email: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET email = $2, updated_at = NOW() WHERE id = $1")
        .bind(user_id)
        .bind(email)
        .execute(pool)
        .await?;
    Ok(())
}

/// Conditionally replace a verified password hash and revoke every refresh token atomically.
pub async fn update_user_password(
    pool: &PgPool,
    user_id: Uuid,
    expected_password_hash: &str,
    new_password_hash: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        r#"UPDATE users
           SET password_hash = $3, updated_at = NOW()
           WHERE id = $1 AND password_hash = $2"#,
    )
    .bind(user_id)
    .bind(expected_password_hash)
    .bind(new_password_hash)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query("DELETE FROM refresh_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Set a user's presence status string.
pub async fn update_user_status(
    pool: &PgPool,
    user_id: Uuid,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET status = $2, updated_at = NOW() WHERE id = $1")
        .bind(user_id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

// ───── Refresh Tokens ─────

/// Persist a refresh token hash with its expiry.
pub async fn store_refresh_token(
    pool: &PgPool,
    user_id: Uuid,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO refresh_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(token_hash)
        .bind(expires_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// Return the owning user id if the token hash exists and is unexpired.
pub async fn validate_refresh_token(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT user_id FROM refresh_tokens WHERE token_hash = $1 AND expires_at > NOW()",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Atomically delete an unexpired token hash, returning its owner (single-use rotation).
pub async fn consume_refresh_token(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "DELETE FROM refresh_tokens WHERE token_hash = $1 AND expires_at > NOW() RETURNING user_id",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Delete one refresh token by hash.
pub async fn delete_refresh_token(pool: &PgPool, token_hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM refresh_tokens WHERE token_hash = $1")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete every refresh token a user holds (global logout).
pub async fn delete_user_refresh_tokens(pool: &PgPool, user_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM refresh_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ───── Servers ─────

/// Insert a server owned by the given user.
pub async fn create_server(
    pool: &PgPool,
    name: &str,
    description: Option<&str>,
    owner_id: Uuid,
) -> Result<Server, sqlx::Error> {
    sqlx::query_as::<_, Server>(
        r#"INSERT INTO servers (name, description, owner_id)
           VALUES ($1, $2, $3)
           RETURNING *"#,
    )
    .bind(name)
    .bind(description)
    .bind(owner_id)
    .fetch_one(pool)
    .await
}

/// Fetch a server by primary key.
pub async fn get_server_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Server>, sqlx::Error> {
    sqlx::query_as::<_, Server>("SELECT * FROM servers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Patch server name and description; absent fields keep their values.
pub async fn update_server(
    pool: &PgPool,
    server_id: Uuid,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<Server, sqlx::Error> {
    sqlx::query_as::<_, Server>(
        r#"UPDATE servers
           SET name = COALESCE($2, name),
               description = COALESCE($3, description)
           WHERE id = $1
           RETURNING *"#,
    )
    .bind(server_id)
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await
}

/// Delete a server (children cascade via foreign keys).
pub async fn delete_server(pool: &PgPool, server_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM servers WHERE id = $1")
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// List the servers a user is a member of, ordered by name.
pub async fn get_user_servers(pool: &PgPool, user_id: Uuid) -> Result<Vec<Server>, sqlx::Error> {
    sqlx::query_as::<_, Server>(
        r#"SELECT s.* FROM servers s
           INNER JOIN members m ON s.id = m.server_id
           WHERE m.user_id = $1
           ORDER BY s.name"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

// ───── Members ─────

/// Join a user to a server, returning the membership row.
///
/// Idempotent: re-adding an existing member returns the existing row rather
/// than erroring. The no-op `DO UPDATE` is deliberate -- `DO NOTHING` suppresses
/// the `RETURNING` clause, so `fetch_one` used to fail with `RowNotFound` for an
/// already-joined user. That made every bridge restart against a populated
/// database look like a provisioning failure.
pub async fn add_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<Member, sqlx::Error> {
    sqlx::query_as::<_, Member>(
        r#"INSERT INTO members (server_id, user_id)
           VALUES ($1, $2)
           ON CONFLICT (server_id, user_id)
           DO UPDATE SET server_id = EXCLUDED.server_id
           RETURNING *"#,
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
}

/// Converge one agent identity, membership, and message history under one room fence lock.
pub async fn provision_agent_with_fence(
    pool: &PgPool,
    server_id: Uuid,
    fence: Option<&RoomFence>,
    username: &str,
    email: &str,
    password_hash: &str,
    display_name: Option<&str>,
) -> Result<(User, u64), ProvisionAgentError> {
    let mut transaction = pool.begin().await?;
    match agent_control::require_room_fence_locked(&mut transaction, server_id, fence).await {
        Ok(()) => {}
        Err(agent_control::RoomFenceError::Stale) => {
            return Err(ProvisionAgentError::StaleLeadership);
        }
        Err(agent_control::RoomFenceError::Database(error)) => {
            return Err(ProvisionAgentError::Database(error));
        }
    }

    let existing = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = $1 FOR UPDATE")
        .bind(username)
        .fetch_optional(&mut *transaction)
        .await?;
    let user = match existing {
        Some(user) if user.is_agent => user,
        Some(_) => return Err(ProvisionAgentError::HumanUsername),
        None => {
            sqlx::query_as::<_, User>(
                r#"INSERT INTO users (username, email, password_hash, display_name, is_agent)
                   VALUES ($1, $2, $3, $4, TRUE)
                   RETURNING *"#,
            )
            .bind(username)
            .bind(email)
            .bind(password_hash)
            .bind(display_name)
            .fetch_one(&mut *transaction)
            .await?
        }
    };
    sqlx::query(
        r#"INSERT INTO members (server_id, user_id)
           VALUES ($1, $2)
           ON CONFLICT (server_id, user_id)
           DO UPDATE SET server_id = EXCLUDED.server_id"#,
    )
    .bind(server_id)
    .bind(user.id)
    .execute(&mut *transaction)
    .await?;
    let retyped = sqlx::query(
        r#"UPDATE messages
           SET message_type = CASE
               WHEN content LIKE '[STIMULUS] %' THEN 'stimulus'
               WHEN content LIKE '[SYSTEM] %' THEN 'system'
               WHEN content LIKE '[EXEC] %' THEN 'system'
               ELSE 'agent'
           END
           WHERE author_id = $1
             AND message_type = 'user'"#,
    )
    .bind(user.id)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    transaction.commit().await?;
    Ok((user, retyped))
}

/// Create a user that is flagged as an agent from birth.
///
/// Distinct from `create_user`, which always produces a human account
/// (`is_agent` defaults to FALSE). Agents never authenticate themselves -- the
/// bridge mints their tokens -- so callers pass a hash of an unguessable random
/// password purely to keep the login path fail-closed.
pub async fn create_agent_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    password_hash: &str,
    display_name: Option<&str>,
) -> Result<User, sqlx::Error> {
    sqlx::query_as::<_, User>(
        r#"INSERT INTO users (username, email, password_hash, display_name, is_agent)
           VALUES ($1, $2, $3, $4, TRUE)
           RETURNING *"#,
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .bind(display_name)
    .fetch_one(pool)
    .await
}

/// Atomically create an agent identity and assign its first human owner.
pub async fn create_owned_agent_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    password_hash: &str,
    display_name: Option<&str>,
    owner_user_id: Uuid,
) -> Result<User, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let user = sqlx::query_as::<_, User>(
        r#"INSERT INTO users (username, email, password_hash, display_name, is_agent)
           SELECT $1, $2, $3, $4, TRUE
           FROM users owner
           WHERE owner.id = $5 AND owner.is_agent = FALSE
           RETURNING users.*"#,
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .bind(display_name)
    .bind(owner_user_id)
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query("INSERT INTO agent_ownership (agent_user_id, owner_user_id) VALUES ($1, $2)")
        .bind(user.id)
        .bind(owner_user_id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(user)
}

/// Retype an agent's historic messages still carrying the 'user' column
/// default, returning how many rows changed.
///
/// Mirrors migration 004's classification exactly. The migration runs once
/// at boot before a pre-stamping server build necessarily stops writing
/// 'user' rows, so provisioning must converge each agent's history itself.
pub async fn retype_agent_messages(pool: &PgPool, user_id: Uuid) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"UPDATE messages
           SET message_type = CASE
               WHEN content LIKE '[STIMULUS] %' THEN 'stimulus'
               WHEN content LIKE '[SYSTEM] %' THEN 'system'
               WHEN content LIKE '[EXEC] %' THEN 'system'
               ELSE 'agent'
           END
           WHERE author_id = $1
             AND message_type = 'user'"#,
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Remove a user's membership from a server.
pub async fn remove_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM members WHERE server_id = $1 AND user_id = $2")
        .bind(server_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Fetch one membership row.
pub async fn get_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<Option<Member>, sqlx::Error> {
    sqlx::query_as::<_, Member>("SELECT * FROM members WHERE server_id = $1 AND user_id = $2")
        .bind(server_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// List a server's members joined with their user rows (emails scrubbed).
pub async fn get_server_members(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Vec<(Member, User)>, sqlx::Error> {
    // Return as flat rows, assemble in caller
    let rows = sqlx::query_as::<_, MemberWithUser>(
        r#"SELECT m.server_id, m.user_id, m.nickname, m.joined_at,
                  u.username, u.display_name, u.avatar_url, u.status,
                  u.about, u.created_at, u.updated_at,
                  u.is_agent, u.executor_type, u.agent_roster_id
           FROM members m
           INNER JOIN users u ON m.user_id = u.id
           WHERE m.server_id = $1
           ORDER BY m.joined_at"#,
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                Member {
                    server_id: r.server_id,
                    user_id: r.user_id,
                    nickname: r.nickname.clone(),
                    joined_at: r.joined_at,
                },
                User {
                    id: r.user_id,
                    username: r.username,
                    display_name: r.display_name,
                    email: String::new(), // not exposed
                    password_hash: String::new(),
                    avatar_url: r.avatar_url,
                    status: r.status,
                    about: r.about,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                    is_agent: r.is_agent,
                    executor_type: r.executor_type,
                    agent_roster_id: r.agent_roster_id,
                },
            )
        })
        .collect())
}

/// Flat row shape for the members-with-users join.
#[derive(Debug, sqlx::FromRow)]
struct MemberWithUser {
    server_id: Uuid,
    user_id: Uuid,
    nickname: Option<String>,
    joined_at: DateTime<Utc>,
    username: String,
    display_name: Option<String>,
    avatar_url: Option<String>,
    status: String,
    about: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    is_agent: bool,
    executor_type: Option<String>,
    agent_roster_id: Option<String>,
}

/// True when the user has a membership row in the server.
pub async fn is_member(pool: &PgPool, server_id: Uuid, user_id: Uuid) -> Result<bool, sqlx::Error> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT COUNT(*) FROM members WHERE server_id = $1 AND user_id = $2")
            .bind(server_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0 > 0).unwrap_or(false))
}

/// Atomically claim an agent only when the human manages a shared server.
pub async fn claim_agent_as_shared_manager(
    pool: &PgPool,
    owner_user_id: Uuid,
    agent_user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    use crate::models::permissions::perms;

    let claimed = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO agent_ownership (agent_user_id, owner_user_id)
           SELECT agent.id, owner.id
           FROM users agent
           CROSS JOIN users owner
           WHERE agent.id = $2
             AND agent.is_agent = TRUE
             AND agent.email = agent.username || $5
             AND agent.executor_type IS DISTINCT FROM 'System'
             AND agent.agent_roster_id IS DISTINCT FROM 'henosis-room-owner'
             AND owner.id = $1
             AND owner.is_agent = FALSE
             AND EXISTS (
                 SELECT 1
                 FROM members owner_member
                 INNER JOIN members agent_member
                   ON agent_member.server_id = owner_member.server_id
                 INNER JOIN servers server
                   ON server.id = owner_member.server_id
                 WHERE owner_member.user_id = owner.id
                   AND agent_member.user_id = agent.id
                   AND (
                       server.owner_id = owner.id
                       OR EXISTS (
                           SELECT 1
                           FROM roles role
                           WHERE role.server_id = server.id
                             AND (
                                 role.is_default = TRUE
                                 OR EXISTS (
                                     SELECT 1
                                     FROM member_roles member_role
                                     WHERE member_role.server_id = server.id
                                       AND member_role.user_id = owner.id
                                       AND member_role.role_id = role.id
                                 )
                             )
                             AND (
                                 (role.permissions & $3) <> 0
                                 OR (role.permissions & $4) <> 0
                             )
                       )
                   )
             )
           ON CONFLICT (agent_user_id) DO NOTHING
           RETURNING agent_user_id"#,
    )
    .bind(owner_user_id)
    .bind(agent_user_id)
    .bind(perms::MANAGE_SERVER)
    .bind(perms::ADMINISTRATOR)
    .bind(CLAIMABLE_AGENT_EMAIL_SUFFIX)
    .fetch_optional(pool)
    .await?;
    Ok(claimed.is_some())
}

// ───── Channels ─────

/// Insert a channel at the next position within the server.
pub async fn create_channel(
    pool: &PgPool,
    server_id: Uuid,
    name: &str,
    topic: Option<&str>,
    channel_type: &str,
) -> Result<Channel, sqlx::Error> {
    // Get next position
    let pos: Option<(i32,)> =
        sqlx::query_as("SELECT COALESCE(MAX(position), -1) FROM channels WHERE server_id = $1")
            .bind(server_id)
            .fetch_optional(pool)
            .await?;
    let position = pos.map(|r| r.0 + 1).unwrap_or(0);

    sqlx::query_as::<_, Channel>(
        r#"INSERT INTO channels (server_id, name, topic, channel_type, position)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING *"#,
    )
    .bind(server_id)
    .bind(name)
    .bind(topic)
    .bind(channel_type)
    .bind(position)
    .fetch_one(pool)
    .await
}

/// Fetch a channel by primary key.
pub async fn get_channel_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Channel>, sqlx::Error> {
    sqlx::query_as::<_, Channel>("SELECT * FROM channels WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// List a server's channels in position order.
pub async fn get_server_channels(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Vec<Channel>, sqlx::Error> {
    sqlx::query_as::<_, Channel>("SELECT * FROM channels WHERE server_id = $1 ORDER BY position")
        .bind(server_id)
        .fetch_all(pool)
        .await
}

/// Patch channel name, topic, and position; absent fields keep their values.
pub async fn update_channel(
    pool: &PgPool,
    channel_id: Uuid,
    name: Option<&str>,
    topic: Option<&str>,
    position: Option<i32>,
) -> Result<Channel, sqlx::Error> {
    sqlx::query_as::<_, Channel>(
        r#"UPDATE channels
           SET name = COALESCE($2, name),
               topic = COALESCE($3, topic),
               position = COALESCE($4, position)
           WHERE id = $1
           RETURNING *"#,
    )
    .bind(channel_id)
    .bind(name)
    .bind(topic)
    .bind(position)
    .fetch_one(pool)
    .await
}

/// Delete a channel (messages cascade via foreign keys).
pub async fn delete_channel(pool: &PgPool, channel_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM channels WHERE id = $1")
        .bind(channel_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ───── Messages ─────

/// Domain separator for per-channel message creation advisory locks.
const MESSAGE_CREATE_LOCK_DOMAIN: u64 = 0x4845_4e4f_5349_534d;

/// Server-truth authorization mode held through one message transaction.
#[derive(Debug, Clone, Copy)]
pub enum MessageWriteAuthorization<'a> {
    /// A human account whose writes are independent of bridge leadership.
    Human,
    /// An agent account that must present the target room's current fence when managed.
    Agent {
        /// Server independently resolved from the message channel.
        server_id: Uuid,
        /// Optional capability retained for standalone, unfenced Rift rooms.
        fence: Option<&'a RoomFence>,
        /// Permission bits that must all remain granted when the insert executes.
        required_permissions: i64,
    },
}

/// Failure while reading one authorized channel message page under a stable room fence.
#[derive(Debug, thiserror::Error)]
pub enum ListMessagesError {
    /// The requested channel did not exist in the transaction snapshot.
    #[error("channel not found")]
    ChannelNotFound,
    /// The authenticated account was not a current member of the channel's room.
    #[error("message history access is forbidden")]
    Forbidden,
    /// The managed agent's room generation is absent or no longer current.
    #[error("managed room leadership fence is stale")]
    StaleLeadership,
    /// The requested cursor did not identify a message in the target channel.
    #[error("message cursor does not exist in channel")]
    InvalidCursor,
    /// PostgreSQL could not complete the authorized read transaction.
    #[error("message list database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Failure while inserting a message and its attachments atomically.
#[derive(Debug, thiserror::Error)]
pub enum CreateMessageError {
    /// The actor is no longer an agent member with every required permission.
    #[error("message write is forbidden")]
    Forbidden,
    /// The agent's managed-room generation is absent or no longer current.
    #[error("managed room leadership fence is stale")]
    StaleLeadership,
    /// PostgreSQL could not complete the message transaction.
    #[error("message database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Failure while editing or deleting one channel message under its target room fence.
#[derive(Debug, thiserror::Error)]
pub enum MessageMutationError {
    /// The route channel or message did not identify one current channel message.
    #[error("message not found in channel")]
    NotFound,
    /// The managed agent's room generation is absent or no longer current.
    #[error("managed room leadership fence is stale")]
    StaleLeadership,
    /// PostgreSQL could not complete the message mutation transaction.
    #[error("message mutation database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Validated attachment metadata inserted with its owning message transaction.
#[derive(Debug, Clone)]
pub struct NewAttachment {
    /// Original user-visible filename retained as inert metadata.
    pub filename: String,
    /// Opaque same-origin URL of the already staged object.
    pub url: String,
    /// Caller-declared media type retained as inert metadata.
    pub content_type: Option<String>,
    /// Stored object size in bytes.
    pub size_bytes: i64,
}

/// Return whether one permission set contains every requested bit or administrator authority.
fn permissions_contain_all(granted: i64, required: i64) -> bool {
    (granted & perms::ADMINISTRATOR) != 0 || (granted & required) == required
}

/// Lock and revalidate one managed agent's room membership and permissions before insertion.
async fn authorize_agent_message_write(
    connection: &mut PgConnection,
    channel_id: Uuid,
    author_id: Uuid,
    expected_server_id: Uuid,
    fence: Option<&RoomFence>,
    required_permissions: i64,
) -> Result<(), CreateMessageError> {
    let resolved_server_id: Option<Uuid> =
        sqlx::query_scalar("SELECT server_id FROM channels WHERE id = $1")
            .bind(channel_id)
            .fetch_optional(&mut *connection)
            .await?;
    if resolved_server_id != Some(expected_server_id) {
        return Err(CreateMessageError::Forbidden);
    }
    match agent_control::require_room_fence_locked(connection, expected_server_id, fence).await {
        Ok(()) => {}
        Err(agent_control::RoomFenceError::Stale) => {
            return Err(CreateMessageError::StaleLeadership);
        }
        Err(agent_control::RoomFenceError::Database(error)) => {
            return Err(CreateMessageError::Database(error));
        }
    }
    let scope: Option<(bool, bool)> = sqlx::query_as(
        r#"SELECT actor.is_agent, server.owner_id = actor.id
           FROM channels AS channel
           INNER JOIN servers AS server ON server.id = channel.server_id
           INNER JOIN users AS actor ON actor.id = $2
           INNER JOIN members AS membership
             ON membership.server_id = channel.server_id
            AND membership.user_id = actor.id
           WHERE channel.id = $1
             AND channel.server_id = $3
           FOR SHARE OF channel, server, actor, membership"#,
    )
    .bind(channel_id)
    .bind(author_id)
    .bind(expected_server_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some((true, is_owner)) = scope else {
        return Err(CreateMessageError::Forbidden);
    };
    if is_owner {
        return Ok(());
    }
    let role_ids = sqlx::query_scalar::<_, Uuid>(
        r#"SELECT role_id
           FROM member_roles
           WHERE server_id = $1 AND user_id = $2
           FOR SHARE"#,
    )
    .bind(expected_server_id)
    .bind(author_id)
    .fetch_all(&mut *connection)
    .await?;
    let permission_rows = sqlx::query_scalar::<_, i64>(
        r#"SELECT permissions
           FROM roles
           WHERE server_id = $1
             AND (is_default = TRUE OR id = ANY($2))
           FOR SHARE"#,
    )
    .bind(expected_server_id)
    .bind(&role_ids)
    .fetch_all(&mut *connection)
    .await?;
    let granted = permission_rows
        .into_iter()
        .fold(0_i64, |combined, permissions| combined | permissions);
    if !permissions_contain_all(granted, required_permissions) {
        return Err(CreateMessageError::Forbidden);
    }
    Ok(())
}

/// Insert a channel-ordered message with an explicit authorized type.
///
/// A transaction-scoped advisory lock serializes creation per channel, and the
/// following insert reads a fresh snapshot after acquiring that lock. Its
/// stored timestamp advances beyond that channel's current maximum even if the
/// host clock stalls, which keeps cursors monotonic across Rift processes.
pub async fn create_message(
    pool: &PgPool,
    channel_id: Uuid,
    author_id: Uuid,
    content: &str,
    message_type: &str,
) -> Result<MessageWithAuthor, CreateMessageError> {
    let (message, _) = create_message_with_attachments(
        pool,
        channel_id,
        author_id,
        content,
        message_type,
        MessageWriteAuthorization::Human,
        &[],
    )
    .await?;
    Ok(message)
}

/// Insert one channel-ordered message and all attachments in a single transaction.
pub async fn create_message_with_attachments(
    pool: &PgPool,
    channel_id: Uuid,
    author_id: Uuid,
    content: &str,
    message_type: &str,
    authorization: MessageWriteAuthorization<'_>,
    attachments: &[NewAttachment],
) -> Result<(MessageWithAuthor, Vec<Attachment>), CreateMessageError> {
    let mut transaction = pool.begin().await?;
    let result = create_message_with_attachments_in_transaction(
        &mut transaction,
        channel_id,
        author_id,
        content,
        message_type,
        authorization,
        attachments,
    )
    .await?;
    transaction.commit().await?;
    Ok(result)
}

/// Insert a message, attachments, and its stable gateway event in one caller-owned transaction.
pub(crate) async fn create_message_with_attachments_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    channel_id: Uuid,
    author_id: Uuid,
    content: &str,
    message_type: &str,
    authorization: MessageWriteAuthorization<'_>,
    attachments: &[NewAttachment],
) -> Result<(MessageWithAuthor, Vec<Attachment>), CreateMessageError> {
    let raw_channel_id = channel_id.as_u128();
    let folded_channel_id =
        (raw_channel_id as u64) ^ ((raw_channel_id >> 64) as u64) ^ MESSAGE_CREATE_LOCK_DOMAIN;
    let lock_key = i64::from_be_bytes(folded_channel_id.to_be_bytes());
    // Per-statement snapshots are required so a waiter observes the preceding
    // lock holder's committed message before choosing its own timestamp.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut **transaction)
        .await?;
    if let MessageWriteAuthorization::Agent {
        server_id,
        fence,
        required_permissions,
    } = authorization
    {
        authorize_agent_message_write(
            transaction,
            channel_id,
            author_id,
            server_id,
            fence,
            required_permissions,
        )
        .await?;
    }
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(lock_key)
        .execute(&mut **transaction)
        .await?;
    let message = sqlx::query_as::<_, MessageWithAuthor>(
        r#"WITH next_time AS (
               SELECT GREATEST(
                          clock_timestamp(),
                          COALESCE(
                              MAX(created_at) + INTERVAL '1 microsecond',
                              '-infinity'::timestamptz
                          )
                      ) AS created_at
               FROM messages
               WHERE channel_id = $1
           ),
           new_msg AS (
               INSERT INTO messages (channel_id, author_id, content, message_type, created_at)
               SELECT $1, $2, $3, $4, created_at
               FROM next_time
               RETURNING *
           )
           SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                  m.message_type,
                  u.username AS author_username,
                  u.display_name AS author_display_name,
                  u.avatar_url AS author_avatar_url
           FROM new_msg m
           INNER JOIN users u ON m.author_id = u.id"#,
    )
    .bind(channel_id)
    .bind(author_id)
    .bind(content)
    .bind(message_type)
    .fetch_one(&mut **transaction)
    .await?;
    let mut created_attachments = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let created = sqlx::query_as::<_, Attachment>(
            r#"INSERT INTO attachments (message_id, filename, url, content_type, size_bytes)
               VALUES ($1, $2, $3, $4, $5)
               RETURNING *"#,
        )
        .bind(message.id)
        .bind(&attachment.filename)
        .bind(&attachment.url)
        .bind(attachment.content_type.as_deref())
        .bind(attachment.size_bytes)
        .fetch_one(&mut **transaction)
        .await?;
        created_attachments.push(created);
    }
    let event = GatewayEvent::MessageCreate {
        event_id: Uuid::new_v4(),
        id: message.id,
        channel_id: message.channel_id,
        author_id: message.author_id,
        author_username: message.author_username.clone(),
        author_display_name: message.author_display_name.clone(),
        author_avatar_url: message.author_avatar_url.clone(),
        content: message.content.clone(),
        attachments: created_attachments.clone(),
        message_type: message.message_type.clone(),
        created_at: message.created_at.to_rfc3339(),
    };
    outbox::enqueue_message_event(transaction, &event).await?;
    Ok((message, created_attachments))
}

/// Report whether a message cursor belongs to the requested channel.
pub async fn message_cursor_exists_in_channel(
    pool: &PgPool,
    channel_id: Uuid,
    message_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    message_cursor_exists_on_connection(&mut connection, channel_id, message_id).await
}

/// Report whether a cursor belongs to the channel in the caller's stable transaction.
async fn message_cursor_exists_on_connection(
    connection: &mut PgConnection,
    channel_id: Uuid,
    message_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE channel_id = $1 AND id = $2)")
        .bind(channel_id)
        .bind(message_id)
        .fetch_one(connection)
        .await
}

/// Page a channel's messages with deterministic before/after cursor ordering.
pub async fn get_messages(
    pool: &PgPool,
    channel_id: Uuid,
    query: &MessageQuery,
) -> Result<Vec<MessageWithAuthor>, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    get_messages_on_connection(&mut connection, channel_id, query).await
}

/// Page messages through the caller's stable transaction snapshot.
async fn get_messages_on_connection(
    connection: &mut PgConnection,
    channel_id: Uuid,
    query: &MessageQuery,
) -> Result<Vec<MessageWithAuthor>, sqlx::Error> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);

    if let Some(before) = query.before {
        sqlx::query_as::<_, MessageWithAuthor>(
            r#"WITH boundary AS (
                   SELECT created_at, id
                   FROM messages
                   WHERE channel_id = $1 AND id = $2
               )
               SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                      m.message_type,
                      u.username AS author_username,
                      u.display_name AS author_display_name,
                      u.avatar_url AS author_avatar_url
               FROM messages m
               INNER JOIN users u ON m.author_id = u.id
               CROSS JOIN boundary b
               WHERE m.channel_id = $1
                 AND (m.created_at, m.id) < (b.created_at, b.id)
               ORDER BY m.created_at DESC, m.id DESC
               LIMIT $3"#,
        )
        .bind(channel_id)
        .bind(before)
        .bind(limit)
        .fetch_all(&mut *connection)
        .await
    } else if let Some(after) = query.after {
        if after == channel_id {
            return sqlx::query_as::<_, MessageWithAuthor>(
                r#"SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                          m.message_type,
                          u.username AS author_username,
                          u.display_name AS author_display_name,
                          u.avatar_url AS author_avatar_url
                   FROM messages m
                   INNER JOIN users u ON m.author_id = u.id
                   WHERE m.channel_id = $1
                   ORDER BY m.created_at ASC, m.id ASC
                   LIMIT $2"#,
            )
            .bind(channel_id)
            .bind(limit)
            .fetch_all(&mut *connection)
            .await;
        }
        sqlx::query_as::<_, MessageWithAuthor>(
            r#"WITH boundary AS (
                   SELECT created_at, id
                   FROM messages
                   WHERE channel_id = $1 AND id = $2
               )
               SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                      m.message_type,
                      u.username AS author_username,
                      u.display_name AS author_display_name,
                      u.avatar_url AS author_avatar_url
               FROM messages m
               INNER JOIN users u ON m.author_id = u.id
               CROSS JOIN boundary b
               WHERE m.channel_id = $1
                 AND (m.created_at, m.id) > (b.created_at, b.id)
               ORDER BY m.created_at ASC, m.id ASC
               LIMIT $3"#,
        )
        .bind(channel_id)
        .bind(after)
        .bind(limit)
        .fetch_all(&mut *connection)
        .await
    } else {
        // Latest messages (most recent first)
        sqlx::query_as::<_, MessageWithAuthor>(
            r#"SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                      m.message_type,
                      u.username AS author_username,
                      u.display_name AS author_display_name,
                      u.avatar_url AS author_avatar_url
               FROM messages m
               INNER JOIN users u ON m.author_id = u.id
               WHERE m.channel_id = $1
               ORDER BY m.created_at DESC, m.id DESC
               LIMIT $2"#,
        )
        .bind(channel_id)
        .bind(limit)
        .fetch_all(&mut *connection)
        .await
    }
}

/// Read one channel page, cursor, and attachments under one membership and fence snapshot.
pub async fn list_channel_messages_authorized(
    pool: &PgPool,
    channel_id: Uuid,
    user_id: Uuid,
    fence: Option<&RoomFence>,
    query: &MessageQuery,
) -> Result<(Vec<MessageWithAuthor>, Vec<Attachment>), ListMessagesError> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await?;
    let server_id: Option<Uuid> =
        sqlx::query_scalar("SELECT server_id FROM channels WHERE id = $1")
            .bind(channel_id)
            .fetch_optional(&mut *transaction)
            .await?;
    let Some(server_id) = server_id else {
        return Err(ListMessagesError::ChannelNotFound);
    };
    let actor_is_agent: Option<bool> =
        sqlx::query_scalar("SELECT is_agent FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&mut *transaction)
            .await?;
    let Some(actor_is_agent) = actor_is_agent else {
        return Err(ListMessagesError::Forbidden);
    };
    if actor_is_agent {
        match agent_control::require_room_fence_locked(&mut transaction, server_id, fence).await {
            Ok(()) => {}
            Err(agent_control::RoomFenceError::Stale) => {
                return Err(ListMessagesError::StaleLeadership);
            }
            Err(agent_control::RoomFenceError::Database(error)) => {
                return Err(ListMessagesError::Database(error));
            }
        }
    }
    let locked_membership: Option<bool> = sqlx::query_scalar(
        r#"SELECT actor.is_agent
           FROM channels AS channel
           INNER JOIN users AS actor ON actor.id = $2
           INNER JOIN members AS membership
             ON membership.user_id = actor.id
            AND membership.server_id = channel.server_id
           WHERE channel.id = $1
             AND channel.server_id = $3
           FOR SHARE OF channel, actor, membership"#,
    )
    .bind(channel_id)
    .bind(user_id)
    .bind(server_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if locked_membership != Some(actor_is_agent) {
        return Err(ListMessagesError::Forbidden);
    }
    let cursor = query.before.or(query.after);
    let reserved_beginning = query.before.is_none() && query.after == Some(channel_id);
    if let Some(cursor) = cursor
        && !reserved_beginning
        && !message_cursor_exists_on_connection(&mut transaction, channel_id, cursor).await?
    {
        return Err(ListMessagesError::InvalidCursor);
    }
    let messages = get_messages_on_connection(&mut transaction, channel_id, query).await?;
    let message_ids = messages
        .iter()
        .map(|message| message.id)
        .collect::<Vec<_>>();
    let attachments =
        get_attachments_for_messages_on_connection(&mut transaction, &message_ids).await?;
    transaction.commit().await?;
    Ok((messages, attachments))
}

/// Fetch one message joined with author info.
pub async fn get_message_by_id(
    pool: &PgPool,
    message_id: Uuid,
) -> Result<Option<MessageWithAuthor>, sqlx::Error> {
    sqlx::query_as::<_, MessageWithAuthor>(
        r#"SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                  m.message_type,
                  u.username AS author_username,
                  u.display_name AS author_display_name,
                  u.avatar_url AS author_avatar_url
           FROM messages m
           INNER JOIN users u ON m.author_id = u.id
           WHERE m.id = $1"#,
    )
    .bind(message_id)
    .fetch_optional(pool)
    .await
}

/// Lock one mutation target and authorize its agent actor against the target room fence.
async fn authorize_message_mutation(
    connection: &mut PgConnection,
    message_id: Uuid,
    channel_id: Uuid,
    actor_id: Uuid,
    fence: Option<&RoomFence>,
) -> Result<(), MessageMutationError> {
    let scope: Option<(Uuid, bool)> = sqlx::query_as(
        r#"SELECT channels.server_id, users.is_agent
           FROM messages
           INNER JOIN channels ON channels.id = messages.channel_id
           INNER JOIN users ON users.id = $3
           WHERE messages.id = $1
             AND messages.channel_id = $2
           FOR UPDATE OF messages"#,
    )
    .bind(message_id)
    .bind(channel_id)
    .bind(actor_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some((server_id, actor_is_agent)) = scope else {
        return Err(MessageMutationError::NotFound);
    };
    if actor_is_agent {
        match agent_control::require_room_fence_locked(connection, server_id, fence).await {
            Ok(()) => {}
            Err(agent_control::RoomFenceError::Stale) => {
                return Err(MessageMutationError::StaleLeadership);
            }
            Err(agent_control::RoomFenceError::Database(error)) => {
                return Err(MessageMutationError::Database(error));
            }
        }
    }
    Ok(())
}

/// Replace a channel message's content under its target room's current agent fence.
pub async fn update_message_with_fence(
    pool: &PgPool,
    message_id: Uuid,
    channel_id: Uuid,
    actor_id: Uuid,
    fence: Option<&RoomFence>,
    content: &str,
) -> Result<MessageWithAuthor, MessageMutationError> {
    let mut transaction = pool.begin().await?;
    let message = update_message_in_transaction(
        &mut transaction,
        message_id,
        channel_id,
        actor_id,
        fence,
        content,
    )
    .await?;
    transaction.commit().await?;
    Ok(message)
}

/// Edit a message and enqueue its stable gateway event in one caller-owned transaction.
pub(crate) async fn update_message_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    channel_id: Uuid,
    actor_id: Uuid,
    fence: Option<&RoomFence>,
    content: &str,
) -> Result<MessageWithAuthor, MessageMutationError> {
    authorize_message_mutation(transaction, message_id, channel_id, actor_id, fence).await?;
    let message = sqlx::query_as::<_, MessageWithAuthor>(
        r#"WITH updated AS (
               UPDATE messages SET content = $2, edited_at = NOW()
               WHERE id = $1 AND channel_id = $3
               RETURNING *
           )
           SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                  m.message_type,
                  u.username AS author_username,
                  u.display_name AS author_display_name,
                  u.avatar_url AS author_avatar_url
           FROM updated m
           INNER JOIN users u ON m.author_id = u.id"#,
    )
    .bind(message_id)
    .bind(content)
    .bind(channel_id)
    .fetch_one(&mut **transaction)
    .await?;
    let event = GatewayEvent::MessageUpdate {
        event_id: Uuid::new_v4(),
        id: message.id,
        channel_id: message.channel_id,
        content: message.content.clone(),
        edited_at: message
            .edited_at
            .map(|edited_at| edited_at.to_rfc3339())
            .unwrap_or_default(),
    };
    outbox::enqueue_message_event(transaction, &event).await?;
    Ok(message)
}

/// Delete a channel message under its target room's current agent fence.
pub async fn delete_message_with_fence(
    pool: &PgPool,
    message_id: Uuid,
    channel_id: Uuid,
    actor_id: Uuid,
    fence: Option<&RoomFence>,
) -> Result<(), MessageMutationError> {
    let mut transaction = pool.begin().await?;
    delete_message_in_transaction(&mut transaction, message_id, channel_id, actor_id, fence)
        .await?;
    transaction.commit().await?;
    Ok(())
}

/// Delete a message and enqueue its stable gateway event in one caller-owned transaction.
pub(crate) async fn delete_message_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    message_id: Uuid,
    channel_id: Uuid,
    actor_id: Uuid,
    fence: Option<&RoomFence>,
) -> Result<(), MessageMutationError> {
    authorize_message_mutation(transaction, message_id, channel_id, actor_id, fence).await?;
    let deleted = sqlx::query("DELETE FROM messages WHERE id = $1 AND channel_id = $2")
        .bind(message_id)
        .bind(channel_id)
        .execute(&mut **transaction)
        .await?;
    if deleted.rows_affected() != 1 {
        return Err(MessageMutationError::NotFound);
    }
    let event = GatewayEvent::MessageDelete {
        event_id: Uuid::new_v4(),
        id: message_id,
        channel_id,
    };
    outbox::enqueue_message_event(transaction, &event).await?;
    Ok(())
}

// ───── Roles ─────

// ───── Attachments ─────

/// Insert an attachment row for a message.
pub async fn create_attachment(
    pool: &PgPool,
    message_id: Uuid,
    filename: &str,
    url: &str,
    content_type: Option<&str>,
    size_bytes: Option<i64>,
) -> Result<Attachment, sqlx::Error> {
    sqlx::query_as::<_, Attachment>(
        r#"INSERT INTO attachments (message_id, filename, url, content_type, size_bytes)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING *"#,
    )
    .bind(message_id)
    .bind(filename)
    .bind(url)
    .bind(content_type)
    .bind(size_bytes)
    .fetch_one(pool)
    .await
}

/// List a message's attachments oldest-first.
pub async fn get_attachments_for_message(
    pool: &PgPool,
    message_id: Uuid,
) -> Result<Vec<Attachment>, sqlx::Error> {
    sqlx::query_as::<_, Attachment>(
        "SELECT * FROM attachments WHERE message_id = $1 ORDER BY created_at",
    )
    .bind(message_id)
    .fetch_all(pool)
    .await
}

/// Batch-load attachments for many messages in one query.
pub async fn get_attachments_for_messages(
    pool: &PgPool,
    message_ids: &[Uuid],
) -> Result<Vec<Attachment>, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    get_attachments_for_messages_on_connection(&mut connection, message_ids).await
}

/// Batch-load attachments through the caller's stable transaction snapshot.
async fn get_attachments_for_messages_on_connection(
    connection: &mut PgConnection,
    message_ids: &[Uuid],
) -> Result<Vec<Attachment>, sqlx::Error> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, Attachment>(
        "SELECT * FROM attachments WHERE message_id = ANY($1) ORDER BY created_at",
    )
    .bind(message_ids)
    .fetch_all(connection)
    .await
}

// ───── Roles (continued) ─────

/// Insert a role at the next position within the server.
pub async fn create_role(
    pool: &PgPool,
    server_id: Uuid,
    name: &str,
    color: i32,
    permissions: i64,
) -> Result<Role, sqlx::Error> {
    let pos: Option<(i32,)> =
        sqlx::query_as("SELECT COALESCE(MAX(position), -1) FROM roles WHERE server_id = $1")
            .bind(server_id)
            .fetch_optional(pool)
            .await?;
    let position = pos.map(|r| r.0 + 1).unwrap_or(0);

    sqlx::query_as::<_, Role>(
        r#"INSERT INTO roles (server_id, name, color, permissions, position)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING *"#,
    )
    .bind(server_id)
    .bind(name)
    .bind(color)
    .bind(permissions)
    .bind(position)
    .fetch_one(pool)
    .await
}

/// Insert the @everyone default role with baseline permissions.
pub async fn create_default_role(pool: &PgPool, server_id: Uuid) -> Result<Role, sqlx::Error> {
    use crate::models::permissions::perms;
    sqlx::query_as::<_, Role>(
        r#"INSERT INTO roles (server_id, name, color, permissions, position, is_default)
           VALUES ($1, '@everyone', 0, $2, 0, true)
           RETURNING *"#,
    )
    .bind(server_id)
    .bind(perms::DEFAULT)
    .fetch_one(pool)
    .await
}

/// Fetch a role by primary key.
pub async fn get_role_by_id(pool: &PgPool, role_id: Uuid) -> Result<Option<Role>, sqlx::Error> {
    sqlx::query_as::<_, Role>("SELECT * FROM roles WHERE id = $1")
        .bind(role_id)
        .fetch_optional(pool)
        .await
}

/// List a server's roles in position order.
pub async fn get_server_roles(pool: &PgPool, server_id: Uuid) -> Result<Vec<Role>, sqlx::Error> {
    sqlx::query_as::<_, Role>("SELECT * FROM roles WHERE server_id = $1 ORDER BY position")
        .bind(server_id)
        .fetch_all(pool)
        .await
}

/// Patch role name, color, permissions, and position; absent fields keep their values.
pub async fn update_role(
    pool: &PgPool,
    role_id: Uuid,
    name: Option<&str>,
    color: Option<i32>,
    permissions: Option<i64>,
    position: Option<i32>,
) -> Result<Role, sqlx::Error> {
    sqlx::query_as::<_, Role>(
        r#"UPDATE roles
           SET name = COALESCE($2, name),
               color = COALESCE($3, color),
               permissions = COALESCE($4, permissions),
               position = COALESCE($5, position)
           WHERE id = $1
           RETURNING *"#,
    )
    .bind(role_id)
    .bind(name)
    .bind(color)
    .bind(permissions)
    .bind(position)
    .fetch_one(pool)
    .await
}

/// Delete a role.
pub async fn delete_role(pool: &PgPool, role_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM roles WHERE id = $1")
        .bind(role_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Grant a role to a member; already-granted is a no-op.
pub async fn assign_role(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    role_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO member_roles (server_id, user_id, role_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(server_id)
    .bind(user_id)
    .bind(role_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// List the role ids granted to a member.
pub async fn get_member_role_ids(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT role_id FROM member_roles WHERE server_id = $1 AND user_id = $2")
            .bind(server_id)
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// Revoke one role from a member.
pub async fn remove_role_from_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    role_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM member_roles WHERE server_id = $1 AND user_id = $2 AND role_id = $3")
        .bind(server_id)
        .bind(user_id)
        .bind(role_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Get combined permissions for a user in a server (union of all role permissions)
pub async fn get_member_permissions(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<i64, sqlx::Error> {
    // Owner has all permissions
    let server = get_server_by_id(pool, server_id).await?;
    if let Some(s) = &server
        && s.owner_id == user_id
    {
        return Ok(i64::MAX); // all bits set
    }

    // Union of: default role permissions + all assigned role permissions
    let row: Option<(i64,)> = sqlx::query_as(
        r#"SELECT COALESCE(BIT_OR(r.permissions), 0)
           FROM roles r
           WHERE r.server_id = $1
             AND (r.is_default = true
                  OR r.id IN (
                      SELECT role_id FROM member_roles
                      WHERE server_id = $1 AND user_id = $2
                  ))"#,
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0).unwrap_or(0))
}

// ───── Invites ─────

/// Failures returned while atomically consuming invite capacity and membership.
#[derive(Debug, thiserror::Error)]
pub enum JoinInviteError {
    /// No invite exists for the supplied code.
    #[error("invite not found")]
    NotFound,
    /// The invite expired before its row lock was acquired.
    #[error("invite expired")]
    Expired,
    /// The invite has no remaining uses.
    #[error("invite exhausted")]
    Exhausted,
    /// PostgreSQL could not complete the membership transaction.
    #[error("invite database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Insert an invite code with optional use cap and expiry.
pub async fn create_invite(
    pool: &PgPool,
    server_id: Uuid,
    creator_id: Uuid,
    code: &str,
    max_uses: Option<i32>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Invite, sqlx::Error> {
    sqlx::query_as::<_, Invite>(
        r#"INSERT INTO invites (code, server_id, creator_id, max_uses, expires_at)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING *"#,
    )
    .bind(code)
    .bind(server_id)
    .bind(creator_id)
    .bind(max_uses)
    .bind(expires_at)
    .fetch_one(pool)
    .await
}

/// Fetch an invite by code.
pub async fn get_invite(pool: &PgPool, code: &str) -> Result<Option<Invite>, sqlx::Error> {
    sqlx::query_as::<_, Invite>("SELECT * FROM invites WHERE code = $1")
        .bind(code)
        .fetch_optional(pool)
        .await
}

/// Atomically validate an invite, add one member, and consume one use.
pub async fn join_server_via_invite(
    pool: &PgPool,
    code: &str,
    user_id: Uuid,
) -> Result<Server, JoinInviteError> {
    let mut transaction = pool.begin().await?;
    let invite = sqlx::query_as::<_, Invite>("SELECT * FROM invites WHERE code = $1 FOR UPDATE")
        .bind(code)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(JoinInviteError::NotFound)?;
    if invite
        .expires_at
        .is_some_and(|expires_at| expires_at <= Utc::now())
    {
        return Err(JoinInviteError::Expired);
    }
    if invite
        .max_uses
        .is_some_and(|max_uses| invite.uses >= max_uses)
    {
        return Err(JoinInviteError::Exhausted);
    }

    let already_member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM members WHERE server_id = $1 AND user_id = $2)",
    )
    .bind(invite.server_id)
    .bind(user_id)
    .fetch_one(&mut *transaction)
    .await?;
    if !already_member {
        sqlx::query("INSERT INTO members (server_id, user_id) VALUES ($1, $2)")
            .bind(invite.server_id)
            .bind(user_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("UPDATE invites SET uses = uses + 1 WHERE code = $1")
            .bind(code)
            .execute(&mut *transaction)
            .await?;
    }
    let server = sqlx::query_as::<_, Server>("SELECT * FROM servers WHERE id = $1")
        .bind(invite.server_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(JoinInviteError::NotFound)?;
    transaction.commit().await?;
    Ok(server)
}

/// List a server's invites oldest-first.
pub async fn get_server_invites(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Vec<Invite>, sqlx::Error> {
    sqlx::query_as::<_, Invite>("SELECT * FROM invites WHERE server_id = $1 ORDER BY created_at")
        .bind(server_id)
        .fetch_all(pool)
        .await
}

/// Delete an invite by code.
pub async fn delete_invite(pool: &PgPool, code: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM invites WHERE code = $1")
        .bind(code)
        .execute(pool)
        .await?;
    Ok(())
}

// ───── DM Channels ─────

/// Minimal DM channel row (id plus creation time).
#[derive(Debug, sqlx::FromRow)]
pub struct DmChannelRow {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Return the DM channel between two users, creating it on first contact.
pub async fn get_or_create_dm_channel(
    pool: &PgPool,
    user_a: Uuid,
    user_b: Uuid,
) -> Result<Uuid, sqlx::Error> {
    // Check if DM channel already exists between these two users
    let existing: Option<(Uuid,)> = sqlx::query_as(
        r#"SELECT dp1.dm_channel_id
           FROM dm_participants dp1
           INNER JOIN dm_participants dp2 ON dp1.dm_channel_id = dp2.dm_channel_id
           WHERE dp1.user_id = $1 AND dp2.user_id = $2"#,
    )
    .bind(user_a)
    .bind(user_b)
    .fetch_optional(pool)
    .await?;

    if let Some((id,)) = existing {
        return Ok(id);
    }

    // Create new DM channel
    let row: (Uuid,) = sqlx::query_as("INSERT INTO dm_channels DEFAULT VALUES RETURNING id")
        .fetch_one(pool)
        .await?;

    sqlx::query("INSERT INTO dm_participants (dm_channel_id, user_id) VALUES ($1, $2), ($1, $3)")
        .bind(row.0)
        .bind(user_a)
        .bind(user_b)
        .execute(pool)
        .await?;

    Ok(row.0)
}

/// List a user's DM channels with the counterpart's profile fields.
pub async fn get_user_dm_channels(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<
    Vec<(
        Uuid,
        Uuid,
        String,
        Option<String>,
        Option<String>,
        String,
        bool,
    )>,
    sqlx::Error,
> {
    // Returns: (dm_channel_id, other_user_id, username, display_name, avatar_url, status, is_agent)
    sqlx::query_as(
        r#"SELECT dp1.dm_channel_id, dp2.user_id, u.username, u.display_name, u.avatar_url,
                  u.status, u.is_agent
           FROM dm_participants dp1
           INNER JOIN dm_participants dp2 ON dp1.dm_channel_id = dp2.dm_channel_id AND dp2.user_id != dp1.user_id
           INNER JOIN users u ON dp2.user_id = u.id
           WHERE dp1.user_id = $1"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// DM message row joined with author profile fields.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct DmMessageWithAuthor {
    pub id: Uuid,
    pub dm_channel_id: Uuid,
    pub author_id: Uuid,
    pub content: String,
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub author_username: String,
    pub author_display_name: Option<String>,
    pub author_avatar_url: Option<String>,
}

/// Insert a DM message and return it joined with author info.
pub async fn create_dm_message(
    pool: &PgPool,
    dm_channel_id: Uuid,
    author_id: Uuid,
    content: &str,
) -> Result<DmMessageWithAuthor, sqlx::Error> {
    sqlx::query_as::<_, DmMessageWithAuthor>(
        r#"WITH new_msg AS (
               INSERT INTO dm_messages (dm_channel_id, author_id, content)
               VALUES ($1, $2, $3)
               RETURNING *
           )
           SELECT m.id, m.dm_channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                  u.username AS author_username,
                  u.display_name AS author_display_name,
                  u.avatar_url AS author_avatar_url
           FROM new_msg m
           INNER JOIN users u ON m.author_id = u.id"#,
    )
    .bind(dm_channel_id)
    .bind(author_id)
    .bind(content)
    .fetch_one(pool)
    .await
}

/// Page a DM channel's messages latest-first with an optional before cursor.
pub async fn get_dm_messages(
    pool: &PgPool,
    dm_channel_id: Uuid,
    limit: i64,
    before: Option<Uuid>,
) -> Result<Vec<DmMessageWithAuthor>, sqlx::Error> {
    let limit = limit.clamp(1, 100);
    if let Some(before_id) = before {
        sqlx::query_as::<_, DmMessageWithAuthor>(
            r#"SELECT m.id, m.dm_channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                      u.username AS author_username,
                      u.display_name AS author_display_name,
                      u.avatar_url AS author_avatar_url
               FROM dm_messages m
               INNER JOIN users u ON m.author_id = u.id
               WHERE m.dm_channel_id = $1
                 AND m.created_at < (SELECT created_at FROM dm_messages WHERE id = $2)
               ORDER BY m.created_at DESC
               LIMIT $3"#,
        )
        .bind(dm_channel_id)
        .bind(before_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, DmMessageWithAuthor>(
            r#"SELECT m.id, m.dm_channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                      u.username AS author_username,
                      u.display_name AS author_display_name,
                      u.avatar_url AS author_avatar_url
               FROM dm_messages m
               INNER JOIN users u ON m.author_id = u.id
               WHERE m.dm_channel_id = $1
               ORDER BY m.created_at DESC
               LIMIT $2"#,
        )
        .bind(dm_channel_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    }
}

/// True when the user participates in the DM channel.
pub async fn is_dm_participant(
    pool: &PgPool,
    dm_channel_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM dm_participants WHERE dm_channel_id = $1 AND user_id = $2",
    )
    .bind(dm_channel_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0 > 0).unwrap_or(false))
}

/// Set the paused state for one server's bridge.
pub async fn set_bridge_paused(
    pool: &PgPool,
    server_id: Uuid,
    paused: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO bridge_server_state (server_id, paused)
           VALUES ($1, $2)
           ON CONFLICT (server_id) DO UPDATE
           SET paused = EXCLUDED.paused, updated_at = NOW()"#,
    )
    .bind(server_id)
    .bind(paused)
    .execute(pool)
    .await?;
    Ok(())
}

/// Check whether one server's bridge is currently paused.
pub async fn is_bridge_paused(pool: &PgPool, server_id: Uuid) -> Result<bool, sqlx::Error> {
    let row: Option<(bool,)> =
        sqlx::query_as("SELECT paused FROM bridge_server_state WHERE server_id = $1")
            .bind(server_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0).unwrap_or(false))
}
