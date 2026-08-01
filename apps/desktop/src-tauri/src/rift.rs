//! Native Rift HTTP client and room-summary aggregation.

use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::Duration as StdDuration;
use url::Url;

use crate::model::{
    CommandError, CommandErrorKind, ConnectionProfile, DirectorySource, RiftConnectionInput,
    RoomDirectorySnapshot, RoomParticipant, RoomStatus, RoomSummary, SanitizedConnection,
};

/// Access and refresh tokens retained only inside the native process.
#[derive(Clone)]
pub struct RiftSession {
    /// Normalized Rift base URL.
    base_url: Url,
    /// Short-lived Rift access token.
    access_token: String,
    /// Long-lived refresh token reserved for the later refresh loop.
    _refresh_token: String,
    /// Sanitized authenticated identity.
    connection: SanitizedConnection,
}

/// Internal Rift client failure before conversion into a safe command error.
#[derive(Debug, thiserror::Error)]
pub enum RiftError {
    /// Endpoint failed local validation.
    #[error("{0}")]
    Validation(String),
    /// Rift rejected credentials or an access token.
    #[error("Rift rejected the current credentials.")]
    Authentication,
    /// HTTP transport could not reach Rift.
    #[error("Rift could not be reached.")]
    Network(#[source] reqwest::Error),
    /// Rift returned an error outside the authentication contract.
    #[error("Rift returned {status}: {message}")]
    Remote {
        /// HTTP status returned by Rift.
        status: StatusCode,
        /// Safe error body supplied by Rift.
        message: String,
    },
    /// Rift returned JSON outside the expected contract.
    #[error("Rift returned data Henosis could not understand.")]
    Protocol(#[source] reqwest::Error),
}

/// Map native Rift failures into stable webview-safe error categories.
impl From<RiftError> for CommandError {
    /// Convert a Rift failure without including URLs, credentials, or response internals.
    fn from(error: RiftError) -> Self {
        match error {
            RiftError::Validation(message) => Self::new(CommandErrorKind::Validation, message),
            RiftError::Authentication => Self::new(
                CommandErrorKind::Authentication,
                "Rift did not accept that username and password.",
            ),
            RiftError::Network(_) => Self::new(
                CommandErrorKind::Network,
                "Henosis could not reach Rift. Check the endpoint and service status.",
            ),
            RiftError::Remote { status, message } => Self::new(
                CommandErrorKind::Protocol,
                format!("Rift returned {status}: {message}"),
            ),
            RiftError::Protocol(_) => Self::new(
                CommandErrorKind::Protocol,
                "Rift returned data this Henosis build does not understand.",
            ),
        }
    }
}

/// Rift login request body.
#[derive(Serialize)]
struct LoginRequest<'a> {
    /// Rift login handle.
    username: &'a str,
    /// Rift password.
    password: &'a str,
}

/// Public Rift user returned by login and membership routes.
#[derive(Clone, Deserialize)]
struct ApiUser {
    /// Stable user identifier.
    id: String,
    /// Rift login handle.
    username: String,
    /// Optional human-facing display name.
    display_name: Option<String>,
    /// Optional Rift avatar URL.
    avatar_url: Option<String>,
    /// Presence state.
    status: String,
    /// Whether this identity belongs to an agent.
    is_agent: bool,
}

/// Rift login response retained only long enough to construct native state.
#[derive(Deserialize)]
struct LoginResponse {
    /// Access token used for authenticated Rift requests.
    token: String,
    /// Refresh token retained inside native session state.
    refresh_token: String,
    /// Authenticated public identity.
    user: ApiUser,
}

/// Rift server row returned to a member.
#[derive(Deserialize)]
struct ApiServer {
    /// Stable server identifier.
    id: String,
    /// Human-facing server name.
    name: String,
}

/// Rift channel row mapped into a user-facing room.
#[derive(Deserialize)]
struct ApiChannel {
    /// Stable channel identifier.
    id: String,
    /// Channel name.
    name: String,
    /// Optional channel topic.
    topic: Option<String>,
    /// Rift channel kind.
    channel_type: String,
    /// Creation timestamp used when no messages exist.
    created_at: DateTime<Utc>,
}

/// Rift member response enriched with its public user.
#[derive(Deserialize)]
struct ApiMember {
    /// Public Rift user.
    user: ApiUser,
}

/// Rift attachment shape needed only to describe attachment-only messages.
#[derive(Deserialize)]
struct ApiAttachment {
    /// Original filename.
    filename: String,
}

/// Latest Rift message used to populate one room summary.
#[derive(Deserialize)]
struct ApiMessage {
    /// Text content, possibly empty for attachment-only messages.
    content: String,
    /// Message timestamp used as room activity.
    created_at: DateTime<Utc>,
    /// Author login handle.
    author_username: String,
    /// Optional author display name.
    author_display_name: Option<String>,
    /// Attached files.
    attachments: Vec<ApiAttachment>,
}

/// Rift bridge state used to identify paused rooms.
#[derive(Deserialize)]
struct ApiBridgeStatus {
    /// Whether autonomous bridge activity is paused.
    paused: bool,
}

/// Rift JSON error body.
#[derive(Deserialize)]
struct ApiError {
    /// Safe error message from the Rift server.
    error: String,
}

/// Build a Rift client whose network operations cannot leave the GUI busy forever.
fn build_client() -> Result<Client, RiftError> {
    Client::builder()
        .connect_timeout(StdDuration::from_secs(8))
        .timeout(StdDuration::from_secs(20))
        .build()
        .map_err(RiftError::Network)
}

/// Normalize and validate a Rift base endpoint.
pub fn normalize_endpoint(endpoint: &str) -> Result<Url, RiftError> {
    let trimmed = endpoint.trim();
    let mut url =
        Url::parse(trimmed).map_err(|_| RiftError::Validation("Enter a valid Rift URL.".into()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(RiftError::Validation(
            "Rift endpoints must use HTTP or HTTPS.".into(),
        ));
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(RiftError::Validation(
            "Enter a Rift URL without embedded credentials.".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(RiftError::Validation(
            "The Rift URL cannot contain a query or fragment.".into(),
        ));
    }
    if !matches!(url.path(), "" | "/") {
        return Err(RiftError::Validation(
            "Enter the Rift service root, without an API path.".into(),
        ));
    }
    url.set_path("/");
    Ok(url)
}

/// Parse one authenticated Rift response or preserve its safe error message.
async fn parse_response<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, RiftError> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(RiftError::Authentication);
    }
    if !status.is_success() {
        let message = response
            .json::<ApiError>()
            .await
            .map(|body| body.error)
            .unwrap_or_else(|_| "Unexpected service error".into());
        return Err(RiftError::Remote { status, message });
    }
    response.json().await.map_err(RiftError::Protocol)
}

/// Send one authenticated Rift GET request and deserialize its JSON response.
async fn get_json<T: DeserializeOwned>(
    client: &Client,
    session: &RiftSession,
    path: &str,
) -> Result<T, RiftError> {
    let url = session
        .base_url
        .join(path)
        .map_err(|_| RiftError::Validation("Henosis could not build a Rift API URL.".into()))?;
    let response = client
        .get(url)
        .bearer_auth(&session.access_token)
        .send()
        .await
        .map_err(RiftError::Network)?;
    parse_response(response).await
}

/// Authenticate to Rift and construct native-only session state.
pub async fn login(input: &RiftConnectionInput) -> Result<RiftSession, RiftError> {
    if input.username.trim().is_empty() || input.password.is_empty() {
        return Err(RiftError::Validation(
            "Enter both a Rift username and password.".into(),
        ));
    }

    let base_url = normalize_endpoint(&input.endpoint)?;
    let login_url = base_url
        .join("api/auth/login")
        .map_err(|_| RiftError::Validation("Henosis could not build the login URL.".into()))?;
    let response = build_client()?
        .post(login_url)
        .json(&LoginRequest {
            username: input.username.trim(),
            password: &input.password,
        })
        .send()
        .await
        .map_err(RiftError::Network)?;
    let login: LoginResponse = parse_response(response).await?;
    let endpoint = base_url.as_str().trim_end_matches('/').to_owned();
    let display_name = login
        .user
        .display_name
        .clone()
        .unwrap_or_else(|| login.user.username.clone());

    Ok(RiftSession {
        base_url,
        access_token: login.token,
        _refresh_token: login.refresh_token,
        connection: SanitizedConnection {
            endpoint,
            username: login.user.username,
            user_id: login.user.id,
            display_name,
        },
    })
}

/// Produce a non-secret profile from an authenticated native session.
pub fn profile_for(session: &RiftSession) -> ConnectionProfile {
    ConnectionProfile {
        endpoint: session.connection.endpoint.clone(),
        username: session.connection.username.clone(),
    }
}

/// Convert Rift members into selector-safe participants.
fn participants_from(members: &[ApiMember]) -> Vec<RoomParticipant> {
    members
        .iter()
        .map(|member| RoomParticipant {
            id: member.user.id.clone(),
            display_name: member
                .user
                .display_name
                .clone()
                .unwrap_or_else(|| member.user.username.clone()),
            avatar_url: member.user.avatar_url.clone(),
            is_agent: member.user.is_agent,
            presence: Some(member.user.status.clone()),
        })
        .collect()
}

/// Create a useful preview for text and attachment-only messages.
fn message_preview(message: &ApiMessage) -> String {
    let content = message.content.trim();
    if !content.is_empty() {
        return content.to_owned();
    }
    match message.attachments.as_slice() {
        [] => "New activity".into(),
        [attachment] => format!("Shared {}", attachment.filename),
        attachments => format!("Shared {} attachments", attachments.len()),
    }
}

/// Derive a room status from bridge state and recent message activity.
fn room_status(paused: bool, latest: Option<&ApiMessage>) -> RoomStatus {
    if paused {
        return RoomStatus::Paused;
    }
    if latest.is_some_and(|message| message.created_at >= Utc::now() - Duration::minutes(15)) {
        RoomStatus::Active
    } else {
        RoomStatus::Quiet
    }
}

/// Compare rooms newest-first with stable name and id tie breakers.
fn compare_rooms(left: &RoomSummary, right: &RoomSummary) -> std::cmp::Ordering {
    right
        .last_activity_at
        .cmp(&left.last_activity_at)
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        .then_with(|| left.id.cmp(&right.id))
}

/// Fetch all visible Rift channels and aggregate them into room summaries.
pub async fn fetch_room_directory(
    session: &RiftSession,
) -> Result<RoomDirectorySnapshot, RiftError> {
    let client = build_client()?;
    let servers: Vec<ApiServer> = get_json(&client, session, "api/servers").await?;
    let mut rooms = Vec::new();

    for server in servers {
        let channels_path = format!("api/servers/{}/channels", server.id);
        let members_path = format!("api/servers/{}/members", server.id);
        let bridge_path = format!("api/servers/{}/bridge/status", server.id);
        let channels: Vec<ApiChannel> = get_json(&client, session, &channels_path).await?;
        let members: Vec<ApiMember> = get_json(&client, session, &members_path).await?;
        let bridge: ApiBridgeStatus = get_json(&client, session, &bridge_path).await?;
        let participants = participants_from(&members);

        for channel in channels
            .into_iter()
            .filter(|channel| channel.channel_type == "text")
        {
            let messages_path = format!("api/channels/{}/messages?limit=1", channel.id);
            let latest = get_json::<Vec<ApiMessage>>(&client, session, &messages_path)
                .await?
                .into_iter()
                .next();
            let last_activity_at = latest
                .as_ref()
                .map(|message| message.created_at)
                .unwrap_or(channel.created_at)
                .to_rfc3339();
            let latest_author = latest.as_ref().map(|message| {
                message
                    .author_display_name
                    .clone()
                    .unwrap_or_else(|| message.author_username.clone())
            });
            let preview = latest
                .as_ref()
                .map(message_preview)
                .unwrap_or_else(|| "No messages yet. Start the room.".into());

            rooms.push(RoomSummary {
                id: channel.id,
                name: channel.name,
                server_id: server.id.clone(),
                server_name: Some(server.name.clone()),
                topic: channel.topic,
                preview,
                latest_author,
                last_activity_at,
                participants: participants.clone(),
                unread_count: 0,
                status: room_status(bridge.paused, latest.as_ref()),
                active_work: None,
                pending_approvals: 0,
            });
        }
    }

    rooms.sort_by(compare_rooms);
    Ok(RoomDirectorySnapshot {
        connection: Some(session.connection.clone()),
        rooms,
        source: DirectorySource::Live,
        fetched_at: Utc::now().to_rfc3339(),
        connected: true,
    })
}

/// Best-effort Rift logout that never blocks local token destruction.
pub async fn logout(session: &RiftSession) {
    let Ok(url) = session.base_url.join("api/auth/logout") else {
        return;
    };
    let Ok(client) = build_client() else {
        return;
    };
    let _ = client
        .post(url)
        .bearer_auth(&session.access_token)
        .send()
        .await;
}

#[cfg(test)]
/// Pure native client tests that do not require a running Rift service.
mod tests {
    use super::*;

    /// Accept and normalize supported service-root URLs.
    #[test]
    fn normalizes_supported_endpoints() {
        let local = normalize_endpoint(" http://127.0.0.1:4010/ ").expect("valid local endpoint");
        let remote = normalize_endpoint("https://rift.example.test").expect("valid HTTPS endpoint");

        assert_eq!(local.as_str(), "http://127.0.0.1:4010/");
        assert_eq!(remote.as_str(), "https://rift.example.test/");
    }

    /// Reject dangerous or ambiguous endpoint shapes before a network request.
    #[test]
    fn rejects_invalid_endpoint_shapes() {
        assert!(normalize_endpoint("file:///tmp/rift").is_err());
        assert!(normalize_endpoint("https://user:secret@example.test").is_err());
        assert!(normalize_endpoint("https://example.test/api").is_err());
        assert!(normalize_endpoint("https://example.test?token=nope").is_err());
    }

    /// Sort equal timestamps by case-insensitive room name and opaque id.
    #[test]
    fn room_sorting_has_stable_tie_breakers() {
        let room = |id: &str, name: &str| RoomSummary {
            id: id.into(),
            name: name.into(),
            server_id: "server".into(),
            server_name: Some("Henosis".into()),
            topic: None,
            preview: "Preview".into(),
            latest_author: None,
            last_activity_at: "2026-07-26T12:00:00Z".into(),
            participants: Vec::new(),
            unread_count: 0,
            status: RoomStatus::Quiet,
            active_work: None,
            pending_approvals: 0,
        };
        let mut rooms = [room("z", "Zulu"), room("b", "alpha"), room("a", "Alpha")];

        rooms.sort_by(compare_rooms);

        assert_eq!(
            rooms
                .iter()
                .map(|room| room.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "z"]
        );
    }
}
