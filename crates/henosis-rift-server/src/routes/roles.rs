use axum::{
    extract::{Path, State},
    Json,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::db;
use crate::error::AppError;
use crate::models::permissions::perms;
use crate::models::role::{CreateRoleRequest, Role, UpdateRoleRequest};
use crate::ws::gateway::{Gateway, GatewayEvent};

async fn require_member(pool: &PgPool, server_id: Uuid, user_id: Uuid) -> Result<(), AppError> {
    if !db::is_member(pool, server_id, user_id).await? {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

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
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    let name = req.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::BadRequest("Role name must be 1-100 characters".into()));
    }

    let color = req.color.unwrap_or(0);
    let permissions = req.permissions.unwrap_or(perms::DEFAULT);

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
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    // Verify role belongs to this server
    let existing = get_role_in_server(&pool, server_id, role_id).await?;

    // Don't allow renaming the @everyone role
    if existing.is_default && req.name.is_some() {
        return Err(AppError::BadRequest("Cannot rename the @everyone role".into()));
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
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    let role = get_role_in_server(&pool, server_id, role_id).await?;

    if role.is_default {
        return Err(AppError::BadRequest("Cannot delete the @everyone role".into()));
    }

    db::delete_role(&pool, role_id).await?;

    gateway.broadcast_to_server(
        server_id,
        GatewayEvent::RoleDelete {
            server_id,
            role_id,
        },
    );

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
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    // Verify role belongs to this server
    let role = get_role_in_server(&pool, server_id, role_id).await?;

    if role.is_default {
        return Err(AppError::BadRequest("Cannot manually assign the @everyone role".into()));
    }

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
    require_permission(&pool, server_id, auth.user_id, perms::MANAGE_ROLES).await?;

    // Verify role belongs to this server
    let role = get_role_in_server(&pool, server_id, role_id).await?;

    if role.is_default {
        return Err(AppError::BadRequest("Cannot remove the @everyone role from a member".into()));
    }

    db::remove_role_from_member(&pool, server_id, user_id, role_id).await?;

    Ok(Json(serde_json::json!({ "removed": true })))
}
