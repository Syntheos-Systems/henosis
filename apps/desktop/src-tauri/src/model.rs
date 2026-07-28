//! Serialized contracts crossing the native Henosis webview boundary.

use serde::{Deserialize, Serialize};

/// Credentials accepted from the first-run GUI and never returned to it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiftConnectionInput {
    /// Rift HTTP or HTTPS base URL.
    pub endpoint: String,
    /// Rift login handle.
    pub username: String,
    /// Rift password retained only for the login request.
    pub password: String,
}

/// Non-secret connection fields safe to save in the application profile.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionProfile {
    /// Normalized Rift base URL.
    pub endpoint: String,
    /// Rift login handle.
    pub username: String,
}

/// Authenticated Rift identity safe to expose to the webview.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SanitizedConnection {
    /// Normalized Rift base URL.
    pub endpoint: String,
    /// Rift login handle.
    pub username: String,
    /// Stable Rift user identifier.
    pub user_id: String,
    /// Human-facing display name.
    pub display_name: String,
}

/// Provenance label that prevents cached data from masquerading as live.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DirectorySource {
    /// Data fetched from Rift during this operation.
    Live,
    /// Last known data loaded from the native cache.
    Cached,
}

/// Operational state shown for a room summary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoomStatus {
    /// No current bridge activity is known.
    Quiet,
    /// Recent messages indicate current room activity.
    Active,
    /// The server bridge is paused.
    Paused,
    /// Cached data is visible while Rift is unavailable.
    Disconnected,
    /// A linked approval is waiting on a human.
    AwaitingApproval,
}

/// Human or agent identity shown in room avatar stacks and search.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomParticipant {
    /// Stable Rift user identifier.
    pub id: String,
    /// Display name or login handle.
    pub display_name: String,
    /// Optional Rift avatar URL.
    pub avatar_url: Option<String>,
    /// True when Rift identifies the member as an agent.
    pub is_agent: bool,
    /// Current presence string supplied by Rift.
    pub presence: Option<String>,
}

/// Sanitized room data returned to the selector.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomSummary {
    /// Opaque Rift channel identifier.
    pub id: String,
    /// Channel name presented as the room name.
    pub name: String,
    /// Parent Rift server identifier.
    pub server_id: String,
    /// Parent Rift server name.
    pub server_name: Option<String>,
    /// Optional channel topic.
    pub topic: Option<String>,
    /// Latest message or a useful empty-room description.
    pub preview: String,
    /// Display name of the latest message author.
    pub latest_author: Option<String>,
    /// ISO timestamp for latest activity or channel creation.
    pub last_activity_at: String,
    /// Visible members of the parent server.
    pub participants: Vec<RoomParticipant>,
    /// Unread count when supported by the upstream contract.
    pub unread_count: u64,
    /// Current room status.
    pub status: RoomStatus,
    /// Optional active task or cascade summary.
    pub active_work: Option<String>,
    /// Number of approvals waiting on a person.
    pub pending_approvals: u64,
}

/// Complete room-directory result returned by a native command.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomDirectorySnapshot {
    /// Sanitized authenticated identity when a live session exists.
    pub connection: Option<SanitizedConnection>,
    /// Rooms sorted by descending activity.
    pub rooms: Vec<RoomSummary>,
    /// Whether data came from Rift or native cache.
    pub source: DirectorySource,
    /// ISO timestamp for the snapshot.
    pub fetched_at: String,
    /// True only while the native process retains an authenticated session.
    pub connected: bool,
}

/// Initial state used to choose setup or the room directory.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapResult {
    /// Saved non-secret fields used to prefill the setup form.
    pub saved_profile: Option<ConnectionProfile>,
    /// Live or cached room data when available.
    pub directory: Option<RoomDirectorySnapshot>,
    /// True when a person must authenticate before live refreshes.
    pub requires_authentication: bool,
}

/// Stable error categories rendered as actionable GUI states.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandErrorKind {
    /// Rift rejected the credentials or current access token.
    Authentication,
    /// No authenticated native Rift session exists.
    ConnectionRequired,
    /// Rift could not be reached.
    Network,
    /// Rift returned an unexpected or invalid response.
    Protocol,
    /// A local input failed validation.
    Validation,
    /// Native profile or cache storage failed.
    Storage,
}

/// Safe serialized error returned across the Tauri command boundary.
#[derive(Clone, Debug, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct CommandError {
    /// Machine-readable error category.
    pub kind: CommandErrorKind,
    /// Human-readable recovery guidance without secrets.
    pub message: String,
}

/// Construction helpers for consistent command errors.
impl CommandError {
    /// Create a new safe command error.
    pub fn new(kind: CommandErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
