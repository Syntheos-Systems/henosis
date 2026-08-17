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

/// Effective Rift capabilities safe for React affordance decisions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomPermissions {
    /// Whether the signed-in human may create messages.
    pub send_messages: bool,
    /// Whether the signed-in human may send messages with attachments.
    pub attach_files: bool,
    /// Whether the signed-in human may delete another member's messages.
    pub manage_messages: bool,
    /// Whether the signed-in human may manage room-wide settings.
    pub manage_server: bool,
}

/// Native unread-divider placement derived from one bounded read cursor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RoomUnreadBoundary {
    /// No unread divider belongs in the loaded conversation window.
    None,
    /// Place the divider immediately before one loaded message.
    BeforeMessage {
        /// First loaded message that has not been marked read.
        message_id: String,
    },
    /// Label the loaded window without fetching unbounded older history.
    BeforeLoadedWindow,
}

/// Server-staged upload metadata that never includes a native file path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRoomAttachment {
    /// Opaque Rift upload identifier supplied when sending the message.
    pub upload_id: String,
    /// Original display filename without a local directory.
    pub filename: String,
    /// Optional declared media type.
    pub content_type: Option<String>,
    /// Uploaded byte count validated by the native client.
    pub size_bytes: u64,
}

/// Sanitized attachment metadata nested beneath a room message.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomAttachment {
    /// Stable Rift attachment identifier.
    pub id: String,
    /// Original display filename without a local directory.
    pub filename: String,
    /// Same-origin HTTP or HTTPS URL validated by the native client.
    pub url: String,
    /// Optional declared media type.
    pub content_type: Option<String>,
    /// Stored byte count when Rift supplied one.
    pub size_bytes: Option<u64>,
}

/// Sanitized room message shared by snapshots, commands, and native events.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomMessage {
    /// Stable Rift message identifier.
    pub id: String,
    /// Room identifier normalized from Rift's channel identifier.
    pub room_id: String,
    /// Stable Rift author identifier.
    pub author_id: String,
    /// Rift login handle retained for unambiguous identity.
    pub author_username: String,
    /// Optional human-facing author name.
    pub author_display_name: Option<String>,
    /// Optional Rift avatar URL.
    pub author_avatar_url: Option<String>,
    /// Message body, possibly empty for attachment-only messages.
    pub content: String,
    /// ISO timestamp of the latest edit when present.
    pub edited_at: Option<String>,
    /// ISO timestamp when Rift created the message.
    pub created_at: String,
    /// Rift message discriminator such as user, agent, stimulus, or system.
    pub message_type: String,
    /// Sanitized attachments in Rift response order.
    pub attachments: Vec<RoomAttachment>,
}

/// One oldest-first page of sanitized room messages.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    /// Messages ordered oldest to newest for direct timeline insertion.
    pub messages: Vec<RoomMessage>,
    /// Whether an explicit user action may request an older page.
    pub has_older: bool,
}

/// Native transport state for the currently open room.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RoomConnectionStatus {
    /// Native room setup or initial transport connection is underway.
    Connecting,
    /// HTTP reconciliation and the live gateway are available.
    Connected,
    /// Native transport is retrying after an interruption.
    Reconnecting,
    /// No live transport is currently available.
    Disconnected,
}

/// Complete sanitized native state returned when opening one room.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomConversationSnapshot {
    /// Open room identifier.
    pub room_id: String,
    /// Caller-generated one-use token bound to the exact native room generation.
    pub stream_id: String,
    /// Highest event sequence already reflected in this snapshot.
    pub last_event_sequence: u64,
    /// Signed-in Rift user used for authorship affordances.
    pub current_user_id: String,
    /// Server-authoritative capabilities for the signed-in human.
    pub permissions: RoomPermissions,
    /// Placement of the bounded unread divider.
    pub unread_boundary: RoomUnreadBoundary,
    /// Initial oldest-first live message window.
    pub page: MessagePage,
    /// Current native transport state.
    pub connection_status: RoomConnectionStatus,
}

/// Sanitized incremental updates emitted on the fixed native room event channel.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RoomConversationEvent {
    /// Insert or idempotently replace one newly created message.
    MessageCreate {
        /// Room receiving the message.
        room_id: String,
        /// Complete sanitized message.
        message: RoomMessage,
    },
    /// Apply the editable fields supplied by Rift's update event.
    MessageUpdate {
        /// Room containing the edited message.
        room_id: String,
        /// Edited message identifier.
        message_id: String,
        /// Replacement message body.
        content: String,
        /// ISO timestamp of the edit.
        edited_at: String,
    },
    /// Remove one message by identifier.
    MessageDelete {
        /// Room containing the deleted message.
        room_id: String,
        /// Deleted message identifier.
        message_id: String,
    },
    /// Start or refresh one user's short-lived typing indicator.
    TypingStart {
        /// Room receiving the typing signal.
        room_id: String,
        /// Typing Rift user identifier.
        user_id: String,
        /// Typing user's Rift login handle.
        username: String,
    },
    /// Replace one participant's presence state in the open room.
    PresenceUpdate {
        /// Open room whose participant view should change.
        room_id: String,
        /// Rift user whose presence changed.
        user_id: String,
        /// Sanitized Rift presence value.
        status: String,
    },
    /// Report native upload progress without exposing a selected path.
    UploadProgress {
        /// Room receiving the staged upload.
        room_id: String,
        /// Native opaque identifier for this transfer.
        transfer_id: String,
        /// Display filename without its local directory.
        filename: String,
        /// Number of bytes accepted by the native transport.
        bytes_sent: u64,
        /// Total validated size of the selected file.
        total_bytes: u64,
    },
    /// Replace the visible native connection indicator.
    ConnectionChanged {
        /// Open room affected by the transport change.
        room_id: String,
        /// Current native transport state.
        status: RoomConnectionStatus,
    },
    /// Apply a bounded reconnect page or replace the live window after fallback.
    Reconciliation {
        /// Open room receiving reconciled messages.
        room_id: String,
        /// Oldest-first messages returned by the bounded algorithm.
        page: MessagePage,
        /// True when React must replace rather than merge its live window.
        replace_live_window: bool,
    },
}

/// One generation-scoped, monotonically ordered event crossing the native webview boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomConversationEventEnvelope {
    /// Caller-generated one-use token shared with its opening snapshot.
    pub stream_id: String,
    /// Strictly increasing sequence within this room generation.
    pub sequence: u64,
    /// Sanitized room update applied to native state before delivery.
    pub event: RoomConversationEvent,
}

/// One command value ordered on the same stream sequence as native room events.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomConversationCommandResult<T> {
    /// Caller-generated one-use token shared with the opening snapshot and events.
    pub stream_id: String,
    /// Strictly increasing sequence within this room generation.
    pub sequence: u64,
    /// Sanitized command value committed to native state at this sequence.
    pub value: T,
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

/// Persistent agent identity owned by the signed-in Rift human.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnedAgentIdentity {
    /// Stable Rift user identifier for the agent.
    pub id: String,
    /// Unique Rift username.
    pub username: String,
    /// Optional human-facing display name.
    pub display_name: Option<String>,
    /// Stable Rift user identifier for the owning human.
    pub owner_user_id: String,
}

/// Imported persistent agent identity that has not been claimed by a human.
///
/// Rift exposes these identities through roster context rather than a global
/// collection endpoint, so the native type remains available to roster views
/// without broadening the public identity-discovery boundary.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnownedAgentIdentity {
    /// Stable Rift user identifier for the agent.
    pub id: String,
    /// Unique Rift username.
    pub username: String,
    /// Optional human-facing display name.
    pub display_name: Option<String>,
}

/// Credential selection behavior declared by one host harness.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HarnessCredentialMode {
    /// Use only an authenticated session already present on the host.
    HostSession,
    /// Allow either a host session or an opaque deployment-owned binding.
    OptionalBinding,
    /// Require an opaque deployment-owned binding.
    RequiredBinding,
}

/// One selectable model reported by a host execution harness.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapability {
    /// Stable catalog identifier submitted with a roster update.
    pub id: String,
    /// Human-facing model name.
    pub label: String,
    /// Whether this deployment can currently use the model.
    pub available: bool,
    /// Safe deployment-supplied explanation when unavailable.
    pub unavailable_reason: Option<String>,
}

/// One selectable value for a catalog-defined setting.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingCapabilityOption {
    /// Stable non-secret value submitted to Rift.
    pub id: String,
    /// Human-facing option name.
    pub label: String,
}

/// Typed control metadata for one catalog-defined non-secret setting.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SettingCapabilityControl {
    /// Choose one value from a finite deployment-supplied list.
    Select {
        /// Allowed values in display order.
        options: Vec<SettingCapabilityOption>,
    },
    /// Choose a bounded stepped integer.
    Integer {
        /// Inclusive lower bound.
        minimum: i64,
        /// Inclusive upper bound.
        maximum: i64,
        /// Positive increment measured from the lower bound.
        step: i64,
    },
    /// Choose an enabled or disabled value.
    Boolean,
}

/// One typed non-secret setting exposed by a host harness.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingCapability {
    /// Stable key used in the seat settings object.
    pub id: String,
    /// Human-facing setting name.
    pub label: String,
    /// Whether each seat must submit a value.
    pub required: bool,
    /// Control type and validation constraints.
    pub control: SettingCapabilityControl,
}

/// One execution harness and its deployment-discovered choices.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCapability {
    /// Stable harness identifier submitted with a roster update.
    pub id: String,
    /// Human-facing harness name.
    pub label: String,
    /// Whether this deployment can currently use the harness.
    pub available: bool,
    /// Safe deployment-supplied explanation when unavailable.
    pub unavailable_reason: Option<String>,
    /// Supported credential selection behavior.
    pub credential_mode: HarnessCredentialMode,
    /// Models currently declared by this harness.
    pub models: Vec<ModelCapability>,
    /// Typed non-secret settings currently declared by this harness.
    pub settings: Vec<SettingCapability>,
}

/// Generation-stamped execution catalog discovered from the connected host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilityCatalog {
    /// Opaque generation that changes whenever host discovery is rebuilt.
    pub generation: String,
    /// Deployment-discovered harnesses without a client-side allowlist.
    pub harnesses: Vec<HarnessCapability>,
}

/// Opaque readiness of the host or deployment-owned credential binding.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentCredentialReadiness {
    /// The harness will use an authenticated host session.
    HostSession,
    /// The opaque binding is currently usable.
    Ready,
    /// No usable host session or binding is available.
    Unavailable,
    /// The binding needs human intervention outside the dashboard.
    Attention,
}

/// Durable activation state for the desired room roster revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeActivationState {
    /// No managed revision has been requested.
    Idle,
    /// A desired revision is waiting for reconciliation.
    Pending,
    /// The desired revision is running and proven good.
    Active,
    /// The desired revision failed validation, preflight, or startup.
    Failed,
}

/// One server-authoritative agent seat safe to expose to the dashboard.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSeatSnapshot {
    /// Stable seat identifier that survives reordering.
    pub seat_id: String,
    /// Persistent Rift agent identity occupying the seat.
    pub agent_identity_id: String,
    /// Unique public username of the selected agent identity.
    pub agent_username: String,
    /// Optional human-facing name of the selected agent identity.
    pub agent_display_name: Option<String>,
    /// Current human owner, or none for an imported unclaimed identity.
    pub owner_human_id: Option<String>,
    /// Deployment-discovered execution harness key.
    pub harness_key: String,
    /// Deployment-discovered model key beneath the harness.
    pub model_key: String,
    /// Typed non-secret settings validated by Rift against the catalog.
    pub settings: serde_json::Value,
    /// Opaque deployment-owned credential binding identifier, when selected.
    pub credential_binding_id: Option<String>,
    /// Whether this seat participates in room responses.
    pub enabled: bool,
    /// Non-negative room display and execution order.
    pub position: i32,
    /// Desired immutable roster revision containing this seat.
    pub configuration_revision: Option<i64>,
    /// Credential usability without credential contents or host locators.
    pub credential_readiness: AgentCredentialReadiness,
    /// Current activation state of the containing roster revision.
    pub runtime_activation: RuntimeActivationState,
}

/// Complete server-authoritative room roster and asynchronous activation status.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRosterSnapshot {
    /// Rift server whose bridge the roster configures.
    pub server_id: String,
    /// Latest desired immutable revision.
    pub desired_revision: Option<i64>,
    /// Revision currently running in the bridge.
    pub active_revision: Option<i64>,
    /// Most recent revision proven to start successfully.
    pub last_good_revision: Option<i64>,
    /// Current asynchronous activation state.
    pub runtime_activation: RuntimeActivationState,
    /// Stable activation failure code, when the desired revision failed.
    pub runtime_error_code: Option<String>,
    /// Bounded safe activation failure detail.
    pub runtime_error_message: Option<String>,
    /// Desired seats in server-authoritative position order.
    pub seats: Vec<AgentSeatSnapshot>,
}

/// One editable non-secret seat in a whole-roster replacement.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSeatDraft {
    /// Stable seat identifier retained across edits and reordering.
    pub seat_id: String,
    /// Persistent Rift agent identity assigned to the seat.
    pub agent_identity_id: String,
    /// Deployment-discovered execution harness key.
    pub harness_key: String,
    /// Deployment-discovered model key beneath the harness.
    pub model_key: String,
    /// Typed non-secret settings validated by Rift against the live catalog.
    pub settings: serde_json::Value,
    /// Opaque deployment-owned credential binding identifier, when selected.
    pub credential_binding_id: Option<String>,
    /// Whether this seat participates in room responses.
    pub enabled: bool,
    /// Non-negative room display and execution order.
    pub position: i32,
}

/// Optimistic whole-roster replacement submitted by the dashboard.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplyAgentRosterRequest {
    /// Desired revision observed before the local draft was edited.
    pub expected_revision: Option<i64>,
    /// Complete next roster, never a partial patch.
    pub seats: Vec<AgentSeatDraft>,
}

/// Public pause and activation state for one room bridge.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomBridgeStatus {
    /// Whether autonomous bridge activity is paused.
    pub paused: bool,
    /// Latest desired immutable revision.
    pub desired_revision: Option<i64>,
    /// Revision currently running in the bridge.
    pub active_revision: Option<i64>,
    /// Most recent revision proven to start successfully.
    pub last_good_revision: Option<i64>,
    /// Current asynchronous activation state.
    pub runtime_activation: RuntimeActivationState,
    /// Stable activation failure code, when the desired revision failed.
    pub runtime_error_code: Option<String>,
    /// Bounded safe activation failure detail.
    pub runtime_error_message: Option<String>,
}

/// Stable dashboard error categories used for recovery-oriented UI behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DashboardErrorKind {
    /// Rift rejected or expired the authenticated native session.
    Authentication,
    /// No authenticated native Rift session exists.
    ConnectionRequired,
    /// Rift could not be reached.
    Network,
    /// The submitted dashboard data failed validation.
    Validation,
    /// The requested state conflicts with current server state.
    Conflict,
    /// The signed-in human lacks authority for the operation.
    Forbidden,
    /// A runtime capability or opaque credential is unavailable.
    Unavailable,
    /// Rift returned data outside the supported dashboard contract.
    Protocol,
    /// Native session state could not be read safely.
    Storage,
}

/// Safe code-aware failure returned by dashboard Tauri commands.
#[derive(Clone, Debug, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct DashboardCommandError {
    /// Recovery category used by the dashboard without parsing text.
    pub kind: DashboardErrorKind,
    /// Stable machine-readable local or Rift error code.
    pub code: String,
    /// Human-readable recovery guidance without secrets or native paths.
    pub message: String,
}

/// Construction helpers for consistent dashboard command failures.
impl DashboardCommandError {
    /// Create one safe dashboard error with an explicit stable code.
    pub fn new(
        kind: DashboardErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
        }
    }
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

#[cfg(test)]
/// Exact serialization tests for the native room conversation boundary.
mod tests {
    use serde_json::json;

    use super::*;

    /// Construct one representative sanitized room message.
    fn room_message() -> RoomMessage {
        RoomMessage {
            id: "message-1".into(),
            room_id: "room-1".into(),
            author_id: "agent-1".into(),
            author_username: "cartographer".into(),
            author_display_name: Some("Cartographer".into()),
            author_avatar_url: None,
            content: "Mapped the next ridge.".into(),
            edited_at: None,
            created_at: "2026-08-02T12:00:00Z".into(),
            message_type: "agent".into(),
            attachments: vec![RoomAttachment {
                id: "attachment-1".into(),
                filename: "ridge.txt".into(),
                url: "https://rift.example/uploads/opaque".into(),
                content_type: Some("text/plain".into()),
                size_bytes: Some(128),
            }],
        }
    }

    /// Construct one dynamic harness with every supported setting control.
    fn dashboard_harness() -> HarnessCapability {
        HarnessCapability {
            id: "codex-cli".into(),
            label: "Codex CLI".into(),
            available: true,
            unavailable_reason: None,
            credential_mode: HarnessCredentialMode::HostSession,
            models: vec![ModelCapability {
                id: "gpt-5.6-sol".into(),
                label: "GPT-5.6 Sol".into(),
                available: true,
                unavailable_reason: None,
            }],
            settings: vec![
                SettingCapability {
                    id: "effort".into(),
                    label: "Reasoning effort".into(),
                    required: true,
                    control: SettingCapabilityControl::Select {
                        options: vec![SettingCapabilityOption {
                            id: "medium".into(),
                            label: "Medium".into(),
                        }],
                    },
                },
                SettingCapability {
                    id: "turnLimit".into(),
                    label: "Turn limit".into(),
                    required: false,
                    control: SettingCapabilityControl::Integer {
                        minimum: 1,
                        maximum: 20,
                        step: 1,
                    },
                },
                SettingCapability {
                    id: "webSearch".into(),
                    label: "Web search".into(),
                    required: false,
                    control: SettingCapabilityControl::Boolean,
                },
            ],
        }
    }

    /// Construct one editable dashboard seat with only catalog-defined settings.
    fn dashboard_seat_draft() -> AgentSeatDraft {
        AgentSeatDraft {
            seat_id: "seat-1".into(),
            agent_identity_id: "agent-1".into(),
            harness_key: "codex-cli".into(),
            model_key: "gpt-5.6-sol".into(),
            settings: json!({ "effort": "medium", "turnLimit": 8, "webSearch": false }),
            credential_binding_id: None,
            enabled: true,
            position: 0,
        }
    }

    /// Construct one failed roster snapshot with opaque readiness and revision state.
    fn dashboard_failed_roster() -> AgentRosterSnapshot {
        AgentRosterSnapshot {
            server_id: "server-1".into(),
            desired_revision: Some(7),
            active_revision: Some(6),
            last_good_revision: Some(6),
            runtime_activation: RuntimeActivationState::Failed,
            runtime_error_code: Some("process_exited".into()),
            runtime_error_message: Some("Agent process exited during startup.".into()),
            seats: vec![AgentSeatSnapshot {
                seat_id: "seat-1".into(),
                agent_identity_id: "agent-1".into(),
                agent_username: "cartographer".into(),
                agent_display_name: Some("Cartographer".into()),
                owner_human_id: Some("human-1".into()),
                harness_key: "codex-cli".into(),
                model_key: "gpt-5.6-sol".into(),
                settings: json!({ "effort": "medium" }),
                credential_binding_id: None,
                enabled: true,
                position: 0,
                configuration_revision: Some(7),
                credential_readiness: AgentCredentialReadiness::HostSession,
                runtime_activation: RuntimeActivationState::Failed,
            }],
        }
    }

    /// Dashboard identities and dynamic catalog serialize as stable secret-free contracts.
    #[test]
    fn dashboard_identities_and_capabilities_serialize_stably() {
        let owned = OwnedAgentIdentity {
            id: "agent-1".into(),
            username: "cartographer".into(),
            display_name: Some("Cartographer".into()),
            owner_user_id: "human-1".into(),
        };
        let unowned = UnownedAgentIdentity {
            id: "agent-legacy".into(),
            username: "legacy-scout".into(),
            display_name: None,
        };
        let catalog = AgentCapabilityCatalog {
            generation: "catalog-7".into(),
            harnesses: vec![dashboard_harness()],
        };

        assert_eq!(
            serde_json::to_value(&owned).expect("owned identity must serialize"),
            json!({
                "id": "agent-1",
                "username": "cartographer",
                "displayName": "Cartographer",
                "ownerUserId": "human-1",
            })
        );
        assert_eq!(
            serde_json::to_value(&unowned).expect("unowned identity must serialize"),
            json!({
                "id": "agent-legacy",
                "username": "legacy-scout",
                "displayName": null,
            })
        );
        assert_eq!(
            serde_json::to_value(&catalog).expect("catalog must serialize"),
            json!({
                "generation": "catalog-7",
                "harnesses": [{
                    "id": "codex-cli",
                    "label": "Codex CLI",
                    "available": true,
                    "unavailableReason": null,
                    "credentialMode": "hostSession",
                    "models": [{
                        "id": "gpt-5.6-sol",
                        "label": "GPT-5.6 Sol",
                        "available": true,
                        "unavailableReason": null,
                    }],
                    "settings": [
                        {
                            "id": "effort",
                            "label": "Reasoning effort",
                            "required": true,
                            "control": {
                                "type": "select",
                                "options": [{ "id": "medium", "label": "Medium" }],
                            },
                        },
                        {
                            "id": "turnLimit",
                            "label": "Turn limit",
                            "required": false,
                            "control": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 20,
                                "step": 1,
                            },
                        },
                        {
                            "id": "webSearch",
                            "label": "Web search",
                            "required": false,
                            "control": { "type": "boolean" },
                        },
                    ],
                }],
            })
        );
    }

    /// Dashboard roster, update, and bridge state retain revisions and runtime failure detail.
    #[test]
    fn dashboard_read_context_roster_update_and_runtime_failure_serialize_stably() {
        let roster = dashboard_failed_roster();
        let update = ApplyAgentRosterRequest {
            expected_revision: Some(7),
            seats: vec![dashboard_seat_draft()],
        };
        let status = RoomBridgeStatus {
            paused: false,
            desired_revision: Some(7),
            active_revision: Some(6),
            last_good_revision: Some(6),
            runtime_activation: RuntimeActivationState::Failed,
            runtime_error_code: Some("process_exited".into()),
            runtime_error_message: Some("Agent process exited during startup.".into()),
        };

        assert_eq!(
            serde_json::to_value(&roster).expect("roster must serialize"),
            json!({
                "serverId": "server-1",
                "desiredRevision": 7,
                "activeRevision": 6,
                "lastGoodRevision": 6,
                "runtimeActivation": "failed",
                "runtimeErrorCode": "process_exited",
                "runtimeErrorMessage": "Agent process exited during startup.",
                "seats": [{
                    "seatId": "seat-1",
                    "agentIdentityId": "agent-1",
                    "agentUsername": "cartographer",
                    "agentDisplayName": "Cartographer",
                    "ownerHumanId": "human-1",
                    "harnessKey": "codex-cli",
                    "modelKey": "gpt-5.6-sol",
                    "settings": { "effort": "medium" },
                    "credentialBindingId": null,
                    "enabled": true,
                    "position": 0,
                    "configurationRevision": 7,
                    "credentialReadiness": "hostSession",
                    "runtimeActivation": "failed",
                }],
            })
        );
        assert_eq!(
            serde_json::to_value(&update).expect("roster update must serialize"),
            json!({
                "expectedRevision": 7,
                "seats": [{
                    "seatId": "seat-1",
                    "agentIdentityId": "agent-1",
                    "harnessKey": "codex-cli",
                    "modelKey": "gpt-5.6-sol",
                    "settings": {
                        "effort": "medium",
                        "turnLimit": 8,
                        "webSearch": false,
                    },
                    "credentialBindingId": null,
                    "enabled": true,
                    "position": 0,
                }],
            })
        );
        assert_eq!(
            serde_json::to_value(&status).expect("bridge status must serialize"),
            json!({
                "paused": false,
                "desiredRevision": 7,
                "activeRevision": 6,
                "lastGoodRevision": 6,
                "runtimeActivation": "failed",
                "runtimeErrorCode": "process_exited",
                "runtimeErrorMessage": "Agent process exited during startup.",
            })
        );
    }

    /// Dashboard success and failure bodies contain no credential values or native paths.
    #[test]
    fn dashboard_serialized_contracts_exclude_secrets_and_native_paths() {
        let created = OwnedAgentIdentity {
            id: "agent-created".into(),
            username: "builder".into(),
            display_name: Some("Builder".into()),
            owner_user_id: "human-1".into(),
        };
        let claimed = OwnedAgentIdentity {
            id: "agent-legacy".into(),
            username: "legacy-scout".into(),
            display_name: None,
            owner_user_id: "human-1".into(),
        };
        let errors = [
            DashboardCommandError::new(
                DashboardErrorKind::Conflict,
                "revision_conflict",
                "Room roster changed at revision 8.",
            ),
            DashboardCommandError::new(
                DashboardErrorKind::Validation,
                "bad_request",
                "Choose a valid agent identity.",
            ),
            DashboardCommandError::new(
                DashboardErrorKind::Forbidden,
                "forbidden",
                "You cannot change this room roster.",
            ),
            DashboardCommandError::new(
                DashboardErrorKind::Unavailable,
                "credential_not_ready",
                "The selected credential binding needs attention.",
            ),
        ];
        let encoded = serde_json::to_string(&(
            created,
            claimed,
            AgentCapabilityCatalog {
                generation: "catalog-7".into(),
                harnesses: vec![dashboard_harness()],
            },
            dashboard_failed_roster(),
            ApplyAgentRosterRequest {
                expected_revision: Some(7),
                seats: vec![dashboard_seat_draft()],
            },
            errors,
        ))
        .expect("dashboard contracts must serialize");

        for forbidden in [
            "accessToken",
            "refreshToken",
            "credentialValue",
            "environmentValue",
            "executablePath",
            "commandTemplate",
            "localPath",
            "/native/",
        ] {
            assert!(!encoded.contains(forbidden));
        }
    }

    /// Representative snapshots use camel-case fields and contain no secret-bearing keys.
    #[test]
    fn room_snapshot_serializes_sanitized_camel_case_contract() {
        let snapshot = RoomConversationSnapshot {
            room_id: "room-1".into(),
            stream_id: "stream-0000000007".into(),
            last_event_sequence: 12,
            current_user_id: "user-1".into(),
            permissions: RoomPermissions {
                send_messages: true,
                attach_files: true,
                manage_messages: false,
                manage_server: false,
            },
            unread_boundary: RoomUnreadBoundary::BeforeMessage {
                message_id: "message-1".into(),
            },
            page: MessagePage {
                messages: vec![room_message()],
                has_older: true,
            },
            connection_status: RoomConnectionStatus::Connected,
        };

        let value = serde_json::to_value(&snapshot).expect("snapshot must serialize");
        assert_eq!(
            value,
            json!({
                "roomId": "room-1",
                "streamId": "stream-0000000007",
                "lastEventSequence": 12,
                "currentUserId": "user-1",
                "permissions": {
                    "sendMessages": true,
                    "attachFiles": true,
                    "manageMessages": false,
                    "manageServer": false,
                },
                "unreadBoundary": {
                    "kind": "beforeMessage",
                    "messageId": "message-1",
                },
                "page": {
                    "messages": [{
                        "id": "message-1",
                        "roomId": "room-1",
                        "authorId": "agent-1",
                        "authorUsername": "cartographer",
                        "authorDisplayName": "Cartographer",
                        "authorAvatarUrl": null,
                        "content": "Mapped the next ridge.",
                        "editedAt": null,
                        "createdAt": "2026-08-02T12:00:00Z",
                        "messageType": "agent",
                        "attachments": [{
                            "id": "attachment-1",
                            "filename": "ridge.txt",
                            "url": "https://rift.example/uploads/opaque",
                            "contentType": "text/plain",
                            "sizeBytes": 128,
                        }],
                    }],
                    "hasOlder": true,
                },
                "connectionStatus": "connected",
            })
        );
        let encoded = serde_json::to_string(&snapshot).expect("snapshot must encode");
        for forbidden in [
            "accessToken",
            "refreshToken",
            "credentialBinding",
            "localPath",
            "/native/",
        ] {
            assert!(!encoded.contains(forbidden));
        }
    }

    /// Every unread placement state has a stable tagged JSON shape.
    #[test]
    fn room_unread_boundaries_serialize_stably() {
        let cases = [
            (RoomUnreadBoundary::None, json!({ "kind": "none" })),
            (
                RoomUnreadBoundary::BeforeMessage {
                    message_id: "message-9".into(),
                },
                json!({ "kind": "beforeMessage", "messageId": "message-9" }),
            ),
            (
                RoomUnreadBoundary::BeforeLoadedWindow,
                json!({ "kind": "beforeLoadedWindow" }),
            ),
        ];

        for (boundary, expected) in cases {
            assert_eq!(
                serde_json::to_value(boundary).expect("boundary must serialize"),
                expected
            );
        }
    }

    /// Pending uploads expose an opaque identifier and display-safe metadata only.
    #[test]
    fn room_pending_attachment_exposes_only_safe_metadata() {
        let pending = PendingRoomAttachment {
            upload_id: "upload-opaque".into(),
            filename: "diagram.png".into(),
            content_type: Some("image/png".into()),
            size_bytes: 4096,
        };

        assert_eq!(
            serde_json::to_value(pending).expect("pending attachment must serialize"),
            json!({
                "uploadId": "upload-opaque",
                "filename": "diagram.png",
                "contentType": "image/png",
                "sizeBytes": 4096,
            })
        );
    }

    /// Every room event variant round-trips through its exact tagged JSON contract.
    #[test]
    fn room_events_serialize_every_variant() {
        let cases = vec![
            (
                RoomConversationEvent::MessageCreate {
                    room_id: "room-1".into(),
                    message: room_message(),
                },
                json!({
                    "type": "messageCreate",
                    "data": { "roomId": "room-1", "message": room_message() },
                }),
            ),
            (
                RoomConversationEvent::MessageUpdate {
                    room_id: "room-1".into(),
                    message_id: "message-1".into(),
                    content: "Mapped the lower ridge.".into(),
                    edited_at: "2026-08-02T12:01:00Z".into(),
                },
                json!({
                    "type": "messageUpdate",
                    "data": {
                        "roomId": "room-1",
                        "messageId": "message-1",
                        "content": "Mapped the lower ridge.",
                        "editedAt": "2026-08-02T12:01:00Z",
                    },
                }),
            ),
            (
                RoomConversationEvent::MessageDelete {
                    room_id: "room-1".into(),
                    message_id: "message-1".into(),
                },
                json!({
                    "type": "messageDelete",
                    "data": { "roomId": "room-1", "messageId": "message-1" },
                }),
            ),
            (
                RoomConversationEvent::TypingStart {
                    room_id: "room-1".into(),
                    user_id: "user-2".into(),
                    username: "operator".into(),
                },
                json!({
                    "type": "typingStart",
                    "data": {
                        "roomId": "room-1",
                        "userId": "user-2",
                        "username": "operator",
                    },
                }),
            ),
            (
                RoomConversationEvent::PresenceUpdate {
                    room_id: "room-1".into(),
                    user_id: "user-2".into(),
                    status: "online".into(),
                },
                json!({
                    "type": "presenceUpdate",
                    "data": {
                        "roomId": "room-1",
                        "userId": "user-2",
                        "status": "online",
                    },
                }),
            ),
            (
                RoomConversationEvent::UploadProgress {
                    room_id: "room-1".into(),
                    transfer_id: "transfer-1".into(),
                    filename: "diagram.png".into(),
                    bytes_sent: 2048,
                    total_bytes: 4096,
                },
                json!({
                    "type": "uploadProgress",
                    "data": {
                        "roomId": "room-1",
                        "transferId": "transfer-1",
                        "filename": "diagram.png",
                        "bytesSent": 2048,
                        "totalBytes": 4096,
                    },
                }),
            ),
            (
                RoomConversationEvent::ConnectionChanged {
                    room_id: "room-1".into(),
                    status: RoomConnectionStatus::Reconnecting,
                },
                json!({
                    "type": "connectionChanged",
                    "data": { "roomId": "room-1", "status": "reconnecting" },
                }),
            ),
            (
                RoomConversationEvent::Reconciliation {
                    room_id: "room-1".into(),
                    page: MessagePage {
                        messages: vec![room_message()],
                        has_older: false,
                    },
                    replace_live_window: true,
                },
                json!({
                    "type": "reconciliation",
                    "data": {
                        "roomId": "room-1",
                        "page": { "messages": [room_message()], "hasOlder": false },
                        "replaceLiveWindow": true,
                    },
                }),
            ),
        ];

        for (event, expected) in cases {
            let value = serde_json::to_value(&event).expect("event must serialize");
            assert_eq!(value, expected);
            assert_eq!(
                serde_json::from_value::<RoomConversationEvent>(value)
                    .expect("event must deserialize"),
                event
            );
        }
    }

    /// Event envelopes carry only the ordering cursor and the existing sanitized event contract.
    #[test]
    fn room_event_envelope_serializes_ordering_contract() {
        let envelope = RoomConversationEventEnvelope {
            stream_id: "stream-0000000007".into(),
            sequence: 13,
            event: RoomConversationEvent::MessageDelete {
                room_id: "room-1".into(),
                message_id: "message-1".into(),
            },
        };

        assert_eq!(
            serde_json::to_value(&envelope).expect("event envelope must serialize"),
            json!({
                "streamId": "stream-0000000007",
                "sequence": 13,
                "event": {
                    "type": "messageDelete",
                    "data": { "roomId": "room-1", "messageId": "message-1" },
                },
            })
        );
    }

    /// Command results expose their sanitized value on the same stream ordering contract.
    #[test]
    fn room_command_result_serializes_ordering_contract() {
        let result = RoomConversationCommandResult {
            stream_id: "stream-0000000007".into(),
            sequence: 14,
            value: Some(room_message()),
        };

        assert_eq!(
            serde_json::to_value(&result).expect("command result must serialize"),
            json!({
                "streamId": "stream-0000000007",
                "sequence": 14,
                "value": room_message(),
            })
        );
    }

    /// Connection states remain compact string values for React status rendering.
    #[test]
    fn room_connection_statuses_serialize_stably() {
        assert_eq!(
            serde_json::to_value([
                RoomConnectionStatus::Connecting,
                RoomConnectionStatus::Connected,
                RoomConnectionStatus::Reconnecting,
                RoomConnectionStatus::Disconnected,
            ])
            .expect("connection states must serialize"),
            json!(["connecting", "connected", "reconnecting", "disconnected"])
        );
    }
}
