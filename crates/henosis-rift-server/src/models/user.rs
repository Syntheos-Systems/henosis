use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// Full user row as stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    /// Unique user identifier.
    pub id: Uuid,
    /// Unique login handle chosen by the user.
    pub username: String,
    /// Optional display name shown in the UI instead of username.
    pub display_name: Option<String>,
    /// User's email address (not exposed in public responses).
    pub email: String,
    /// Argon2 password hash; excluded from serialized output.
    #[serde(skip_serializing)]
    pub password_hash: String,
    /// URL of the user's avatar image.
    pub avatar_url: Option<String>,
    /// Presence status string (e.g., "online", "idle", "dnd", "offline").
    pub status: String,
    /// Optional user bio / about text.
    pub about: Option<String>,
    /// Timestamp when the row was first created.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the most recent profile update.
    pub updated_at: DateTime<Utc>,
    /// Whether this user is an AI agent managed by the bridge.
    pub is_agent: bool,
    /// Which executor backend this agent uses (e.g., "ClaudeCode").
    pub executor_type: Option<String>,
    /// Bridge-assigned roster identifier for this agent.
    pub agent_roster_id: Option<String>,
}

/// Safe user representation for API responses (no password hash, no email to non-self).
#[derive(Debug, Serialize)]
pub struct PublicUser {
    /// Unique user identifier.
    pub id: Uuid,
    /// Unique login handle.
    pub username: String,
    /// Optional display name.
    pub display_name: Option<String>,
    /// URL of the user's avatar image.
    pub avatar_url: Option<String>,
    /// Presence status string.
    pub status: String,
    /// Optional user bio / about text.
    pub about: Option<String>,
    /// Whether this user is an AI agent.
    pub is_agent: bool,
}

impl From<User> for PublicUser {
    /// Convert a full User row into the public-facing representation.
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            username: u.username,
            display_name: u.display_name,
            avatar_url: u.avatar_url,
            status: u.status,
            about: u.about,
            is_agent: u.is_agent,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub refresh_token: String,
    pub user: PublicUser,
}
