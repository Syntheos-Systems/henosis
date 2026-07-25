//! Transactional bootstrap for the managed Henosis Rift room.

use argon2::{
    Argon2, PasswordHasher,
    password_hash::{SaltString, rand_core::OsRng},
};
use rand::Rng as _;
use sqlx::PgPool;
use uuid::Uuid;

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
    /// Rift server containing the persistent agent room.
    pub server_id: Uuid,
    /// Primary text channel observed by the bridge.
    pub channel_id: Uuid,
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
        server_id,
        channel_id,
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

/// Exercises database-independent managed room input validation.
#[cfg(test)]
mod tests {
    use super::{ManagedRoomConfig, validate_config};

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
}
