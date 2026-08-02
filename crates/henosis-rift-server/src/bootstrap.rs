//! Transactional bootstrap for the managed Henosis Rift room.

use argon2::{
    password_hash::{rand_core::OsRng, SaltString},
    Argon2, PasswordHasher,
};
use rand::Rng as _;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::agent_control::{AgentSeatInput, UpdateRoomAgentRoster};
use crate::models::permissions::perms;

/// Reserved username owned exclusively by the Henosis room runtime.
const SERVICE_USERNAME: &str = "henosis-room";

/// Reserved email proving the service account was created by Henosis.
const SERVICE_EMAIL: &str = "henosis-room@agent.local";

/// Stable marker used to distinguish the managed room from user-created rooms.
const ROOM_MARKER: &str = "Managed by the Henosis room runtime.";

/// Stable marker used to recover the managed channel after a display-name change.
const CHANNEL_MARKER: &str = "Primary Henosis agent room.";

/// PostgreSQL advisory-lock key serializing managed room creation.
const BOOTSTRAP_LOCK_KEY: i64 = 0x48_65_6e_6f_73_69_73;

/// Display settings used when creating a managed room for the first time.
pub struct ManagedRoomConfig {
    /// Initial server name visible to room participants.
    pub server_name: String,
    /// Initial primary channel name visible to room participants.
    pub channel_name: String,
}

/// Supplies stable public-facing names for a fresh Henosis room.
impl Default for ManagedRoomConfig {
    /// Build the default Henosis server and general room names.
    fn default() -> Self {
        Self {
            server_name: "Henosis".to_string(),
            channel_name: "general".to_string(),
        }
    }
}

/// Identifiers required to connect the Synapse bridge to its managed room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedRoom {
    /// Service identity that owns the managed room and authors its imported revision.
    pub owner_id: Uuid,
    /// Rift server containing the persistent agent room.
    pub server_id: Uuid,
    /// Primary text channel observed by the bridge.
    pub channel_id: Uuid,
}

/// Result of attempting the one-time deployment-roster import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialRosterImport {
    /// This transaction installed the first immutable revision.
    Imported {
        /// Newly installed revision, always one for an initial import.
        revision: i64,
    },
    /// Durable desired state already existed and was left unchanged.
    Existing {
        /// Desired revision observed while holding the room state lock.
        desired_revision: i64,
    },
}

/// Failures returned while converging managed room persistence.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// A display setting violates the same length contract as the public API.
    #[error("invalid managed room configuration: {0}")]
    InvalidConfig(String),
    /// The reserved service identity belongs to a non-Henosis account.
    #[error("reserved Rift identity conflict: {0}")]
    ReservedIdentity(String),
    /// Argon2 could not hash the generated service credential.
    #[error("failed to secure the managed Rift identity: {0}")]
    Password(String),
    /// PostgreSQL could not converge or commit the managed room.
    #[error("managed Rift room database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Converge the machine-owned room resources and return their persistent IDs.
pub async fn bootstrap_managed_room(
    pool: &PgPool,
    config: ManagedRoomConfig,
) -> Result<ManagedRoom, BootstrapError> {
    validate_config(&config)?;
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(BOOTSTRAP_LOCK_KEY)
        .execute(&mut *transaction)
        .await?;

    let owner_id = match sqlx::query_as::<_, (Uuid, bool, String)>(
        "SELECT id, is_agent, email FROM users WHERE username = $1",
    )
    .bind(SERVICE_USERNAME)
    .fetch_optional(&mut *transaction)
    .await?
    {
        Some((id, true, email)) if email == SERVICE_EMAIL => id,
        Some((_, is_agent, email)) => {
            return Err(BootstrapError::ReservedIdentity(format!(
                "username {SERVICE_USERNAME:?} has is_agent={is_agent} and email {email:?}"
            )));
        }
        None => {
            let password_hash = random_password_hash()?;
            sqlx::query_scalar::<_, Uuid>(
                r#"INSERT INTO users
                   (username, display_name, email, password_hash, is_agent, executor_type, agent_roster_id)
                   VALUES ($1, 'Henosis', $2, $3, TRUE, 'System', 'henosis-room-owner')
                   RETURNING id"#,
            )
            .bind(SERVICE_USERNAME)
            .bind(SERVICE_EMAIL)
            .bind(password_hash)
            .fetch_one(&mut *transaction)
            .await?
        }
    };

    let server_id = match sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM servers WHERE owner_id = $1 AND description = $2 ORDER BY created_at LIMIT 1",
    )
    .bind(owner_id)
    .bind(ROOM_MARKER)
    .fetch_optional(&mut *transaction)
    .await?
    {
        Some(id) => id,
        None => {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO servers (name, description, owner_id) VALUES ($1, $2, $3) RETURNING id",
            )
            .bind(&config.server_name)
            .bind(ROOM_MARKER)
            .bind(owner_id)
            .fetch_one(&mut *transaction)
            .await?
        }
    };

    sqlx::query("INSERT INTO members (server_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(server_id)
        .bind(owner_id)
        .execute(&mut *transaction)
        .await?;

    if sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM roles WHERE server_id = $1 AND is_default = TRUE ORDER BY created_at LIMIT 1",
    )
    .bind(server_id)
    .fetch_optional(&mut *transaction)
    .await?
    .is_none()
    {
        sqlx::query(
            r#"INSERT INTO roles
               (server_id, name, color, permissions, position, is_default)
               VALUES ($1, '@everyone', 0, $2, 0, TRUE)"#,
        )
        .bind(server_id)
        .bind(perms::DEFAULT)
        .execute(&mut *transaction)
        .await?;
    }

    let channel_id = match sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM channels WHERE server_id = $1 AND topic = $2 ORDER BY created_at LIMIT 1",
    )
    .bind(server_id)
    .bind(CHANNEL_MARKER)
    .fetch_optional(&mut *transaction)
    .await?
    {
        Some(id) => id,
        None => {
            sqlx::query_scalar::<_, Uuid>(
                r#"INSERT INTO channels (server_id, name, topic, channel_type, position)
                   VALUES ($1, $2, $3, 'text', 0)
                   RETURNING id"#,
            )
            .bind(server_id)
            .bind(&config.channel_name)
            .bind(CHANNEL_MARKER)
            .fetch_one(&mut *transaction)
            .await?
        }
    };

    sqlx::query(
        r#"INSERT INTO bridge_server_state (server_id, paused)
           VALUES ($1, FALSE)
           ON CONFLICT (server_id) DO NOTHING"#,
    )
    .bind(server_id)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    Ok(ManagedRoom {
        owner_id,
        server_id,
        channel_id,
    })
}

/// Atomically import the already-running TOML roster as active revision one.
///
/// The caller supplies stable agent IDs from the bridge readiness payload. This
/// operation never resolves identities by name and never inserts ownership
/// records. A row lock makes an existing desired revision win without mutation.
pub async fn import_initial_agent_roster(
    pool: &PgPool,
    server_id: Uuid,
    created_by: Uuid,
    seats: &[AgentSeatInput],
) -> Result<InitialRosterImport, sqlx::Error> {
    UpdateRoomAgentRoster {
        expected_revision: None,
        seats: seats.to_vec(),
    }
    .validate()
    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;

    let mut transaction = pool.begin().await?;
    sqlx::query(
        r#"INSERT INTO bridge_server_state (server_id, paused)
           VALUES ($1, FALSE)
           ON CONFLICT (server_id) DO NOTHING"#,
    )
    .bind(server_id)
    .execute(&mut *transaction)
    .await?;

    let desired_revision: Option<i64> = sqlx::query_scalar(
        r#"SELECT desired_revision
           FROM bridge_server_state
           WHERE server_id = $1
           FOR UPDATE"#,
    )
    .bind(server_id)
    .fetch_one(&mut *transaction)
    .await?;
    if let Some(desired_revision) = desired_revision {
        transaction.commit().await?;
        return Ok(InitialRosterImport::Existing { desired_revision });
    }

    /// First immutable revision reserved for deployment-roster import.
    const INITIAL_REVISION: i64 = 1;
    sqlx::query(
        r#"INSERT INTO room_agent_config_revisions (server_id, revision, created_by)
           VALUES ($1, $2, $3)"#,
    )
    .bind(server_id)
    .bind(INITIAL_REVISION)
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
        .bind(INITIAL_REVISION)
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

    let updated = sqlx::query(
        r#"UPDATE bridge_server_state
           SET desired_revision = $2,
               active_revision = $2,
               last_good_revision = $2,
               apply_state = 'active',
               apply_error_code = NULL,
               apply_error_message = NULL,
               apply_updated_at = NOW(),
               updated_at = NOW()
           WHERE server_id = $1"#,
    )
    .bind(server_id)
    .bind(INITIAL_REVISION)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound);
    }

    transaction.commit().await?;
    Ok(InitialRosterImport::Imported {
        revision: INITIAL_REVISION,
    })
}

/// Validate managed display names before opening a database transaction.
fn validate_config(config: &ManagedRoomConfig) -> Result<(), BootstrapError> {
    let server_len = config.server_name.chars().count();
    if !(1..=100).contains(&server_len) {
        return Err(BootstrapError::InvalidConfig(
            "server_name must contain 1 to 100 characters".to_string(),
        ));
    }
    let channel_len = config.channel_name.chars().count();
    if !(1..=100).contains(&channel_len) {
        return Err(BootstrapError::InvalidConfig(
            "channel_name must contain 1 to 100 characters".to_string(),
        ));
    }
    Ok(())
}

/// Hash an unguessable generated credential that is never returned or persisted in plaintext.
fn random_password_hash() -> Result<String, BootstrapError> {
    let random: [u8; 32] = rand::rng().random();
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(&random, &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| BootstrapError::Password(error.to_string()))
}

/// Exercises managed room validation and optional live initial-roster persistence.
#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    /// Build one valid imported seat for a provisioned Rift agent.
    fn imported_seat(agent_user_id: Uuid, position: i32) -> AgentSeatInput {
        AgentSeatInput {
            seat_id: agent_user_id,
            agent_user_id,
            harness_id: "codex".to_string(),
            model_id: "gpt-5.6-sol".to_string(),
            settings: serde_json::json!({"reasoning_effort": "medium"}),
            credential_binding_id: None,
            enabled: true,
            position,
        }
    }

    /// Default room display names satisfy the public Rift length contract.
    #[test]
    fn default_config_is_valid() {
        assert!(validate_config(&ManagedRoomConfig::default()).is_ok());
    }

    /// Empty and oversized display names fail before any database work.
    #[test]
    fn invalid_display_names_are_rejected() {
        let empty_server = ManagedRoomConfig {
            server_name: String::new(),
            channel_name: "general".to_string(),
        };
        assert!(validate_config(&empty_server).is_err());

        let oversized_channel = ManagedRoomConfig {
            server_name: "Henosis".to_string(),
            channel_name: "x".repeat(101),
        };
        assert!(validate_config(&oversized_channel).is_err());
    }

    /// A live database imports once, leaves identities unowned, and preserves the winner.
    #[tokio::test]
    async fn live_initial_roster_import_is_atomic_and_idempotent() {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping live initial roster import test: HENOSIS_RIFT_TEST_DATABASE_URL is unset"
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
        let author = crate::db::create_user(
            &pool,
            &format!("import-owner-{}", &suffix[..10]),
            &format!("import-owner-{suffix}@example.invalid"),
            "unusable-test-hash",
            Some("Import Owner"),
        )
        .await
        .expect("revision author must be created");
        let first_agent = crate::db::create_agent_user(
            &pool,
            &format!("import-agent-a-{}", &suffix[..8]),
            &format!("import-agent-a-{suffix}@example.invalid"),
            "unusable-test-hash",
            Some("Imported Agent A"),
        )
        .await
        .expect("first imported identity must be created");
        let second_agent = crate::db::create_agent_user(
            &pool,
            &format!("import-agent-b-{}", &suffix[..8]),
            &format!("import-agent-b-{suffix}@example.invalid"),
            "unusable-test-hash",
            Some("Imported Agent B"),
        )
        .await
        .expect("second imported identity must be created");
        let server = crate::db::create_server(&pool, "Initial roster import test", None, author.id)
            .await
            .expect("test server must be created");

        let first = vec![imported_seat(first_agent.id, 0)];
        assert_eq!(
            import_initial_agent_roster(&pool, server.id, author.id, &first)
                .await
                .expect("initial import must commit"),
            InitialRosterImport::Imported { revision: 1 }
        );
        let roster = crate::db::agent_control::read_room_agent_roster(&pool, server.id)
            .await
            .expect("imported roster must be readable");
        assert_eq!(roster.desired_revision, Some(1));
        assert_eq!(roster.active_revision, Some(1));
        assert_eq!(roster.last_good_revision, Some(1));
        assert_eq!(
            roster.apply_state,
            crate::models::agent_control::ApplyState::Active
        );
        assert_eq!(roster.seats.len(), 1);
        assert_eq!(roster.seats[0].owner_user_id, None);
        assert_eq!(
            crate::db::agent_control::owner_for_agent(&pool, first_agent.id)
                .await
                .expect("ownership lookup must succeed"),
            None
        );

        let losing = vec![imported_seat(second_agent.id, 0)];
        assert_eq!(
            import_initial_agent_roster(&pool, server.id, author.id, &losing)
                .await
                .expect("repeat import must observe the winner"),
            InitialRosterImport::Existing {
                desired_revision: 1
            }
        );
        let preserved = crate::db::agent_control::read_room_agent_revision(&pool, server.id, 1)
            .await
            .expect("winning revision must remain readable");
        assert_eq!(preserved.len(), 1);
        assert_eq!(preserved[0].seat.agent_user_id, first_agent.id);

        sqlx::query("DELETE FROM servers WHERE id = $1")
            .bind(server.id)
            .execute(&pool)
            .await
            .expect("test server cleanup must succeed");
        for user_id in [first_agent.id, second_agent.id, author.id] {
            sqlx::query("DELETE FROM users WHERE id = $1")
                .bind(user_id)
                .execute(&pool)
                .await
                .expect("test user cleanup must succeed");
        }
    }
}
