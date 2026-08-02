use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::db;
use crate::error::AppError;
use crate::models::channel::Channel;
use crate::models::permissions::perms;
use crate::models::role::Role;
use crate::models::server::{
    CreateInviteRequest, CreateServerRequest, Invite, Server, UpdateServerRequest,
};
use crate::models::user::PublicUser;

/// Server response enriched with its visible channels and roles.
#[derive(Serialize)]
pub struct ServerWithChannels {
    #[serde(flatten)]
    pub server: Server,
    pub channels: Vec<Channel>,
    pub roles: Vec<Role>,
}

/// Public member details returned by a server membership listing.
#[derive(Serialize)]
pub struct MemberInfo {
    pub user: PublicUser,
    pub nickname: Option<String>,
    pub joined_at: chrono::DateTime<chrono::Utc>,
}

/// Effective room capabilities exposed to the authenticated human client.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoomPermissions {
    /// Whether the caller may create room messages.
    pub send_messages: bool,
    /// Whether the caller may both send messages and attach files to them.
    pub attach_files: bool,
    /// Whether the caller may delete messages authored by other members.
    pub manage_messages: bool,
    /// Whether the caller may manage room-wide settings.
    pub manage_server: bool,
}

/// Converts Rift's authoritative permission mask into the narrow public contract.
impl RoomPermissions {
    /// Derive every exposed capability with administrator semantics preserved.
    fn from_mask(mask: i64) -> Self {
        let send_messages = perms::has(mask, perms::SEND_MESSAGES);
        Self {
            send_messages,
            attach_files: send_messages && perms::has(mask, perms::ATTACH_FILES),
            manage_messages: perms::has(mask, perms::MANAGE_MESSAGES),
            manage_server: perms::has(mask, perms::MANAGE_SERVER),
        }
    }
}

/// POST /api/servers
pub async fn create_server(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Json(req): Json<CreateServerRequest>,
) -> Result<Json<ServerWithChannels>, AppError> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::BadRequest(
            "Server name must be 1-100 characters".into(),
        ));
    }

    let server = db::create_server(&pool, name, req.description.as_deref(), auth.user_id).await?;

    // Add owner as member
    db::add_member(&pool, server.id, auth.user_id).await?;

    // Create default @everyone role
    let default_role = db::create_default_role(&pool, server.id).await?;

    // Create default #general channel
    let channel = db::create_channel(&pool, server.id, "general", None, "text").await?;

    Ok(Json(ServerWithChannels {
        server,
        channels: vec![channel],
        roles: vec![default_role],
    }))
}

/// GET /api/servers
pub async fn list_servers(
    State(pool): State<PgPool>,
    auth: AuthUser,
) -> Result<Json<Vec<Server>>, AppError> {
    let servers = db::get_user_servers(&pool, auth.user_id).await?;
    Ok(Json(servers))
}

/// GET /api/servers/:server_id
pub async fn get_server(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<ServerWithChannels>, AppError> {
    require_member(&pool, server_id, auth.user_id).await?;

    let server = db::get_server_by_id(&pool, server_id)
        .await?
        .ok_or(AppError::NotFound("Server not found".into()))?;

    let channels = db::get_server_channels(&pool, server_id).await?;
    let roles = db::get_server_roles(&pool, server_id).await?;

    Ok(Json(ServerWithChannels {
        server,
        channels,
        roles,
    }))
}

/// GET /api/servers/:server_id/permissions/@me
pub async fn current_user_permissions(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<RoomPermissions>, AppError> {
    require_member(&pool, server_id, auth.user_id).await?;
    let mask = db::get_member_permissions(&pool, server_id, auth.user_id).await?;
    Ok(Json(RoomPermissions::from_mask(mask)))
}

/// PATCH /api/servers/:server_id
pub async fn update_server(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<UpdateServerRequest>,
) -> Result<Json<Server>, AppError> {
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_SERVER).await?;

    let server = db::update_server(
        &pool,
        server_id,
        req.name.as_deref(),
        req.description.as_deref(),
    )
    .await?;

    Ok(Json(server))
}

/// DELETE /api/servers/:server_id
pub async fn delete_server(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let server = db::get_server_by_id(&pool, server_id)
        .await?
        .ok_or(AppError::NotFound("Server not found".into()))?;

    if server.owner_id != auth.user_id {
        return Err(AppError::Forbidden);
    }

    db::delete_server(&pool, server_id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/servers/:server_id/members
pub async fn list_members(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<MemberInfo>>, AppError> {
    require_member(&pool, server_id, auth.user_id).await?;

    let members = db::get_server_members(&pool, server_id).await?;
    let result: Vec<MemberInfo> = members
        .into_iter()
        .map(|(m, u)| MemberInfo {
            user: PublicUser::from(u),
            nickname: m.nickname,
            joined_at: m.joined_at,
        })
        .collect();

    Ok(Json(result))
}

/// DELETE /api/servers/:server_id/members/:user_id (kick)
pub async fn remove_member(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path((server_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    if user_id == auth.user_id {
        // Leave server
        db::remove_member(&pool, server_id, user_id).await?;
        return Ok(Json(serde_json::json!({ "ok": true })));
    }

    require_permission(&pool, server_id, auth.user_id, perms::KICK_MEMBERS).await?;

    db::remove_member(&pool, server_id, user_id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ─── Invites ───

/// POST /api/servers/:server_id/invites
pub async fn create_invite(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<CreateInviteRequest>,
) -> Result<Json<Invite>, AppError> {
    require_permission(&pool, server_id, auth.user_id, perms::CREATE_INVITES).await?;

    let code = generate_invite_code();
    let expires_at = req
        .expires_in_hours
        .map(|h| chrono::Utc::now() + chrono::Duration::hours(h));

    let invite = db::create_invite(
        &pool,
        server_id,
        auth.user_id,
        &code,
        req.max_uses,
        expires_at,
    )
    .await?;

    Ok(Json(invite))
}

/// POST /api/invites/:code/join
pub async fn join_via_invite(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(code): Path<String>,
) -> Result<Json<Server>, AppError> {
    let invite = db::get_invite(&pool, &code)
        .await?
        .ok_or(AppError::NotFound("Invite not found".into()))?;

    // Check expiry
    if let Some(expires) = invite.expires_at
        && expires < chrono::Utc::now()
    {
        return Err(AppError::BadRequest("Invite expired".into()));
    }

    // Check max uses
    if let Some(max) = invite.max_uses
        && invite.uses >= max
    {
        return Err(AppError::BadRequest("Invite has reached max uses".into()));
    }

    // Check if already a member
    if db::is_member(&pool, invite.server_id, auth.user_id).await? {
        let server = db::get_server_by_id(&pool, invite.server_id)
            .await?
            .ok_or(AppError::NotFound("Server not found".into()))?;
        return Ok(Json(server));
    }

    db::add_member(&pool, invite.server_id, auth.user_id).await?;
    db::use_invite(&pool, &code).await?;

    let server = db::get_server_by_id(&pool, invite.server_id)
        .await?
        .ok_or(AppError::NotFound("Server not found".into()))?;

    Ok(Json(server))
}

/// GET /api/servers/:server_id/invites
pub async fn list_invites(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<Invite>>, AppError> {
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_INVITES).await?;

    let invites = db::get_server_invites(&pool, server_id).await?;
    Ok(Json(invites))
}

/// DELETE /api/servers/:server_id/invites/:code
pub async fn delete_invite(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path((server_id, code)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_INVITES).await?;

    let invite = db::get_invite(&pool, &code)
        .await?
        .ok_or(AppError::NotFound("Invite not found".into()))?;
    require_invite_server(invite.server_id, server_id)?;

    db::delete_invite(&pool, &code).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ─── Helpers ───

/// Reject callers that are not members of the requested server.
async fn require_member(pool: &PgPool, server_id: Uuid, user_id: Uuid) -> Result<(), AppError> {
    require_membership(db::is_member(pool, server_id, user_id).await?)
}

/// Convert server-truth membership into the shared forbidden boundary.
fn require_membership(is_member: bool) -> Result<(), AppError> {
    if is_member {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// Reject members that lack the requested permission in the server.
async fn require_permission(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    permission: i64,
) -> Result<(), AppError> {
    require_member(pool, server_id, user_id).await?;
    let user_perms = db::get_member_permissions(pool, server_id, user_id).await?;
    if !perms::has(user_perms, permission) {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Reject an invite code that does not belong to the server in the route path.
fn require_invite_server(invite_server_id: Uuid, path_server_id: Uuid) -> Result<(), AppError> {
    if invite_server_id != path_server_id {
        return Err(AppError::NotFound("Invite not found".into()));
    }
    Ok(())
}

/// Generate a random URL-safe invite code.
fn generate_invite_code() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

#[cfg(test)]
/// Exercises parent-server binding for nested invite mutation routes.
mod tests {
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    use super::{
        RoomPermissions, current_user_permissions, require_invite_server, require_membership,
    };
    use crate::auth::middleware::AuthUser;
    use crate::db;
    use crate::models::permissions::perms;
    use uuid::Uuid;

    /// Connect to the opt-in PostgreSQL test database without exposing its URL.
    async fn live_test_pool() -> Option<sqlx::PgPool> {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping live room permission test: HENOSIS_RIFT_TEST_DATABASE_URL is unset"
            );
            return None;
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations must apply");
        Some(pool)
    }

    /// The current-user response contains only the four camel-case booleans.
    #[test]
    fn current_user_permissions_serialize_as_boolean_capabilities() {
        let response = RoomPermissions::from_mask(perms::SEND_MESSAGES | perms::MANAGE_MESSAGES);
        assert_eq!(
            serde_json::to_value(response).expect("permissions must serialize"),
            json!({
                "sendMessages": true,
                "attachFiles": false,
                "manageMessages": true,
                "manageServer": false,
            })
        );
    }

    /// Administrator authority grants every capability represented publicly.
    #[test]
    fn current_user_permissions_preserve_administrator_semantics() {
        assert_eq!(
            RoomPermissions::from_mask(perms::ADMINISTRATOR),
            RoomPermissions {
                send_messages: true,
                attach_files: true,
                manage_messages: true,
                manage_server: true,
            }
        );
    }

    /// Attachment affordance stays disabled when the caller cannot send.
    #[test]
    fn current_user_permissions_require_send_for_attachments() {
        let permissions = RoomPermissions::from_mask(perms::ATTACH_FILES);
        assert!(!permissions.send_messages);
        assert!(!permissions.attach_files);
    }

    /// Non-members are forbidden before effective permissions are exposed.
    #[test]
    fn current_user_permissions_forbid_non_members() {
        assert!(require_membership(true).is_ok());
        let response = require_membership(false)
            .expect_err("non-member must be forbidden")
            .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    }

    /// Live PostgreSQL proves the handler uses membership and authoritative roles.
    #[tokio::test]
    async fn current_user_permissions_enforce_live_membership() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let owner = db::create_user(
            &pool,
            &format!("owner_{suffix}"),
            &format!("owner-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("test owner must be created");
        let member = db::create_user(
            &pool,
            &format!("member_{suffix}"),
            &format!("member-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("test member must be created");
        let outsider = db::create_user(
            &pool,
            &format!("outside_{suffix}"),
            &format!("outside-{suffix}@example.invalid"),
            "test-hash",
            None,
        )
        .await
        .expect("test outsider must be created");
        let server = db::create_server(&pool, &format!("permissions-{suffix}"), None, owner.id)
            .await
            .expect("test server must be created");
        db::add_member(&pool, server.id, owner.id)
            .await
            .expect("test owner membership must be created");
        db::add_member(&pool, server.id, member.id)
            .await
            .expect("test member membership must be created");
        db::create_default_role(&pool, server.id)
            .await
            .expect("default role must be created");

        let response = current_user_permissions(
            State(pool.clone()),
            AuthUser {
                user_id: member.id,
                username: member.username,
            },
            Path(server.id),
        )
        .await
        .expect("member permissions must load");
        assert_eq!(
            response.0,
            RoomPermissions {
                send_messages: true,
                attach_files: true,
                manage_messages: false,
                manage_server: false,
            }
        );

        let error = current_user_permissions(
            State(pool),
            AuthUser {
                user_id: outsider.id,
                username: outsider.username,
            },
            Path(server.id),
        )
        .await
        .expect_err("non-member must not receive permission details");
        assert!(matches!(error, crate::error::AppError::Forbidden));
    }

    /// An invite is accepted only under its authoritative server identifier.
    #[test]
    fn invite_parent_must_match_route_server() {
        let server_id = Uuid::new_v4();
        assert!(require_invite_server(server_id, server_id).is_ok());
        assert!(require_invite_server(server_id, Uuid::new_v4()).is_err());
    }
}
