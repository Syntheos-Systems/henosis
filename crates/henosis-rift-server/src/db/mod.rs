use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::attachment::Attachment;
use crate::models::channel::Channel;
use crate::models::message::{MessageQuery, MessageWithAuthor};
use crate::models::role::Role;
use crate::models::server::{Invite, Member, Server};
use crate::models::user::User;

// ───── Users ─────

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

pub async fn get_user_by_id(pool: &PgPool, id: Uuid) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn get_user_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = $1")
        .bind(username)
        .fetch_optional(pool)
        .await
}

pub async fn get_user_by_email(pool: &PgPool, email: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1")
        .bind(email)
        .fetch_optional(pool)
        .await
}

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

pub async fn update_user_password(
    pool: &PgPool,
    user_id: Uuid,
    password_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = NOW() WHERE id = $1")
        .bind(user_id)
        .bind(password_hash)
        .execute(pool)
        .await?;
    Ok(())
}

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

pub async fn store_refresh_token(
    pool: &PgPool,
    user_id: Uuid,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)",
    )
    .bind(user_id)
    .bind(token_hash)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

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

pub async fn delete_refresh_token(pool: &PgPool, token_hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM refresh_tokens WHERE token_hash = $1")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_user_refresh_tokens(pool: &PgPool, user_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM refresh_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ───── Servers ─────

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

pub async fn get_server_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Server>, sqlx::Error> {
    sqlx::query_as::<_, Server>("SELECT * FROM servers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

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

pub async fn delete_server(pool: &PgPool, server_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM servers WHERE id = $1")
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(())
}

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

pub async fn add_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<Member, sqlx::Error> {
    sqlx::query_as::<_, Member>(
        r#"INSERT INTO members (server_id, user_id)
           VALUES ($1, $2)
           ON CONFLICT DO NOTHING
           RETURNING *"#,
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
}

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

pub async fn get_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<Option<Member>, sqlx::Error> {
    sqlx::query_as::<_, Member>(
        "SELECT * FROM members WHERE server_id = $1 AND user_id = $2",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

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

pub async fn is_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM members WHERE server_id = $1 AND user_id = $2",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0 > 0).unwrap_or(false))
}

// ───── Channels ─────

pub async fn create_channel(
    pool: &PgPool,
    server_id: Uuid,
    name: &str,
    topic: Option<&str>,
    channel_type: &str,
) -> Result<Channel, sqlx::Error> {
    // Get next position
    let pos: Option<(i32,)> = sqlx::query_as(
        "SELECT COALESCE(MAX(position), -1) FROM channels WHERE server_id = $1",
    )
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

pub async fn get_channel_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Channel>, sqlx::Error> {
    sqlx::query_as::<_, Channel>("SELECT * FROM channels WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn get_server_channels(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Vec<Channel>, sqlx::Error> {
    sqlx::query_as::<_, Channel>(
        "SELECT * FROM channels WHERE server_id = $1 ORDER BY position",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await
}

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

pub async fn delete_channel(pool: &PgPool, channel_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM channels WHERE id = $1")
        .bind(channel_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ───── Messages ─────

pub async fn create_message(
    pool: &PgPool,
    channel_id: Uuid,
    author_id: Uuid,
    content: &str,
) -> Result<MessageWithAuthor, sqlx::Error> {
    sqlx::query_as::<_, MessageWithAuthor>(
        r#"WITH new_msg AS (
               INSERT INTO messages (channel_id, author_id, content)
               VALUES ($1, $2, $3)
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
    .fetch_one(pool)
    .await
}

pub async fn get_messages(
    pool: &PgPool,
    channel_id: Uuid,
    query: &MessageQuery,
) -> Result<Vec<MessageWithAuthor>, sqlx::Error> {
    let limit = query.limit.unwrap_or(50).min(100);

    if let Some(before) = query.before {
        sqlx::query_as::<_, MessageWithAuthor>(
            r#"SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                      m.message_type,
                      u.username AS author_username,
                      u.display_name AS author_display_name,
                      u.avatar_url AS author_avatar_url
               FROM messages m
               INNER JOIN users u ON m.author_id = u.id
               WHERE m.channel_id = $1
                 AND m.created_at < (SELECT created_at FROM messages WHERE id = $2)
               ORDER BY m.created_at DESC
               LIMIT $3"#,
        )
        .bind(channel_id)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else if let Some(after) = query.after {
        sqlx::query_as::<_, MessageWithAuthor>(
            r#"SELECT m.id, m.channel_id, m.author_id, m.content, m.edited_at, m.created_at,
                      m.message_type,
                      u.username AS author_username,
                      u.display_name AS author_display_name,
                      u.avatar_url AS author_avatar_url
               FROM messages m
               INNER JOIN users u ON m.author_id = u.id
               WHERE m.channel_id = $1
                 AND m.created_at > (SELECT created_at FROM messages WHERE id = $2)
               ORDER BY m.created_at ASC
               LIMIT $3"#,
        )
        .bind(channel_id)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
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
               ORDER BY m.created_at DESC
               LIMIT $2"#,
        )
        .bind(channel_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    }
}

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

pub async fn update_message(
    pool: &PgPool,
    message_id: Uuid,
    content: &str,
) -> Result<MessageWithAuthor, sqlx::Error> {
    sqlx::query_as::<_, MessageWithAuthor>(
        r#"WITH updated AS (
               UPDATE messages SET content = $2, edited_at = NOW()
               WHERE id = $1
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
    .fetch_one(pool)
    .await
}

pub async fn delete_message(pool: &PgPool, message_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(message_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ───── Roles ─────

// ───── Attachments ─────

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

pub async fn get_attachments_for_messages(
    pool: &PgPool,
    message_ids: &[Uuid],
) -> Result<Vec<Attachment>, sqlx::Error> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, Attachment>(
        "SELECT * FROM attachments WHERE message_id = ANY($1) ORDER BY created_at",
    )
    .bind(message_ids)
    .fetch_all(pool)
    .await
}

// ───── Roles (continued) ─────

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

pub async fn create_default_role(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Role, sqlx::Error> {
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

pub async fn get_role_by_id(pool: &PgPool, role_id: Uuid) -> Result<Option<Role>, sqlx::Error> {
    sqlx::query_as::<_, Role>("SELECT * FROM roles WHERE id = $1")
        .bind(role_id)
        .fetch_optional(pool)
        .await
}

pub async fn get_server_roles(pool: &PgPool, server_id: Uuid) -> Result<Vec<Role>, sqlx::Error> {
    sqlx::query_as::<_, Role>(
        "SELECT * FROM roles WHERE server_id = $1 ORDER BY position",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await
}

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

pub async fn delete_role(pool: &PgPool, role_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM roles WHERE id = $1")
        .bind(role_id)
        .execute(pool)
        .await?;
    Ok(())
}

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

pub async fn get_member_role_ids(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT role_id FROM member_roles WHERE server_id = $1 AND user_id = $2",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

pub async fn remove_role_from_member(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    role_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM member_roles WHERE server_id = $1 AND user_id = $2 AND role_id = $3",
    )
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
    if let Some(s) = &server {
        if s.owner_id == user_id {
            return Ok(i64::MAX); // all bits set
        }
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

pub async fn get_invite(pool: &PgPool, code: &str) -> Result<Option<Invite>, sqlx::Error> {
    sqlx::query_as::<_, Invite>("SELECT * FROM invites WHERE code = $1")
        .bind(code)
        .fetch_optional(pool)
        .await
}

pub async fn use_invite(pool: &PgPool, code: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE invites SET uses = uses + 1 WHERE code = $1")
        .bind(code)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_server_invites(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<Vec<Invite>, sqlx::Error> {
    sqlx::query_as::<_, Invite>("SELECT * FROM invites WHERE server_id = $1 ORDER BY created_at")
        .bind(server_id)
        .fetch_all(pool)
        .await
}

pub async fn delete_invite(pool: &PgPool, code: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM invites WHERE code = $1")
        .bind(code)
        .execute(pool)
        .await?;
    Ok(())
}

// ───── DM Channels ─────

#[derive(Debug, sqlx::FromRow)]
pub struct DmChannelRow {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
}

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
    let row: (Uuid,) =
        sqlx::query_as("INSERT INTO dm_channels DEFAULT VALUES RETURNING id")
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

pub async fn get_user_dm_channels(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<(Uuid, Uuid, String, Option<String>, Option<String>)>, sqlx::Error> {
    // Returns: (dm_channel_id, other_user_id, username, display_name, avatar_url)
    sqlx::query_as(
        r#"SELECT dp1.dm_channel_id, dp2.user_id, u.username, u.display_name, u.avatar_url
           FROM dm_participants dp1
           INNER JOIN dm_participants dp2 ON dp1.dm_channel_id = dp2.dm_channel_id AND dp2.user_id != dp1.user_id
           INNER JOIN users u ON dp2.user_id = u.id
           WHERE dp1.user_id = $1"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

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

pub async fn get_dm_messages(
    pool: &PgPool,
    dm_channel_id: Uuid,
    limit: i64,
    before: Option<Uuid>,
) -> Result<Vec<DmMessageWithAuthor>, sqlx::Error> {
    let limit = limit.min(100);
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

/// Set the bridge paused state in the bridge_state table.
pub async fn set_bridge_paused(pool: &PgPool, paused: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE bridge_state SET value = $1, updated_at = NOW() WHERE key = 'paused'"
    )
    .bind(paused.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Check if the bridge is currently paused.
pub async fn is_bridge_paused(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT value FROM bridge_state WHERE key = 'paused'"
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0 == "true").unwrap_or(false))
}
