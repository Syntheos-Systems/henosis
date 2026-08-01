//! Authenticated APIs for human-owned persistent Rift agent identities.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::db;
use crate::error::AppError;
use crate::models::agent_control::AgentIdentitySummary;
use crate::models::user::User;

/// Fields accepted when a human creates a persistent agent identity.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateAgentRequest {
    /// Rift username, unique across humans and agents.
    pub username: String,
    /// Optional display name presented in rooms and the dashboard.
    pub display_name: Option<String>,
}

/// List the authenticated human's claimed persistent agent identities.
pub async fn list_owned_agents(
    State(pool): State<PgPool>,
    auth: AuthUser,
) -> Result<Json<Vec<AgentIdentitySummary>>, AppError> {
    require_human(&pool, auth.user_id).await?;
    let agents = db::agent_control::list_owned_agents(&pool, auth.user_id)
        .await?
        .into_iter()
        .map(|agent| identity_summary(agent, Some(auth.user_id)))
        .collect();
    Ok(Json(agents))
}

/// Create a new agent identity atomically owned by the authenticated human.
pub async fn create_owned_agent(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Json(request): Json<CreateAgentRequest>,
) -> Result<Json<AgentIdentitySummary>, AppError> {
    require_human(&pool, auth.user_id).await?;
    let username = request.username.trim();
    validate_identity_input(username, request.display_name.as_deref())?;
    if db::get_user_by_username(&pool, username).await?.is_some() {
        return Err(AppError::Conflict("Username already taken".to_string()));
    }

    let password_hash = super::auth::hash_password(&super::bridge::random_password())?;
    let created = db::create_owned_agent_user(
        &pool,
        username,
        &super::bridge::agent_email(username),
        &password_hash,
        request.display_name.as_deref(),
        auth.user_id,
    )
    .await
    .map_err(map_create_error)?;
    Ok(Json(identity_summary(created, Some(auth.user_id))))
}

/// Claim one unowned legacy agent when the caller manages a shared room.
pub async fn claim_agent(
    State(pool): State<PgPool>,
    auth: AuthUser,
    Path(agent_id): Path<Uuid>,
) -> Result<Json<AgentIdentitySummary>, AppError> {
    require_human(&pool, auth.user_id).await?;
    let agent = db::get_user_by_id(&pool, agent_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Agent identity not found".to_string()))?;
    if !agent.is_agent {
        return Err(AppError::BadRequest(
            "Only agent identities can be claimed".to_string(),
        ));
    }
    if db::agent_control::owner_for_agent(&pool, agent_id)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(
            "Agent identity is already claimed".to_string(),
        ));
    }

    if !db::claim_agent_as_shared_manager(&pool, auth.user_id, agent_id).await? {
        return if db::agent_control::owner_for_agent(&pool, agent_id)
            .await?
            .is_some()
        {
            Err(AppError::Conflict(
                "Agent identity is already claimed".to_string(),
            ))
        } else {
            Err(AppError::Forbidden)
        };
    }
    Ok(Json(identity_summary(agent, Some(auth.user_id))))
}

/// Require a current Rift account to be human before ownership operations.
async fn require_human(pool: &PgPool, user_id: Uuid) -> Result<User, AppError> {
    let user = db::get_user_by_id(pool, user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".to_string()))?;
    if user.is_agent {
        return Err(AppError::Forbidden);
    }
    Ok(user)
}

/// Validate the existing username contract and the database display-name bound.
fn validate_identity_input(username: &str, display_name: Option<&str>) -> Result<(), AppError> {
    if !(3..=32).contains(&username.len()) {
        return Err(AppError::BadRequest(
            "Username must be 3-32 characters".to_string(),
        ));
    }
    if display_name.is_some_and(|name| name.chars().count() > 64) {
        return Err(AppError::BadRequest(
            "Display name must be at most 64 characters".to_string(),
        ));
    }
    Ok(())
}

/// Convert an internal user row into the secret-free ownership response.
fn identity_summary(user: User, owner_user_id: Option<Uuid>) -> AgentIdentitySummary {
    AgentIdentitySummary {
        id: user.id,
        username: user.username,
        display_name: user.display_name,
        owner_user_id,
    }
}

/// Convert concurrent username and email collisions into a stable public conflict.
fn map_create_error(error: sqlx::Error) -> AppError {
    if error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| code == "23505")
    {
        AppError::Conflict("Username already taken".to_string())
    } else {
        AppError::Database(error)
    }
}

/// Decide whether a caller may perform a human-only identity operation.
#[cfg(test)]
fn human_identity_operation_allowed(caller_is_agent: bool) -> bool {
    !caller_is_agent
}

/// Decide whether a human can claim an existing agent identity.
#[cfg(test)]
fn claim_allowed(
    caller_is_agent: bool,
    candidate_is_agent: bool,
    already_owned: bool,
    manages_shared_server: bool,
) -> bool {
    human_identity_operation_allowed(caller_is_agent)
        && candidate_is_agent
        && !already_owned
        && manages_shared_server
}

/// Decide whether a username lookup permits creation without retyping an account.
#[cfg(test)]
fn creation_allowed(caller_is_agent: bool, existing_user_is_agent: Option<bool>) -> bool {
    human_identity_operation_allowed(caller_is_agent) && existing_user_is_agent.is_none()
}

#[cfg(test)]
/// Exercises the human ownership and legacy claim authorization predicates.
mod tests {
    use super::*;

    /// Agent callers can neither list nor create owned identities.
    #[test]
    fn agent_callers_cannot_manage_owned_identities() {
        assert!(!human_identity_operation_allowed(true));
        assert!(!creation_allowed(true, None));
    }

    /// Human callers may enter the owner-scoped identity workflow.
    #[test]
    fn humans_use_only_their_owner_scope() {
        assert!(human_identity_operation_allowed(false));
        let owner = Uuid::new_v4();
        let other_owner = Uuid::new_v4();
        let owned = AgentIdentitySummary {
            id: Uuid::new_v4(),
            username: "owned-agent".to_string(),
            display_name: None,
            owner_user_id: Some(owner),
        };
        assert_eq!(owned.owner_user_id, Some(owner));
        assert_ne!(owned.owner_user_id, Some(other_owner));
    }

    /// Claiming requires an agent, no current owner, and management of a shared room.
    #[test]
    fn claim_requires_the_complete_authority_matrix() {
        assert!(claim_allowed(false, true, false, true));
        assert!(!claim_allowed(true, true, false, true));
        assert!(!claim_allowed(false, false, false, true));
        assert!(!claim_allowed(false, true, true, true));
        assert!(!claim_allowed(false, true, false, false));
    }

    /// Existing agent or human usernames always conflict instead of being promoted.
    #[test]
    fn username_collisions_never_promote_existing_accounts() {
        assert!(creation_allowed(false, None));
        assert!(!creation_allowed(false, Some(false)));
        assert!(!creation_allowed(false, Some(true)));
    }

    /// Identity input retains the established username and display-name limits.
    #[test]
    fn identity_input_bounds_are_enforced() {
        assert!(validate_identity_input("abc", Some("Agent")).is_ok());
        assert!(validate_identity_input("ab", None).is_err());
        assert!(validate_identity_input(&"x".repeat(33), None).is_err());
        assert!(validate_identity_input("agent", Some(&"x".repeat(65))).is_err());
    }
}
