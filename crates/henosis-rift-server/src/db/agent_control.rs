//! Transactional persistence for agent ownership and immutable room rosters.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::agent_control::{
    AgentSeatInput, AgentSeatView, ApplyState, ApplyStatusUpdate, CredentialReadiness,
    RoomAgentRoster,
};
use crate::models::user::User;

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
             AND owner.id = $2
             AND owner.is_agent = FALSE
           ON CONFLICT (agent_user_id) DO NOTHING
           RETURNING agent_user_id"#,
    )
    .bind(agent_user_id)
    .bind(owner_user_id)
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

/// Raw seat row selected from `room_agent_seats` joined with agent ownership.
type SeatRow = (
    Uuid,
    Uuid,
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

/// Read one immutable room roster revision in stable execution order.
pub async fn read_room_agent_revision(
    pool: &PgPool,
    server_id: Uuid,
    revision: i64,
) -> Result<Vec<AgentSeatView>, sqlx::Error> {
    let rows: Vec<SeatRow> = sqlx::query_as(
        r#"SELECT seat.seat_id, seat.agent_user_id, seat.harness_id,
                  seat.model_id, seat.settings, seat.credential_binding_id,
                  seat.enabled, seat.position, ownership.owner_user_id
           FROM room_agent_seats seat
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
                harness_id,
                model_id,
                settings,
                credential_binding_id,
                enabled,
                position,
                owner_user_id,
            )| AgentSeatView {
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

/// Persist bridge reconciliation status for one room.
pub async fn set_room_apply_status(
    pool: &PgPool,
    server_id: Uuid,
    status: ApplyStatusUpdate,
) -> Result<(), sqlx::Error> {
    let result = sqlx::query(
        r#"UPDATE bridge_server_state
           SET active_revision = $2,
               last_good_revision = $3,
               apply_state = $4,
               apply_error_code = $5,
               apply_error_message = $6,
               apply_updated_at = NOW(),
               updated_at = NOW()
           WHERE server_id = $1"#,
    )
    .bind(server_id)
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
        let first_agent = crate::db::create_agent_user(
            &pool,
            &format!("agent-a-{}", &suffix[..10]),
            &format!("agent-a-{suffix}@example.invalid"),
            "unusable-test-hash",
            Some("Agent A"),
        )
        .await
        .expect("first agent must be created");
        let second_agent = crate::db::create_agent_user(
            &pool,
            &format!("agent-b-{}", &suffix[..10]),
            &format!("agent-b-{suffix}@example.invalid"),
            "unusable-test-hash",
            Some("Agent B"),
        )
        .await
        .expect("second agent must be created");
        let server = crate::db::create_server(&pool, "Roster revision test", None, owner.id)
            .await
            .expect("server must be created");

        assert!(claim_agent(&pool, first_agent.id, owner.id).await.unwrap());
        assert!(claim_agent(&pool, second_agent.id, owner.id).await.unwrap());
        assert!(!claim_agent(&pool, first_agent.id, owner.id).await.unwrap());
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

        sqlx::query("DELETE FROM servers WHERE id = $1")
            .bind(server.id)
            .execute(&pool)
            .await
            .expect("test server cleanup must succeed");
        for user_id in [first_agent.id, second_agent.id, owner.id] {
            sqlx::query("DELETE FROM users WHERE id = $1")
                .bind(user_id)
                .execute(&pool)
                .await
                .expect("test user cleanup must succeed");
        }
    }
}
