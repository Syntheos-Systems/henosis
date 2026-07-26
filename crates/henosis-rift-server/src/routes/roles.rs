use axum::{
    Json,
    extract::{Path, State},
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::db;
use crate::error::AppError;
use crate::models::permissions::perms;
use crate::models::role::{CreateRoleRequest, Role, UpdateRoleRequest};
use crate::ws::gateway::{Gateway, GatewayEvent};

/// Requires the user to belong to the specified server.
async fn require_member(pool: &PgPool, server_id: Uuid, user_id: Uuid) -> Result<(), AppError> {
    if !db::is_member(pool, server_id, user_id).await? {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Requires server membership and the specified role-management permission.
///
/// Returns the caller's effective permission mask so callers can apply
/// [`ensure_grantable`] without issuing a second query for the same row.
async fn require_permission(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
    permission: i64,
) -> Result<i64, AppError> {
    require_member(pool, server_id, user_id).await?;
    let user_perms = db::get_member_permissions(pool, server_id, user_id).await?;
    if !perms::has(user_perms, permission) {
        return Err(AppError::Forbidden);
    }
    Ok(user_perms)
}

/// Reject any permission bit the acting member does not already hold.
///
/// `MANAGE_ROLES` authorizes editing the role graph, not minting authority the
/// editor lacks. Without this check a member granted only `MANAGE_ROLES` could
/// write `ADMINISTRATOR` into any role -- including `@everyone`, which every
/// member inherits -- and take over the server, because [`perms::has`] treats
/// `ADMINISTRATOR` as an all-bits bypass.
///
/// A member who already holds `ADMINISTRATOR` implicitly holds every bit and may
/// grant any of them. Everyone else is confined to their own effective mask.
fn ensure_grantable(actor_permissions: i64, requested: i64) -> Result<(), AppError> {
    if actor_permissions & perms::ADMINISTRATOR != 0 {
        return Ok(());
    }
    if requested & !actor_permissions != 0 {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Verify a role exists and belongs to the given server
async fn get_role_in_server(
    pool: &PgPool,
    server_id: Uuid,
    role_id: Uuid,
) -> Result<Role, AppError> {
    let role = db::get_role_by_id(pool, role_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Role not found".into()))?;
    if role.server_id != server_id {
        return Err(AppError::NotFound("Role not found".into()));
    }
    Ok(role)
}

/// GET /api/servers/{server_id}/roles
pub async fn list_roles(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<Vec<Role>>, AppError> {
    require_member(&pool, server_id, auth.user_id).await?;
    let roles = db::get_server_roles(&pool, server_id).await?;
    Ok(Json(roles))
}

/// POST /api/servers/{server_id}/roles
pub async fn create_role(
    State(pool): State<PgPool>,
    State(gateway): State<Gateway>,
    auth: AuthUser,
    Path(server_id): Path<Uuid>,
    Json(req): Json<CreateRoleRequest>,
) -> Result<Json<Role>, AppError> {
    let actor = require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    let name = req.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::BadRequest(
            "Role name must be 1-100 characters".into(),
        ));
    }

    let color = req.color.unwrap_or(0);
    let permissions = req.permissions.unwrap_or(perms::DEFAULT);
    ensure_grantable(actor, permissions)?;

    let role = db::create_role(&pool, server_id, name, color, permissions).await?;

    gateway.broadcast_to_server(
        server_id,
        GatewayEvent::RoleCreate {
            server_id,
            role: role.clone(),
        },
    );

    Ok(Json(role))
}

/// PATCH /api/servers/{server_id}/roles/{role_id}
pub async fn update_role(
    State(pool): State<PgPool>,
    State(gateway): State<Gateway>,
    auth: AuthUser,
    Path((server_id, role_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateRoleRequest>,
) -> Result<Json<Role>, AppError> {
    let actor = require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    // Verify role belongs to this server
    let existing = get_role_in_server(&pool, server_id, role_id).await?;

    // Don't allow renaming the @everyone role
    if existing.is_default && req.name.is_some() {
        return Err(AppError::BadRequest(
            "Cannot rename the @everyone role".into(),
        ));
    }

    // Editing a role the actor could not have created is itself an escalation
    // path, so the actor must dominate both the role's current authority and any
    // authority being written into it.
    ensure_grantable(actor, existing.permissions)?;
    if let Some(requested) = req.permissions {
        ensure_grantable(actor, requested)?;
    }

    let role = db::update_role(
        &pool,
        role_id,
        req.name.as_deref(),
        req.color,
        req.permissions,
        req.position,
    )
    .await?;

    gateway.broadcast_to_server(
        server_id,
        GatewayEvent::RoleUpdate {
            server_id,
            role: role.clone(),
        },
    );

    Ok(Json(role))
}

/// DELETE /api/servers/{server_id}/roles/{role_id}
pub async fn delete_role(
    State(pool): State<PgPool>,
    State(gateway): State<Gateway>,
    auth: AuthUser,
    Path((server_id, role_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let actor = require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    let role = get_role_in_server(&pool, server_id, role_id).await?;

    if role.is_default {
        return Err(AppError::BadRequest(
            "Cannot delete the @everyone role".into(),
        ));
    }

    // Deleting a more privileged role is a sabotage vector, not an edit.
    ensure_grantable(actor, role.permissions)?;

    db::delete_role(&pool, role_id).await?;

    gateway.broadcast_to_server(server_id, GatewayEvent::RoleDelete { server_id, role_id });

    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// GET /api/servers/{server_id}/members/{user_id}/roles
pub async fn get_member_roles(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path((server_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<Uuid>>, AppError> {
    require_member(&pool, server_id, auth.user_id).await?;
    require_member(&pool, server_id, user_id).await?;
    let role_ids = db::get_member_role_ids(&pool, server_id, user_id).await?;
    Ok(Json(role_ids))
}

/// PUT /api/servers/{server_id}/members/{user_id}/roles/{role_id}
pub async fn assign_role(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path((server_id, user_id, role_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let actor = require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    // Verify role belongs to this server
    let role = get_role_in_server(&pool, server_id, role_id).await?;

    if role.is_default {
        return Err(AppError::BadRequest(
            "Cannot manually assign the @everyone role".into(),
        ));
    }

    // Handing out authority the actor lacks escalates just as surely as writing
    // it into a role, and `user_id` may be the actor's own.
    ensure_grantable(actor, role.permissions)?;

    // Verify target user is a member
    require_member(&pool, server_id, user_id).await?;

    db::assign_role(&pool, server_id, user_id, role_id).await?;

    Ok(Json(serde_json::json!({ "assigned": true })))
}

/// DELETE /api/servers/{server_id}/members/{user_id}/roles/{role_id}
pub async fn remove_role(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path((server_id, user_id, role_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let actor = require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    // Verify role belongs to this server
    let role = get_role_in_server(&pool, server_id, role_id).await?;

    if role.is_default {
        return Err(AppError::BadRequest(
            "Cannot remove the @everyone role from a member".into(),
        ));
    }

    // Stripping a role the actor does not dominate would let a lesser member
    // demote an administrator.
    ensure_grantable(actor, role.permissions)?;

    db::remove_role_from_member(&pool, server_id, user_id, role_id).await?;

    Ok(Json(serde_json::json!({ "removed": true })))
}

/// Covers the permission-dominance predicate that gates every role mutation.
#[cfg(test)]
mod tests {
    use super::*;

    /// The reported escalation: MANAGE_ROLES alone cannot write ADMINISTRATOR.
    #[test]
    fn manage_roles_cannot_grant_administrator() {
        assert!(ensure_grantable(perms::MANAGE_ROLES, perms::ADMINISTRATOR).is_err());
    }

    /// The same actor cannot launder ADMINISTRATOR through the @everyone role.
    #[test]
    fn manage_roles_cannot_grant_administrator_alongside_defaults() {
        let requested = perms::DEFAULT | perms::ADMINISTRATOR;
        assert!(ensure_grantable(perms::MANAGE_ROLES | perms::DEFAULT, requested).is_err());
    }

    /// An administrator implicitly holds every bit and may grant any of them.
    #[test]
    fn administrator_may_grant_anything() {
        assert!(ensure_grantable(perms::ADMINISTRATOR, perms::ADMINISTRATOR).is_ok());
        assert!(ensure_grantable(perms::ADMINISTRATOR, perms::BAN_MEMBERS).is_ok());
    }

    /// Granting bits the actor already holds stays allowed.
    #[test]
    fn actor_may_grant_bits_it_already_holds() {
        let actor = perms::MANAGE_ROLES | perms::KICK_MEMBERS;
        assert!(ensure_grantable(actor, perms::KICK_MEMBERS).is_ok());
        assert!(ensure_grantable(actor, actor).is_ok());
    }

    /// A single bit outside the actor's mask is enough to reject the whole write.
    #[test]
    fn one_unheld_bit_rejects_the_request() {
        let actor = perms::MANAGE_ROLES | perms::KICK_MEMBERS;
        assert!(ensure_grantable(actor, perms::KICK_MEMBERS | perms::BAN_MEMBERS).is_err());
    }

    /// Requesting nothing is always permitted, so an unrelated PATCH still works.
    #[test]
    fn empty_request_is_permitted() {
        assert!(ensure_grantable(perms::MANAGE_ROLES, 0).is_ok());
    }
}
