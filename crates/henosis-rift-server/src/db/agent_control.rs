//! Transactional persistence for agent ownership and immutable room rosters.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::models::agent_control::{
    AgentSeatInput, AgentSeatView, ApplyState, ApplyStatusUpdate, BridgeStatus,
    CredentialReadiness, RoomAgentRoster,
};
use crate::models::leadership::RoomFence;
use crate::models::user::User;

/// Failure to authorize a managed operation with the current room fence.
#[derive(Debug, thiserror::Error)]
pub enum RoomFenceError {
    /// The room requires a different leadership generation.
    #[error("managed room leadership fence is stale")]
    Stale,
    /// PostgreSQL could not prove the current fence, so authorization fails closed.
    #[error("managed room leadership fence lookup failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Failures produced while atomically appending a room roster revision.
#[derive(Debug, thiserror::Error)]
pub enum WriteRosterError {
    /// The caller edited a revision other than the latest desired revision.
    #[error("room roster revision conflict; current revision is {current:?}")]
    RevisionConflict {
        /// Latest desired revision observed while holding the room state lock.
        current: Option<i64>,
    },
    /// PostgreSQL rejected or could not complete the transaction.
    #[error("room roster database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Return the human owner of an agent identity, when the agent has been claimed.
pub async fn owner_for_agent(
    pool: &PgPool,
    agent_user_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT owner_user_id FROM agent_ownership WHERE agent_user_id = $1")
        .bind(agent_user_id)
        .fetch_optional(pool)
        .await
}

/// Atomically claim an unowned agent for a human and report whether the claim won.
pub async fn claim_agent(
    pool: &PgPool,
    agent_user_id: Uuid,
    owner_user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let claimed = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO agent_ownership (agent_user_id, owner_user_id)
           SELECT agent.id, owner.id
           FROM users agent
           CROSS JOIN users owner
           WHERE agent.id = $1
             AND agent.is_agent = TRUE
             AND agent.email = agent.username || $3
             AND agent.executor_type IS DISTINCT FROM 'System'
             AND agent.agent_roster_id IS DISTINCT FROM 'henosis-room-owner'
             AND owner.id = $2
             AND owner.is_agent = FALSE
           ON CONFLICT (agent_user_id) DO NOTHING
           RETURNING agent_user_id"#,
    )
    .bind(agent_user_id)
    .bind(owner_user_id)
    .bind(super::CLAIMABLE_AGENT_EMAIL_SUFFIX)
    .fetch_optional(pool)
    .await?;
    Ok(claimed.is_some())
}

/// List only the persistent agent identities claimed by one human.
pub async fn list_owned_agents(
    pool: &PgPool,
    owner_user_id: Uuid,
) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query_as::<_, User>(
        r#"SELECT agent.*
           FROM agent_ownership ownership
           INNER JOIN users agent ON agent.id = ownership.agent_user_id
           WHERE ownership.owner_user_id = $1
             AND agent.is_agent = TRUE
           ORDER BY LOWER(agent.username), agent.id"#,
    )
    .bind(owner_user_id)
    .fetch_all(pool)
    .await
}

/// Raw apply-status row selected from `bridge_server_state`.
type BridgeStateRow = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    String,
    Option<String>,
    Option<String>,
);

/// Complete bridge-status row read while a daemon transaction owns the fence lock.
type BridgeStatusRow = (
    bool,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    String,
    Option<String>,
    Option<String>,
);

/// Durable fields needed to authorize one presented room fence.
type RoomFenceRow = (bool, i64, Option<Uuid>);

/// Return whether one existing room-state row authorizes the presented capability.
fn room_fence_row_authorizes(
    server_id: Uuid,
    row: RoomFenceRow,
    presented: Option<&RoomFence>,
) -> bool {
    let (required, epoch, lease_id) = row;
    if required {
        presented.is_some_and(|fence| fence.matches(server_id, epoch, lease_id))
    } else {
        presented.is_none()
    }
}

/// Authorize one room-state lookup without treating a presented fence as standalone traffic.
fn room_fence_lookup_authorizes(
    server_id: Uuid,
    row: Option<RoomFenceRow>,
    presented: Option<&RoomFence>,
) -> bool {
    match row {
        Some(row) => room_fence_row_authorizes(server_id, row, presented),
        None => presented.is_none(),
    }
}

/// Advance a managed room to a fresh, opaque leadership generation.
pub async fn acquire_room_fence(pool: &PgPool, server_id: Uuid) -> Result<RoomFence, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    acquire_room_fence_on_connection(&mut connection, server_id).await
}

/// Advance a managed room fence on the caller's already-owned database session.
pub async fn acquire_room_fence_on_connection(
    connection: &mut PgConnection,
    server_id: Uuid,
) -> Result<RoomFence, sqlx::Error> {
    let lease_id = Uuid::new_v4();
    let epoch = sqlx::query_scalar::<_, i64>(
        r#"UPDATE bridge_server_state
           SET fencing_epoch = fencing_epoch + 1,
               fencing_lease_id = $2,
               updated_at = NOW()
           WHERE server_id = $1
             AND fencing_required = TRUE
             AND fencing_epoch < $3
           RETURNING fencing_epoch"#,
    )
    .bind(server_id)
    .bind(lease_id)
    .bind(i64::MAX)
    .fetch_optional(connection)
    .await?
    .ok_or(sqlx::Error::RowNotFound)?;
    Ok(RoomFence {
        server_id,
        epoch,
        lease_id,
    })
}

/// Return whether a capability is the exact current fence for a managed room.
pub async fn room_fence_is_current(pool: &PgPool, fence: &RoomFence) -> Result<bool, sqlx::Error> {
    let row: Option<RoomFenceRow> = sqlx::query_as(
        r#"SELECT fencing_required, fencing_epoch, fencing_lease_id
           FROM bridge_server_state
           WHERE server_id = $1"#,
    )
    .bind(fence.server_id)
    .fetch_optional(pool)
    .await?;
    Ok(
        row.is_some_and(|row| {
            row.0 && room_fence_row_authorizes(fence.server_id, row, Some(fence))
        }),
    )
}

/// Return whether an agent belongs to at least one room that requires a fence.
pub async fn agent_requires_room_fence(pool: &PgPool, user_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"SELECT EXISTS (
               SELECT 1
               FROM users
               INNER JOIN members ON members.user_id = users.id
               INNER JOIN bridge_server_state
                   ON bridge_server_state.server_id = members.server_id
               WHERE users.id = $1
                 AND users.is_agent = TRUE
                 AND bridge_server_state.fencing_required = TRUE
           )"#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
}

/// Return whether one managed agent capability matches its current member room.
pub async fn agent_room_fence_is_current(
    pool: &PgPool,
    user_id: Uuid,
    fence: &RoomFence,
) -> Result<bool, sqlx::Error> {
    if fence.epoch <= 0 || fence.lease_id.is_nil() {
        return Ok(false);
    }
    sqlx::query_scalar(
        r#"SELECT EXISTS (
               SELECT 1
               FROM users
               INNER JOIN members ON members.user_id = users.id
               INNER JOIN bridge_server_state
                   ON bridge_server_state.server_id = members.server_id
               WHERE users.id = $1
                 AND users.is_agent = TRUE
                 AND members.server_id = $2
                 AND bridge_server_state.fencing_required = TRUE
                 AND bridge_server_state.fencing_epoch = $3
                 AND bridge_server_state.fencing_lease_id = $4
           )"#,
    )
    .bind(user_id)
    .bind(fence.server_id)
    .bind(fence.epoch)
    .bind(fence.lease_id)
    .fetch_one(pool)
    .await
}

/// Require the current fence when a room is managed, leaving ordinary Rift rooms compatible.
pub async fn require_room_fence(
    pool: &PgPool,
    server_id: Uuid,
    presented: Option<&RoomFence>,
) -> Result<(), RoomFenceError> {
    let row: Option<RoomFenceRow> = sqlx::query_as(
        r#"SELECT fencing_required, fencing_epoch, fencing_lease_id
           FROM bridge_server_state
           WHERE server_id = $1"#,
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await?;
    if room_fence_lookup_authorizes(server_id, row, presented) {
        Ok(())
    } else {
        Err(RoomFenceError::Stale)
    }
}

/// Materialize standalone state, lock its fence through the transaction, and reject stale use.
pub async fn require_room_fence_locked(
    connection: &mut PgConnection,
    server_id: Uuid,
    presented: Option<&RoomFence>,
) -> Result<(), RoomFenceError> {
    sqlx::query(
        r#"INSERT INTO bridge_server_state (server_id)
           SELECT id FROM servers WHERE id = $1
           ON CONFLICT (server_id) DO NOTHING"#,
    )
    .bind(server_id)
    .execute(&mut *connection)
    .await?;
    let row: Option<RoomFenceRow> = sqlx::query_as(
        r#"SELECT fencing_required, fencing_epoch, fencing_lease_id
           FROM bridge_server_state
           WHERE server_id = $1
           FOR SHARE"#,
    )
    .bind(server_id)
    .fetch_optional(connection)
    .await?;
    if room_fence_lookup_authorizes(server_id, row, presented) {
        Ok(())
    } else {
        Err(RoomFenceError::Stale)
    }
}

/// Raw seat row joined with its public agent identity and optional human ownership.
type SeatRow = (
    Uuid,
    Uuid,
    String,
    Option<String>,
    String,
    String,
    serde_json::Value,
    Option<Uuid>,
    bool,
    i32,
    Option<Uuid>,
);

/// Read the latest desired room roster together with durable bridge apply status.
pub async fn read_room_agent_roster(
    pool: &PgPool,
    server_id: Uuid,
) -> Result<RoomAgentRoster, sqlx::Error> {
    let state: Option<BridgeStateRow> = sqlx::query_as(
        r#"SELECT desired_revision, active_revision, last_good_revision,
                  apply_state, apply_error_code, apply_error_message
           FROM bridge_server_state
           WHERE server_id = $1"#,
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await?;

    let Some((
        desired_revision,
        active_revision,
        last_good_revision,
        apply_state,
        apply_error_code,
        apply_error_message,
    )) = state
    else {
        return Ok(RoomAgentRoster {
            server_id,
            desired_revision: None,
            active_revision: None,
            last_good_revision: None,
            apply_state: ApplyState::Idle,
            apply_error_code: None,
            apply_error_message: None,
            seats: Vec::new(),
        });
    };

    let seats = match desired_revision {
        Some(revision) => read_room_agent_revision(pool, server_id, revision).await?,
        None => Vec::new(),
    };

    Ok(RoomAgentRoster {
        server_id,
        desired_revision,
        active_revision,
        last_good_revision,
        apply_state: parse_apply_state(&apply_state)?,
        apply_error_code,
        apply_error_message,
        seats,
    })
}

/// Read daemon-visible bridge status on the caller's fence-locked database transaction.
pub async fn read_bridge_status_on_connection(
    connection: &mut PgConnection,
    server_id: Uuid,
) -> Result<BridgeStatus, sqlx::Error> {
    let row: Option<BridgeStatusRow> = sqlx::query_as(
        r#"SELECT paused, desired_revision, active_revision, last_good_revision,
                  apply_state, apply_error_code, apply_error_message
           FROM bridge_server_state
           WHERE server_id = $1"#,
    )
    .bind(server_id)
    .fetch_optional(connection)
    .await?;
    let Some((
        paused,
        desired_revision,
        active_revision,
        last_good_revision,
        apply_state,
        apply_error_code,
        apply_error_message,
    )) = row
    else {
        return Ok(BridgeStatus {
            paused: false,
            desired_revision: None,
            active_revision: None,
            last_good_revision: None,
            apply_state: ApplyState::Idle,
            apply_error_code: None,
            apply_error_message: None,
        });
    };
    Ok(BridgeStatus {
        paused,
        desired_revision,
        active_revision,
        last_good_revision,
        apply_state: parse_apply_state(&apply_state)?,
        apply_error_code,
        apply_error_message,
    })
}

/// Read one immutable room roster revision in stable execution order.
pub async fn read_room_agent_revision(
    pool: &PgPool,
    server_id: Uuid,
    revision: i64,
) -> Result<Vec<AgentSeatView>, sqlx::Error> {
    let rows: Vec<SeatRow> = sqlx::query_as(
        r#"SELECT seat.seat_id, seat.agent_user_id, agent.username,
                  agent.display_name, seat.harness_id,
                  seat.model_id, seat.settings, seat.credential_binding_id,
                  seat.enabled, seat.position, ownership.owner_user_id
           FROM room_agent_seats seat
           INNER JOIN users agent ON agent.id = seat.agent_user_id
           LEFT JOIN agent_ownership ownership
             ON ownership.agent_user_id = seat.agent_user_id
           WHERE seat.server_id = $1 AND seat.revision = $2
           ORDER BY seat.position, seat.seat_id"#,
    )
    .bind(server_id)
    .bind(revision)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                seat_id,
                agent_user_id,
                agent_username,
                agent_display_name,
                harness_id,
                model_id,
                settings,
                credential_binding_id,
                enabled,
                position,
                owner_user_id,
            )| AgentSeatView {
                agent_username,
                agent_display_name,
                credential_readiness: if credential_binding_id.is_some() {
                    CredentialReadiness::Ready
                } else {
                    CredentialReadiness::HostSession
                },
                seat: AgentSeatInput {
                    seat_id,
                    agent_user_id,
                    harness_id,
                    model_id,
                    settings,
                    credential_binding_id,
                    enabled,
                    position,
                },
                owner_user_id,
            },
        )
        .collect())
}

/// Append one immutable room roster and mark it as the pending desired revision.
pub async fn write_room_agent_roster(
    pool: &PgPool,
    server_id: Uuid,
    created_by: Uuid,
    expected_revision: Option<i64>,
    seats: &[AgentSeatInput],
) -> Result<i64, WriteRosterError> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        r#"INSERT INTO bridge_server_state (server_id, paused)
           VALUES ($1, FALSE)
           ON CONFLICT (server_id) DO NOTHING"#,
    )
    .bind(server_id)
    .execute(&mut *transaction)
    .await?;

    let current: Option<i64> = sqlx::query_scalar(
        r#"SELECT desired_revision
           FROM bridge_server_state
           WHERE server_id = $1
           FOR UPDATE"#,
    )
    .bind(server_id)
    .fetch_one(&mut *transaction)
    .await?;
    if expected_revision != current {
        return Err(WriteRosterError::RevisionConflict { current });
    }
    let revision = next_revision(current).ok_or_else(|| {
        sqlx::Error::Protocol("room roster revision exhausted BIGINT range".to_string())
    })?;

    sqlx::query(
        r#"INSERT INTO room_agent_config_revisions (server_id, revision, created_by)
           VALUES ($1, $2, $3)"#,
    )
    .bind(server_id)
    .bind(revision)
    .bind(created_by)
    .execute(&mut *transaction)
    .await?;

    for seat in seats {
        sqlx::query(
            r#"INSERT INTO room_agent_seats
               (server_id, revision, seat_id, agent_user_id, harness_id, model_id,
                settings, credential_binding_id, enabled, position)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
        )
        .bind(server_id)
        .bind(revision)
        .bind(seat.seat_id)
        .bind(seat.agent_user_id)
        .bind(&seat.harness_id)
        .bind(&seat.model_id)
        .bind(&seat.settings)
        .bind(seat.credential_binding_id)
        .bind(seat.enabled)
        .bind(seat.position)
        .execute(&mut *transaction)
        .await?;
    }

    sqlx::query(
        r#"UPDATE bridge_server_state
           SET desired_revision = $2,
               apply_state = 'pending',
               apply_error_code = NULL,
               apply_error_message = NULL,
               apply_updated_at = NOW(),
               updated_at = NOW()
           WHERE server_id = $1"#,
    )
    .bind(server_id)
    .bind(revision)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(revision)
}

/// Persist bridge reconciliation status only while the reported revision remains desired.
pub async fn set_room_apply_status(
    pool: &PgPool,
    server_id: Uuid,
    fence: &RoomFence,
    expected_desired_revision: Option<i64>,
    status: ApplyStatusUpdate,
) -> Result<(), sqlx::Error> {
    if fence.server_id != server_id {
        return Err(sqlx::Error::RowNotFound);
    }
    let result = sqlx::query(
        r#"UPDATE bridge_server_state
           SET active_revision = $5,
               last_good_revision = $6,
               apply_state = $7,
               apply_error_code = $8,
               apply_error_message = $9,
               apply_updated_at = NOW(),
               updated_at = NOW()
           WHERE server_id = $1
             AND desired_revision IS NOT DISTINCT FROM $2
             AND fencing_required = TRUE
             AND fencing_epoch = $3
             AND fencing_lease_id = $4"#,
    )
    .bind(server_id)
    .bind(expected_desired_revision)
    .bind(fence.epoch)
    .bind(fence.lease_id)
    .bind(status.active_revision)
    .bind(status.last_good_revision)
    .bind(apply_state_name(status.apply_state))
    .bind(status.error_code)
    .bind(status.error_message)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

/// Calculate the next positive revision without overflowing PostgreSQL BIGINT.
fn next_revision(current: Option<i64>) -> Option<i64> {
    current
        .unwrap_or(0)
        .checked_add(1)
        .filter(|revision| *revision > 0)
}

/// Convert a durable apply state into its stable database representation.
fn apply_state_name(state: ApplyState) -> &'static str {
    match state {
        ApplyState::Idle => "idle",
        ApplyState::Pending => "pending",
        ApplyState::Active => "active",
        ApplyState::Failed => "failed",
    }
}

/// Parse the constrained database representation of a bridge apply state.
fn parse_apply_state(value: &str) -> Result<ApplyState, sqlx::Error> {
    match value {
        "idle" => Ok(ApplyState::Idle),
        "pending" => Ok(ApplyState::Pending),
        "active" => Ok(ApplyState::Active),
        "failed" => Ok(ApplyState::Failed),
        other => Err(sqlx::Error::Protocol(format!(
            "unsupported room bridge apply state {other:?}"
        ))),
    }
}

#[cfg(test)]
/// Exercises revision arithmetic and optional live PostgreSQL persistence.
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::models::leadership::RoomFence;

    /// Build one deterministic seat shape with fresh stable identities.
    fn seat(agent_user_id: Uuid, position: i32, model_id: &str) -> AgentSeatInput {
        AgentSeatInput {
            seat_id: Uuid::new_v4(),
            agent_user_id,
            harness_id: "codex".to_string(),
            model_id: model_id.to_string(),
            settings: serde_json::json!({"reasoning_effort": "medium"}),
            credential_binding_id: None,
            enabled: true,
            position,
        }
    }

    /// Revision arithmetic starts at one, advances monotonically, and rejects overflow.
    #[test]
    fn next_revision_is_positive_and_checked() {
        assert_eq!(next_revision(None), Some(1));
        assert_eq!(next_revision(Some(1)), Some(2));
        assert_eq!(next_revision(Some(i64::MAX)), None);
        assert_eq!(next_revision(Some(-1)), None);
    }

    /// Managed rows require every current fence component while ordinary rows remain compatible.
    #[test]
    fn room_fence_row_authorization_fails_closed() {
        let server_id = Uuid::new_v4();
        let lease_id = Uuid::new_v4();
        let current = RoomFence {
            server_id,
            epoch: 4,
            lease_id,
        };

        assert!(room_fence_row_authorizes(server_id, (false, 0, None), None));
        assert!(!room_fence_row_authorizes(
            server_id,
            (false, 0, None),
            Some(&current)
        ));
        assert!(room_fence_row_authorizes(
            server_id,
            (true, 4, Some(lease_id)),
            Some(&current)
        ));
        assert!(!room_fence_row_authorizes(
            server_id,
            (true, 4, Some(lease_id)),
            None
        ));
        assert!(!room_fence_row_authorizes(
            server_id,
            (true, 5, Some(Uuid::new_v4())),
            Some(&current)
        ));
    }

    /// Missing room state stays compatible only when no managed capability was presented.
    #[test]
    fn room_fence_lookup_authorization_rejects_presented_fence_without_state() {
        let presented = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 1,
            lease_id: Uuid::new_v4(),
        };

        assert!(room_fence_lookup_authorizes(
            presented.server_id,
            None,
            None
        ));
        assert!(!room_fence_lookup_authorizes(
            presented.server_id,
            None,
            Some(&presented)
        ));
    }

    /// Live lookups reject presented capabilities for both absent and standalone room state.
    #[tokio::test]
    async fn live_presented_fence_fails_closed_without_managed_state() {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping live fence fail-closed test: HENOSIS_RIFT_TEST_DATABASE_URL is unset"
            );
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations must apply");

        let suffix = Uuid::new_v4().simple().to_string();
        let owner = crate::db::create_user(
            &pool,
            &format!("fence-closed-owner-{}", &suffix[..10]),
            &format!("fence-closed-owner-{suffix}@example.invalid"),
            "unusable-test-hash",
            None,
        )
        .await
        .expect("fence fail-closed owner must be created");
        let server = crate::db::create_server(&pool, "Fence fail-closed", None, owner.id)
            .await
            .expect("fence fail-closed server must be created");
        let presented = RoomFence {
            server_id: server.id,
            epoch: 1,
            lease_id: Uuid::new_v4(),
        };

        require_room_fence(&pool, server.id, None)
            .await
            .expect("no-header standalone compatibility must remain");
        assert!(matches!(
            require_room_fence(&pool, server.id, Some(&presented)).await,
            Err(RoomFenceError::Stale)
        ));
        sqlx::query(
            r#"INSERT INTO bridge_server_state (server_id, fencing_required)
               VALUES ($1, FALSE)"#,
        )
        .bind(server.id)
        .execute(&pool)
        .await
        .expect("standalone state row must be inserted");
        require_room_fence(&pool, server.id, None)
            .await
            .expect("no-header unmanaged-row compatibility must remain");
        let mut transaction = pool
            .begin()
            .await
            .expect("fence test transaction must begin");
        assert!(matches!(
            require_room_fence_locked(&mut transaction, server.id, Some(&presented)).await,
            Err(RoomFenceError::Stale)
        ));
    }

    /// An absent standalone row is locked before concurrent management can enable fencing.
    #[tokio::test]
    async fn live_absent_room_state_serializes_managed_enablement() {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping live absent-state serialization test: HENOSIS_RIFT_TEST_DATABASE_URL is unset"
            );
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations must apply");

        let suffix = Uuid::new_v4().simple().to_string();
        let owner = crate::db::create_user(
            &pool,
            &format!("fence-serialize-owner-{}", &suffix[..10]),
            &format!("fence-serialize-owner-{suffix}@example.invalid"),
            "unusable-test-hash",
            None,
        )
        .await
        .expect("fence serialization owner must be created");
        let server = crate::db::create_server(&pool, "Fence serialization", None, owner.id)
            .await
            .expect("fence serialization server must be created");
        let mut standalone = pool
            .begin()
            .await
            .expect("standalone fence transaction must begin");
        require_room_fence_locked(&mut standalone, server.id, None)
            .await
            .expect("an absent unmanaged room must accept no fence");

        let concurrent_pool = pool.clone();
        let enablement = tokio::spawn(async move {
            let mut transaction = concurrent_pool.begin().await?;
            sqlx::query("SET LOCAL lock_timeout = '100ms'")
                .execute(&mut *transaction)
                .await?;
            sqlx::query(
                r#"INSERT INTO bridge_server_state (server_id, fencing_required)
                   VALUES ($1, TRUE)
                   ON CONFLICT (server_id) DO UPDATE
                   SET fencing_required = TRUE, updated_at = NOW()"#,
            )
            .bind(server.id)
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await
        })
        .await
        .expect("managed enablement task must join");
        let error = enablement.expect_err("managed enablement must wait for the standalone lock");
        let code = error
            .as_database_error()
            .and_then(|database| database.code())
            .map(|code| code.into_owned());
        assert_eq!(code.as_deref(), Some("55P03"));
        standalone
            .commit()
            .await
            .expect("standalone fence transaction must commit");
    }

    /// A live room issues monotonically newer fences and invalidates its predecessor.
    #[tokio::test]
    async fn live_room_fence_advances_atomically() {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!("skipping live room fence test: HENOSIS_RIFT_TEST_DATABASE_URL is unset");
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations must apply");

        let suffix = Uuid::new_v4().simple().to_string();
        let owner = crate::db::create_user(
            &pool,
            &format!("fence-owner-{}", &suffix[..10]),
            &format!("fence-owner-{suffix}@example.invalid"),
            "unusable-test-hash",
            Some("Fence Owner"),
        )
        .await
        .expect("owner must be created");
        let server = crate::db::create_server(&pool, "Fence test", None, owner.id)
            .await
            .expect("server must be created");
        sqlx::query(
            r#"INSERT INTO bridge_server_state (server_id, fencing_required)
               VALUES ($1, TRUE)"#,
        )
        .bind(server.id)
        .execute(&pool)
        .await
        .expect("test room must require fencing");

        let first = acquire_room_fence(&pool, server.id)
            .await
            .expect("first fence must be issued");
        assert!(room_fence_is_current(&pool, &first).await.unwrap());
        let second = acquire_room_fence(&pool, server.id)
            .await
            .expect("second fence must be issued");
        assert_eq!(second.epoch, first.epoch + 1);
        assert_ne!(second.lease_id, first.lease_id);
        assert!(!room_fence_is_current(&pool, &first).await.unwrap());
        assert!(room_fence_is_current(&pool, &second).await.unwrap());
        let mut dedicated = pool
            .acquire()
            .await
            .expect("dedicated fence connection must be available");
        let third = acquire_room_fence_on_connection(&mut dedicated, server.id)
            .await
            .expect("the caller's dedicated connection must issue a fence");
        assert_eq!(third.epoch, second.epoch + 1);
        assert!(room_fence_is_current(&pool, &third).await.unwrap());

        sqlx::query("DELETE FROM servers WHERE id = $1")
            .bind(server.id)
            .execute(&pool)
            .await
            .expect("test server cleanup must succeed");
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(owner.id)
            .execute(&pool)
            .await
            .expect("test owner cleanup must succeed");
    }

    /// A live test database preserves old snapshots and rejects stale editors.
    #[tokio::test]
    async fn live_roster_revisions_are_immutable() {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping live roster persistence test: HENOSIS_RIFT_TEST_DATABASE_URL is unset"
            );
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("test database migrations must apply");

        let suffix = Uuid::new_v4().simple().to_string();
        let owner = crate::db::create_user(
            &pool,
            &format!("owner-{}", &suffix[..12]),
            &format!("owner-{suffix}@example.invalid"),
            "unusable-test-hash",
            Some("Roster Owner"),
        )
        .await
        .expect("owner must be created");
        let first_agent_username = format!("agent-a-{}", &suffix[..10]);
        let first_agent = crate::db::create_agent_user(
            &pool,
            &first_agent_username,
            &format!("{first_agent_username}@agent.local"),
            "unusable-test-hash",
            Some("Agent A"),
        )
        .await
        .expect("first agent must be created");
        let second_agent_username = format!("agent-b-{}", &suffix[..10]);
        let second_agent = crate::db::create_agent_user(
            &pool,
            &second_agent_username,
            &format!("{second_agent_username}@agent.local"),
            "unusable-test-hash",
            Some("Agent B"),
        )
        .await
        .expect("second agent must be created");
        let server = crate::db::create_server(&pool, "Roster revision test", None, owner.id)
            .await
            .expect("server must be created");
        let reserved_agent: crate::models::user::User = sqlx::query_as(
            r#"INSERT INTO users
               (username, email, password_hash, display_name, is_agent, executor_type, agent_roster_id)
               VALUES ($1, $2, 'unusable-test-hash', 'Reserved Agent', TRUE, 'System',
                       'henosis-room-owner')
               RETURNING *"#,
        )
        .bind(format!("reserved-{}", &suffix[..12]))
        .bind(format!("reserved-{suffix}@example.invalid"))
        .fetch_one(&pool)
        .await
        .expect("reserved agent must be created");
        crate::db::add_member(&pool, server.id, owner.id)
            .await
            .expect("owner membership must be created");
        crate::db::add_member(&pool, server.id, reserved_agent.id)
            .await
            .expect("reserved membership must be created");

        assert!(claim_agent(&pool, first_agent.id, owner.id).await.unwrap());
        assert!(claim_agent(&pool, second_agent.id, owner.id).await.unwrap());
        assert!(!claim_agent(&pool, first_agent.id, owner.id).await.unwrap());
        assert!(
            !claim_agent(&pool, reserved_agent.id, owner.id)
                .await
                .unwrap()
        );
        assert!(
            !crate::db::claim_agent_as_shared_manager(&pool, owner.id, reserved_agent.id)
                .await
                .unwrap()
        );
        assert_eq!(
            owner_for_agent(&pool, reserved_agent.id).await.unwrap(),
            None
        );
        assert_eq!(
            owner_for_agent(&pool, first_agent.id).await.unwrap(),
            Some(owner.id)
        );
        assert_eq!(list_owned_agents(&pool, owner.id).await.unwrap().len(), 2);

        let first_revision_seats = vec![seat(first_agent.id, 0, "gpt-5.6-sol")];
        let revision_one =
            write_room_agent_roster(&pool, server.id, owner.id, None, &first_revision_seats)
                .await
                .expect("revision one must commit");
        assert_eq!(revision_one, 1);

        let stale =
            write_room_agent_roster(&pool, server.id, owner.id, Some(0), &first_revision_seats)
                .await
                .unwrap_err();
        assert!(matches!(
            stale,
            WriteRosterError::RevisionConflict { current: Some(1) }
        ));

        let revision_two_seats = vec![
            seat(first_agent.id, 0, "gpt-5.6-sol"),
            seat(second_agent.id, 1, "gpt-5.6-sol"),
        ];
        let revision_two =
            write_room_agent_roster(&pool, server.id, owner.id, Some(1), &revision_two_seats)
                .await
                .expect("revision two must commit");
        assert_eq!(revision_two, 2);

        let preserved: Vec<(Uuid, String)> = sqlx::query_as(
            r#"SELECT agent_user_id, model_id
               FROM room_agent_seats
               WHERE server_id = $1 AND revision = 1
               ORDER BY position"#,
        )
        .bind(server.id)
        .fetch_all(&pool)
        .await
        .expect("revision one must remain readable");
        assert_eq!(preserved, vec![(first_agent.id, "gpt-5.6-sol".to_string())]);

        let roster = read_room_agent_roster(&pool, server.id)
            .await
            .expect("latest roster must be readable");
        assert_eq!(roster.desired_revision, Some(2));
        assert_eq!(roster.seats.len(), 2);
        assert_eq!(roster.apply_state, ApplyState::Pending);
        assert_eq!(roster.seats[0].agent_username, first_agent.username);
        assert_eq!(roster.seats[0].agent_display_name, first_agent.display_name);
        assert_eq!(roster.seats[0].owner_user_id, Some(owner.id));
        assert_eq!(roster.seats[1].agent_username, second_agent.username);
        assert_eq!(
            roster.seats[1].agent_display_name,
            second_agent.display_name
        );
        assert_eq!(roster.seats[1].owner_user_id, Some(owner.id));
        sqlx::query(
            r#"UPDATE bridge_server_state
               SET fencing_required = TRUE
               WHERE server_id = $1"#,
        )
        .bind(server.id)
        .execute(&pool)
        .await
        .expect("test room must require leadership fencing");
        let fence = acquire_room_fence(&pool, server.id)
            .await
            .expect("test leader must acquire a fence");

        let stale_status = set_room_apply_status(
            &pool,
            server.id,
            &fence,
            Some(1),
            ApplyStatusUpdate {
                active_revision: Some(1),
                last_good_revision: Some(1),
                apply_state: ApplyState::Active,
                error_code: None,
                error_message: None,
            },
        )
        .await
        .expect_err("stale status must not overwrite revision two");
        assert!(matches!(stale_status, sqlx::Error::RowNotFound));
        let unchanged = read_room_agent_roster(&pool, server.id)
            .await
            .expect("stale status must leave the roster readable");
        assert_eq!(unchanged.active_revision, None);
        assert_eq!(unchanged.apply_state, ApplyState::Pending);

        set_room_apply_status(
            &pool,
            server.id,
            &fence,
            Some(2),
            ApplyStatusUpdate {
                active_revision: Some(2),
                last_good_revision: Some(2),
                apply_state: ApplyState::Active,
                error_code: None,
                error_message: None,
            },
        )
        .await
        .expect("current desired revision may update status");
        let applied = read_room_agent_roster(&pool, server.id)
            .await
            .expect("applied roster must remain readable");
        assert_eq!(applied.active_revision, Some(2));
        assert_eq!(applied.last_good_revision, Some(2));
        assert_eq!(applied.apply_state, ApplyState::Active);

        sqlx::query("DELETE FROM servers WHERE id = $1")
            .bind(server.id)
            .execute(&pool)
            .await
            .expect("test server cleanup must succeed");
        for user_id in [first_agent.id, second_agent.id, reserved_agent.id, owner.id] {
            sqlx::query("DELETE FROM users WHERE id = $1")
                .bind(user_id)
                .execute(&pool)
                .await
                .expect("test user cleanup must succeed");
        }
    }
}
