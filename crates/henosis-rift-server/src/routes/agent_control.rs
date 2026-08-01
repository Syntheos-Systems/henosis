//! Authenticated room roster, capability discovery, and reconciliation routes.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::agent_control::ManagedAgentControlRegistry;
use crate::auth::middleware::AuthUser;
use crate::db::{self, agent_control::WriteRosterError};
use crate::error::AppError;
use crate::models::agent_control::{
    AgentSeatInput, ExecutionCapabilityCatalog, RoomAgentRoster, UpdateRoomAgentRoster,
};
use crate::models::permissions::perms;

/// Authenticated human context for one room roster request.
#[derive(Debug, Clone, Copy)]
struct RoomActor {
    /// Stable Rift human user identifier.
    user_id: Uuid,
    /// Whether the human has room-management authority.
    can_manage: bool,
}

/// One seat paired with its freshly read persistent owner.
#[derive(Debug, Clone, PartialEq)]
struct OwnedSeat {
    /// Secret-free desired seat configuration.
    seat: AgentSeatInput,
    /// Current human owner, or none for an imported unclaimed identity.
    owner_user_id: Option<Uuid>,
}

/// Pure reasons a whole-roster mutation is not authorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RosterAuthorizationError {
    /// Agent callers and nonmembers cannot mutate rosters.
    CallerDenied,
    /// A stable seat identifier was changed for an existing agent.
    SeatIdentityChanged,
    /// The requested operation requires ownership of the affected agent.
    OwnerRequired,
    /// Reordering another room participant requires manager authority.
    ManagerRequired,
}

/// Return the latest desired room roster to a human room member.
pub async fn get_agent_roster(
    auth: AuthUser,
    State(pool): State<PgPool>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<RoomAgentRoster>, AppError> {
    require_room_actor(&pool, server_id, auth.user_id).await?;
    Ok(Json(
        db::agent_control::read_room_agent_roster(&pool, server_id).await?,
    ))
}

/// Replace the desired roster with one authorized immutable revision.
pub async fn put_agent_roster(
    auth: AuthUser,
    State(pool): State<PgPool>,
    State(registry): State<ManagedAgentControlRegistry>,
    Path(server_id): Path<Uuid>,
    Json(request): Json<UpdateRoomAgentRoster>,
) -> Result<Json<RoomAgentRoster>, AppError> {
    let actor = require_room_actor(&pool, server_id, auth.user_id).await?;
    request
        .validate()
        .map_err(|error| AppError::BadRequest(error.to_string()))?;
    let current = db::agent_control::read_room_agent_roster(&pool, server_id).await?;
    let current_seats = refresh_owners(
        &pool,
        current.seats.into_iter().map(|view| view.seat).collect(),
    )
    .await?;
    let submitted_seats = refresh_owners(&pool, request.seats.clone()).await?;
    authorize_roster_change(Some(actor), &current_seats, &submitted_seats)
        .map_err(map_authorization_error)?;

    let controller = registry.controller()?;
    controller
        .validate_revision(server_id, actor.user_id, &request.seats)
        .await?;
    let revision = db::agent_control::write_room_agent_roster(
        &pool,
        server_id,
        actor.user_id,
        request.expected_revision,
        &request.seats,
    )
    .await
    .map_err(map_write_error)?;
    if let Err(error) = controller.revision_committed(server_id, revision).await {
        tracing::warn!(
            server_id = %server_id,
            revision,
            error = %error,
            "room agent revision committed but its in-process notification failed"
        );
    }
    Ok(Json(
        db::agent_control::read_room_agent_roster(&pool, server_id).await?,
    ))
}

/// Return deployment-discovered execution choices to a human room member.
pub async fn get_agent_capabilities(
    auth: AuthUser,
    State(pool): State<PgPool>,
    State(registry): State<ManagedAgentControlRegistry>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<ExecutionCapabilityCatalog>, AppError> {
    let actor = require_room_actor(&pool, server_id, auth.user_id).await?;
    let catalog = registry
        .controller()?
        .capabilities(server_id, actor.user_id)
        .await?;
    Ok(Json(catalog))
}

/// Retry the current desired revision without creating another snapshot.
pub async fn reconcile_agent_roster(
    auth: AuthUser,
    State(pool): State<PgPool>,
    State(registry): State<ManagedAgentControlRegistry>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<RoomAgentRoster>, AppError> {
    let actor = require_room_actor(&pool, server_id, auth.user_id).await?;
    if !actor.can_manage {
        return Err(AppError::Forbidden);
    }
    let roster = db::agent_control::read_room_agent_roster(&pool, server_id).await?;
    let revision = roster.desired_revision.ok_or_else(|| {
        AppError::BadRequest("Room has no desired agent revision to reconcile".to_string())
    })?;
    registry
        .controller()?
        .retry_revision(server_id, revision)
        .await?;
    Ok(Json(roster))
}

/// Load server-truth human membership and manager authority for one request.
async fn require_room_actor(
    pool: &PgPool,
    server_id: Uuid,
    user_id: Uuid,
) -> Result<RoomActor, AppError> {
    let user = db::get_user_by_id(pool, user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".to_string()))?;
    let is_member = db::is_member(pool, server_id, user_id).await?;
    if user.is_agent || !is_member {
        return Err(AppError::Forbidden);
    }
    let permissions = db::get_member_permissions(pool, server_id, user_id).await?;
    Ok(RoomActor {
        user_id,
        can_manage: perms::has(permissions, perms::MANAGE_SERVER),
    })
}

/// Re-read every referenced identity and owner before authorizing a mutation.
async fn refresh_owners(
    pool: &PgPool,
    seats: Vec<AgentSeatInput>,
) -> Result<Vec<OwnedSeat>, AppError> {
    let mut owned = Vec::with_capacity(seats.len());
    for seat in seats {
        let user = db::get_user_by_id(pool, seat.agent_user_id)
            .await?
            .ok_or_else(|| AppError::BadRequest("Agent identity does not exist".to_string()))?;
        if !user.is_agent {
            return Err(AppError::BadRequest(
                "Room seats require agent identities".to_string(),
            ));
        }
        let owner_user_id = db::agent_control::owner_for_agent(pool, seat.agent_user_id).await?;
        owned.push(OwnedSeat {
            seat,
            owner_user_id,
        });
    }
    Ok(owned)
}

/// Enforce owner and manager powers over a complete desired roster replacement.
fn authorize_roster_change(
    actor: Option<RoomActor>,
    current: &[OwnedSeat],
    submitted: &[OwnedSeat],
) -> Result<(), RosterAuthorizationError> {
    let Some(actor) = actor else {
        return Err(RosterAuthorizationError::CallerDenied);
    };
    let current_by_agent: HashMap<Uuid, &OwnedSeat> = current
        .iter()
        .map(|owned| (owned.seat.agent_user_id, owned))
        .collect();
    let submitted_by_agent: HashMap<Uuid, &OwnedSeat> = submitted
        .iter()
        .map(|owned| (owned.seat.agent_user_id, owned))
        .collect();

    for proposed in submitted {
        let owner_is_actor = proposed.owner_user_id == Some(actor.user_id);
        let Some(existing) = current_by_agent.get(&proposed.seat.agent_user_id) else {
            if !owner_is_actor {
                return Err(RosterAuthorizationError::OwnerRequired);
            }
            continue;
        };
        if existing.seat.seat_id != proposed.seat.seat_id {
            return Err(RosterAuthorizationError::SeatIdentityChanged);
        }
        let owner_only_configuration_changed = existing.seat.harness_id != proposed.seat.harness_id
            || existing.seat.model_id != proposed.seat.model_id
            || existing.seat.settings != proposed.seat.settings
            || existing.seat.credential_binding_id != proposed.seat.credential_binding_id;
        if owner_only_configuration_changed && !owner_is_actor {
            return Err(RosterAuthorizationError::OwnerRequired);
        }
        if existing.seat.enabled != proposed.seat.enabled {
            let manager_disable =
                existing.seat.enabled && !proposed.seat.enabled && actor.can_manage;
            if !owner_is_actor && !manager_disable {
                return Err(RosterAuthorizationError::OwnerRequired);
            }
        }
        if existing.seat.position != proposed.seat.position && !actor.can_manage {
            return Err(RosterAuthorizationError::ManagerRequired);
        }
    }

    for existing in current {
        if !submitted_by_agent.contains_key(&existing.seat.agent_user_id)
            && existing.owner_user_id != Some(actor.user_id)
            && !actor.can_manage
        {
            return Err(RosterAuthorizationError::OwnerRequired);
        }
    }
    Ok(())
}

/// Map private authorization detail to Rift's non-disclosing forbidden response.
fn map_authorization_error(_error: RosterAuthorizationError) -> AppError {
    AppError::Forbidden
}

/// Preserve the current revision only for optimistic concurrency conflicts.
fn map_write_error(error: WriteRosterError) -> AppError {
    match error {
        WriteRosterError::RevisionConflict { current } => AppError::revision_conflict(current),
        WriteRosterError::Database(error) => AppError::Database(error),
    }
}

#[cfg(test)]
/// Exercises the complete owner-manager mutation matrix and conflict mapping.
mod tests {
    use serde_json::json;

    use super::*;

    /// Construct one test actor with optional server-management authority.
    fn actor(user_id: Uuid, can_manage: bool) -> RoomActor {
        RoomActor {
            user_id,
            can_manage,
        }
    }

    /// Construct one owned seat for mutation tests.
    fn owned_seat(owner_user_id: Option<Uuid>, position: i32) -> OwnedSeat {
        OwnedSeat {
            seat: AgentSeatInput {
                seat_id: Uuid::new_v4(),
                agent_user_id: Uuid::new_v4(),
                harness_id: "codex".to_string(),
                model_id: "gpt-5.6-sol".to_string(),
                settings: json!({"reasoning_effort": "medium"}),
                credential_binding_id: None,
                enabled: true,
                position,
            },
            owner_user_id,
        }
    }

    /// Agent or nonmember callers are represented by no authorized room actor.
    #[test]
    fn denied_callers_cannot_mutate_rosters() {
        assert_eq!(
            authorize_roster_change(None, &[], &[]),
            Err(RosterAuthorizationError::CallerDenied)
        );
    }

    /// An owner may add and reconfigure their own persistent agent seat.
    #[test]
    fn owners_control_their_agent_configuration() {
        let owner = Uuid::new_v4();
        let existing = owned_seat(Some(owner), 0);
        let mut changed = existing.clone();
        changed.seat.model_id = "gpt-5.6-sol-fast".to_string();
        changed.seat.settings = json!({"reasoning_effort": "high"});
        changed.seat.credential_binding_id = Some(Uuid::new_v4());
        assert!(
            authorize_roster_change(
                Some(actor(owner, false)),
                &[],
                std::slice::from_ref(&existing)
            )
            .is_ok()
        );
        assert!(
            authorize_roster_change(Some(actor(owner, false)), &[existing], &[changed]).is_ok()
        );
    }

    /// A manager may disable, reorder, or remove another human's seat.
    #[test]
    fn managers_control_room_participation() {
        let owner = Uuid::new_v4();
        let manager = Uuid::new_v4();
        let existing = owned_seat(Some(owner), 0);
        let mut disabled = existing.clone();
        disabled.seat.enabled = false;
        disabled.seat.position = 2;
        assert!(
            authorize_roster_change(
                Some(actor(manager, true)),
                std::slice::from_ref(&existing),
                &[disabled]
            )
            .is_ok()
        );
        assert!(authorize_roster_change(Some(actor(manager, true)), &[existing], &[]).is_ok());
    }

    /// Manager authority alone cannot substitute another owner's credentials or model.
    #[test]
    fn managers_cannot_reconfigure_another_owners_seat() {
        let owner = Uuid::new_v4();
        let manager = Uuid::new_v4();
        let existing = owned_seat(Some(owner), 0);
        let mut changed = existing.clone();
        changed.seat.credential_binding_id = Some(Uuid::new_v4());
        assert_eq!(
            authorize_roster_change(Some(actor(manager, true)), &[existing], &[changed]),
            Err(RosterAuthorizationError::OwnerRequired)
        );
    }

    /// A manager cannot enable another owner's disabled seat.
    #[test]
    fn managers_cannot_enable_another_owners_seat() {
        let owner = Uuid::new_v4();
        let manager = Uuid::new_v4();
        let mut existing = owned_seat(Some(owner), 0);
        existing.seat.enabled = false;
        let mut enabled = existing.clone();
        enabled.seat.enabled = true;
        assert_eq!(
            authorize_roster_change(Some(actor(manager, true)), &[existing], &[enabled]),
            Err(RosterAuthorizationError::OwnerRequired)
        );
    }

    /// Stable seat IDs cannot be swapped even by the owner.
    #[test]
    fn existing_seat_identity_is_immutable() {
        let owner = Uuid::new_v4();
        let existing = owned_seat(Some(owner), 0);
        let mut changed = existing.clone();
        changed.seat.seat_id = Uuid::new_v4();
        assert_eq!(
            authorize_roster_change(Some(actor(owner, true)), &[existing], &[changed]),
            Err(RosterAuthorizationError::SeatIdentityChanged)
        );
    }

    /// Revision mismatches preserve the stable conflict code and current revision.
    #[test]
    fn revision_conflicts_map_stably() {
        assert!(matches!(
            map_write_error(WriteRosterError::RevisionConflict { current: Some(7) }),
            AppError::Coded {
                code: "revision_conflict",
                message,
                ..
            } if message.contains('7')
        ));
    }
}
