//! Native Rift HTTP client and room-summary aggregation.

use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;
use tokio::sync::{Mutex, RwLock};
use url::Url;

use crate::model::{
    CommandError, CommandErrorKind, ConnectionProfile, DirectorySource, RiftConnectionInput,
    RoomDirectorySnapshot, RoomParticipant, RoomStatus, RoomSummary, SanitizedConnection,
};

/// Shared native owner for authenticated Rift HTTP and token rotation.
#[derive(Clone)]
pub struct AuthenticatedRiftClient {
    /// Reference-counted transport and secret session state.
    inner: Arc<AuthenticatedRiftInner>,
}

/// Shared transport state hidden behind one authenticated client handle.
struct AuthenticatedRiftInner {
    /// Reused HTTP client with bounded transport timeouts.
    http: Client,
    /// Normalized Rift service root.
    base_url: Url,
    /// Sanitized identity established by the original login.
    connection: SanitizedConnection,
    /// Atomically readable and replaceable token pair.
    session: RwLock<RiftSession>,
    /// Single-flight guard for refresh-token consumption.
    refresh: Mutex<()>,
    /// False after AppState clears or replaces this authenticated handle.
    active: AtomicBool,
}

/// Access and refresh tokens retained only inside the native process.
#[derive(Clone)]
struct RiftSession {
    /// Short-lived Rift access token.
    access_token: String,
    /// Single-use long-lived token rotated by the refresh endpoint.
    refresh_token: String,
    /// Monotonic version used to detect refreshes completed by another caller.
    token_generation: u64,
}

/// Rebuildable authenticated request metadata safe to replay once.
#[derive(Clone)]
struct ReplayableRequest {
    /// HTTP method used for every attempt.
    method: Method,
    /// Relative Rift API path resolved against the validated service root.
    path: String,
}

/// Internal Rift client failure before conversion into a safe command error.
#[derive(Debug, thiserror::Error)]
pub enum RiftError {
    /// Endpoint failed local validation.
    #[error("{0}")]
    Validation(String),
    /// Rift rejected the username and password supplied to login.
    #[error("Rift rejected the supplied credentials.")]
    CredentialsRejected,
    /// Rift rejected both the current access token and its allowed refresh path.
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
    /// Rift returned a valid JSON shape that violated a session invariant.
    #[error("Rift returned data outside the expected authenticated-session contract.")]
    ProtocolContract,
}

/// Map native Rift failures into stable webview-safe error categories.
impl From<RiftError> for CommandError {
    /// Convert a Rift failure without including URLs, credentials, or response internals.
    fn from(error: RiftError) -> Self {
        match error {
            RiftError::Validation(message) => Self::new(CommandErrorKind::Validation, message),
            RiftError::CredentialsRejected => Self::new(
                CommandErrorKind::Authentication,
                "Rift did not accept that username and password.",
            ),
            RiftError::Authentication => Self::new(
                CommandErrorKind::Authentication,
                "Your Rift session expired. Sign in again to continue.",
            ),
            RiftError::Network(_) => Self::new(
                CommandErrorKind::Network,
                "Henosis could not reach Rift. Check the endpoint and service status.",
            ),
            RiftError::Remote { status, message } => Self::new(
                CommandErrorKind::Protocol,
                format!("Rift returned {status}: {message}"),
            ),
            RiftError::Protocol(_) | RiftError::ProtocolContract => Self::new(
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

/// Rift refresh request body containing the current single-use token.
#[derive(Serialize)]
struct RefreshRequest<'a> {
    /// Refresh token consumed and replaced by Rift.
    refresh_token: &'a str,
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

/// Construction and URL validation for replayable native requests.
impl ReplayableRequest {
    /// Describe one authenticated request without consuming its method or path.
    fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
        }
    }

    /// Describe one replayable authenticated GET request.
    fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path)
    }

    /// Describe one replayable authenticated POST request without a body.
    fn post(path: impl Into<String>) -> Self {
        Self::new(Method::POST, path)
    }

    /// Resolve the relative API path without allowing an origin change.
    fn resolve(&self, base_url: &Url) -> Result<Url, RiftError> {
        let url = base_url
            .join(&self.path)
            .map_err(|_| RiftError::Validation("Henosis could not build a Rift API URL.".into()))?;
        if url.origin() != base_url.origin() {
            return Err(RiftError::Validation(
                "Henosis refused a Rift API URL outside the connected service.".into(),
            ));
        }
        Ok(url)
    }
}

/// Shared authenticated request, refresh, and invalidation behavior.
impl AuthenticatedRiftClient {
    /// Construct one active native client from a successful Rift login.
    fn new(
        http: Client,
        base_url: Url,
        connection: SanitizedConnection,
        access_token: String,
        refresh_token: String,
    ) -> Self {
        Self {
            inner: Arc::new(AuthenticatedRiftInner {
                http,
                base_url,
                connection,
                session: RwLock::new(RiftSession {
                    access_token,
                    refresh_token,
                    token_generation: 0,
                }),
                refresh: Mutex::new(()),
                active: AtomicBool::new(true),
            }),
        }
    }

    /// Reject work after AppState has cleared or replaced this handle.
    fn ensure_active(&self) -> Result<(), RiftError> {
        if !self.inner.active.load(Ordering::Acquire) {
            return Err(RiftError::Authentication);
        }
        Ok(())
    }

    /// Clone the current tokens only while this handle remains active.
    async fn token_snapshot(&self) -> Result<RiftSession, RiftError> {
        self.ensure_active()?;
        Ok(self.inner.session.read().await.clone())
    }

    /// Clone the final token state for best-effort logout after invalidation.
    async fn final_token_snapshot(&self) -> RiftSession {
        self.inner.session.read().await.clone()
    }

    /// Send one described request with an explicit native access token.
    async fn send_with_token(
        &self,
        request: &ReplayableRequest,
        access_token: &str,
    ) -> Result<reqwest::Response, RiftError> {
        self.inner
            .http
            .request(
                request.method.clone(),
                request.resolve(&self.inner.base_url)?,
            )
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(RiftError::Network)
    }

    /// Consume one refresh token and atomically install its rotated token pair.
    async fn refresh_tokens(&self, observed: &RiftSession) -> Result<(), RiftError> {
        let url = self.inner.base_url.join("api/auth/refresh").map_err(|_| {
            RiftError::Validation("Henosis could not build the refresh URL.".into())
        })?;
        let response = self
            .inner
            .http
            .post(url)
            .json(&RefreshRequest {
                refresh_token: &observed.refresh_token,
            })
            .send()
            .await
            .map_err(RiftError::Network)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.invalidate();
            return Err(RiftError::Authentication);
        }
        let refreshed: LoginResponse = parse_response(response).await?;
        if refreshed.user.id != self.inner.connection.user_id {
            self.invalidate();
            return Err(RiftError::Authentication);
        }
        let next_generation = observed
            .token_generation
            .checked_add(1)
            .ok_or(RiftError::ProtocolContract)?;
        let mut current = self.inner.session.write().await;
        if current.token_generation != observed.token_generation {
            return Ok(());
        }
        *current = RiftSession {
            access_token: refreshed.token,
            refresh_token: refreshed.refresh_token,
            token_generation: next_generation,
        };
        Ok(())
    }

    /// Send once, single-flight refresh one observed generation, and replay once.
    async fn send_with_refresh(
        &self,
        request: &ReplayableRequest,
    ) -> Result<reqwest::Response, RiftError> {
        let observed = self.token_snapshot().await?;
        let response = self
            .send_with_token(request, &observed.access_token)
            .await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            self.ensure_active()?;
            return Ok(response);
        }

        let refresh_guard = self.inner.refresh.lock().await;
        let current = self.token_snapshot().await?;
        if current.token_generation == observed.token_generation {
            self.refresh_tokens(&current).await?;
        }
        drop(refresh_guard);

        let replay = self.token_snapshot().await?;
        let response = self.send_with_token(request, &replay.access_token).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.invalidate();
            return Err(RiftError::Authentication);
        }
        self.ensure_active()?;
        Ok(response)
    }

    /// Make this handle reject future authenticated requests and refreshes.
    pub(crate) fn invalidate(&self) {
        self.inner.active.store(false, Ordering::Release);
    }

    /// Report whether AppState still considers this handle authenticated.
    #[cfg(test)]
    fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::Acquire)
    }
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

/// Send one refresh-capable Rift GET request and deserialize its JSON response.
async fn get_json<T: DeserializeOwned>(
    client: &AuthenticatedRiftClient,
    path: &str,
) -> Result<T, RiftError> {
    let response = client
        .send_with_refresh(&ReplayableRequest::get(path))
        .await?;
    parse_response(response).await
}

/// Authenticate to Rift and construct native-only session state.
pub async fn login(input: &RiftConnectionInput) -> Result<AuthenticatedRiftClient, RiftError> {
    if input.username.trim().is_empty() || input.password.is_empty() {
        return Err(RiftError::Validation(
            "Enter both a Rift username and password.".into(),
        ));
    }

    let base_url = normalize_endpoint(&input.endpoint)?;
    let login_url = base_url
        .join("api/auth/login")
        .map_err(|_| RiftError::Validation("Henosis could not build the login URL.".into()))?;
    let http = build_client()?;
    let response = http
        .post(login_url)
        .json(&LoginRequest {
            username: input.username.trim(),
            password: &input.password,
        })
        .send()
        .await
        .map_err(RiftError::Network)?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(RiftError::CredentialsRejected);
    }
    let login: LoginResponse = parse_response(response).await?;
    let endpoint = base_url.as_str().trim_end_matches('/').to_owned();
    let display_name = login
        .user
        .display_name
        .clone()
        .unwrap_or_else(|| login.user.username.clone());

    let connection = SanitizedConnection {
        endpoint,
        username: login.user.username,
        user_id: login.user.id,
        display_name,
    };

    Ok(AuthenticatedRiftClient::new(
        http,
        base_url,
        connection,
        login.token,
        login.refresh_token,
    ))
}

/// Produce a non-secret profile from an authenticated native session.
pub fn profile_for(client: &AuthenticatedRiftClient) -> ConnectionProfile {
    ConnectionProfile {
        endpoint: client.inner.connection.endpoint.clone(),
        username: client.inner.connection.username.clone(),
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
    client: &AuthenticatedRiftClient,
) -> Result<RoomDirectorySnapshot, RiftError> {
    let servers: Vec<ApiServer> = get_json(client, "api/servers").await?;
    let mut rooms = Vec::new();

    for server in servers {
        let channels_path = format!("api/servers/{}/channels", server.id);
        let members_path = format!("api/servers/{}/members", server.id);
        let bridge_path = format!("api/servers/{}/bridge/status", server.id);
        let channels: Vec<ApiChannel> = get_json(client, &channels_path).await?;
        let members: Vec<ApiMember> = get_json(client, &members_path).await?;
        let bridge: ApiBridgeStatus = get_json(client, &bridge_path).await?;
        let participants = participants_from(&members);

        for channel in channels
            .into_iter()
            .filter(|channel| channel.channel_type == "text")
        {
            let messages_path = format!("api/channels/{}/messages?limit=1", channel.id);
            let latest = get_json::<Vec<ApiMessage>>(client, &messages_path)
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
        connection: Some(client.inner.connection.clone()),
        rooms,
        source: DirectorySource::Live,
        fetched_at: Utc::now().to_rfc3339(),
        connected: true,
    })
}

/// Best-effort Rift logout that never blocks local token destruction.
pub async fn logout(client: &AuthenticatedRiftClient) {
    client.invalidate();
    let refresh_guard = client.inner.refresh.lock().await;
    let request = ReplayableRequest::post("api/auth/logout");
    let session = client.final_token_snapshot().await;
    let Ok(response) = client
        .send_with_token(&request, &session.access_token)
        .await
    else {
        return;
    };
    if response.status() != StatusCode::UNAUTHORIZED {
        return;
    }
    if client.refresh_tokens(&session).await.is_err() {
        return;
    }
    let rotated = client.final_token_snapshot().await;
    let _ = client
        .send_with_token(&request, &rotated.access_token)
        .await;
    drop(refresh_guard);
}

#[cfg(test)]
/// Pure native client tests that do not require a running Rift service.
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc as StdArc, Barrier};
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    use serde_json::{Value, json};

    use super::*;

    /// One HTTP request captured by the bounded local mock server.
    struct MockRequest {
        /// Uppercase request method.
        method: String,
        /// Absolute-path request target.
        path: String,
        /// Authorization header when supplied.
        authorization: Option<String>,
        /// UTF-8 request body used by JSON refresh assertions.
        body: String,
    }

    /// One JSON response returned by the bounded local mock server.
    struct MockResponse {
        /// HTTP response status.
        status: u16,
        /// JSON response body.
        body: Value,
    }

    /// Bounded thread-per-connection HTTP fixture for deterministic retry tests.
    struct MockHttpServer {
        /// Loopback service root consumed by the native client.
        endpoint: Url,
        /// Listener task joined explicitly after the expected requests arrive.
        worker: JoinHandle<()>,
    }

    /// Response construction helpers for readable test handlers.
    impl MockResponse {
        /// Build one JSON response with the supplied status.
        fn json(status: u16, body: Value) -> Self {
            Self { status, body }
        }
    }

    /// Listener lifecycle for one exact expected request count.
    impl MockHttpServer {
        /// Start a loopback server whose connections may complete concurrently.
        fn start<F>(expected_requests: usize, handler: F) -> Self
        where
            F: Fn(MockRequest) -> MockResponse + Send + Sync + 'static,
        {
            let listener =
                TcpListener::bind("127.0.0.1:0").expect("authenticated test listener must bind");
            listener
                .set_nonblocking(true)
                .expect("authenticated test listener must become nonblocking");
            let endpoint = Url::parse(&format!(
                "http://{}/",
                listener
                    .local_addr()
                    .expect("authenticated test listener must have an address")
            ))
            .expect("authenticated test endpoint must parse");
            let handler = StdArc::new(handler);
            let worker = thread::spawn(move || {
                let deadline = Instant::now() + StdDuration::from_secs(10);
                let mut connections = Vec::with_capacity(expected_requests);
                while connections.len() < expected_requests && Instant::now() < deadline {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let handler = StdArc::clone(&handler);
                            connections.push(thread::spawn(move || {
                                handle_mock_connection(stream, handler);
                            }));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(StdDuration::from_millis(2));
                        }
                        Err(error) => panic!("authenticated test listener failed: {error}"),
                    }
                }
                assert_eq!(
                    connections.len(),
                    expected_requests,
                    "authenticated client sent an unexpected request count"
                );
                for connection in connections {
                    connection
                        .join()
                        .expect("authenticated test connection must complete");
                }
            });
            Self { endpoint, worker }
        }

        /// Join the bounded listener and surface handler assertion failures.
        fn finish(self) {
            self.worker
                .join()
                .expect("authenticated test server must complete");
        }
    }

    /// Read one HTTP/1.1 request, invoke its handler, and close the connection.
    fn handle_mock_connection<F>(mut stream: TcpStream, handler: StdArc<F>)
    where
        F: Fn(MockRequest) -> MockResponse + Send + Sync + 'static,
    {
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .expect("authenticated test stream must clone"),
        );
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .expect("authenticated request line must be readable");
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .expect("authenticated request must include a method")
            .to_owned();
        let path = parts
            .next()
            .expect("authenticated request must include a path")
            .to_owned();
        let mut authorization = None;
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("authenticated request header must be readable");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value
                    .trim()
                    .parse()
                    .expect("authenticated request content length must parse");
            }
        }
        let mut body = vec![0; content_length];
        reader
            .read_exact(&mut body)
            .expect("authenticated request body must be readable");
        let response = handler(MockRequest {
            method,
            path,
            authorization,
            body: String::from_utf8(body).expect("authenticated request body must be UTF-8"),
        });
        let body = response.body.to_string();
        let reason = match response.status {
            200 => "OK",
            401 => "Unauthorized",
            500 => "Internal Server Error",
            _ => "Test Response",
        };
        let encoded = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.status,
            reason,
            body.len(),
            body
        );
        stream
            .write_all(encoded.as_bytes())
            .expect("authenticated test response must be writable");
    }

    /// Construct one native authenticated client pointed at a loopback fixture.
    fn authenticated_client(
        endpoint: &Url,
        access_token: &str,
        refresh_token: &str,
    ) -> AuthenticatedRiftClient {
        AuthenticatedRiftClient::new(
            Client::builder()
                .no_proxy()
                .connect_timeout(StdDuration::from_secs(2))
                .timeout(StdDuration::from_secs(5))
                .build()
                .expect("authenticated test client must build"),
            endpoint.clone(),
            SanitizedConnection {
                endpoint: endpoint.as_str().trim_end_matches('/').into(),
                username: "operator".into(),
                user_id: "user-1".into(),
                display_name: "Operator".into(),
            },
            access_token.into(),
            refresh_token.into(),
        )
    }

    /// Build the complete Rift refresh response shape for one token pair.
    fn refresh_response(access_token: &str, refresh_token: &str, user_id: &str) -> Value {
        json!({
            "token": access_token,
            "refresh_token": refresh_token,
            "user": {
                "id": user_id,
                "username": "operator",
                "display_name": "Operator",
                "avatar_url": null,
                "status": "online",
                "is_agent": false,
            }
        })
    }

    /// Authenticated requests use the current native access token immediately.
    #[tokio::test]
    async fn authenticated_request_uses_current_bearer_token() {
        let server = MockHttpServer::start(1, |request| {
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/api/test");
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer current-access")
            );
            MockResponse::json(200, json!({ "ok": true }))
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let result = get_json::<Value>(&client, "api/test").await;
        server.finish();

        assert_eq!(
            result.expect("current bearer request must succeed"),
            json!({ "ok": true })
        );
    }

    /// One 401 rotates both tokens, replays once, and reuses the rotated session.
    #[tokio::test]
    async fn authenticated_unauthorized_request_refreshes_and_reuses_rotated_tokens() {
        let refreshes = StdArc::new(AtomicUsize::new(0));
        let observed_refreshes = StdArc::clone(&refreshes);
        let server = MockHttpServer::start(4, move |request| {
            match (
                request.method.as_str(),
                request.path.as_str(),
                request.authorization.as_deref(),
            ) {
                ("GET", "/api/test", Some("Bearer expired-access")) => {
                    MockResponse::json(401, json!({ "error": "expired" }))
                }
                ("POST", "/api/auth/refresh", None) => {
                    observed_refreshes.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        serde_json::from_str::<Value>(&request.body)
                            .expect("refresh request JSON must parse"),
                        json!({ "refresh_token": "refresh-one" })
                    );
                    MockResponse::json(
                        200,
                        refresh_response("rotated-access", "refresh-two", "user-1"),
                    )
                }
                ("GET", "/api/test", Some("Bearer rotated-access")) => {
                    MockResponse::json(200, json!({ "ok": true }))
                }
                unexpected => panic!("unexpected authenticated request: {unexpected:?}"),
            }
        });
        let client = authenticated_client(&server.endpoint, "expired-access", "refresh-one");

        let first = get_json::<Value>(&client, "api/test").await;
        let second = get_json::<Value>(&client, "api/test").await;
        let tokens = client
            .token_snapshot()
            .await
            .expect("rotated token state must remain active");
        server.finish();

        assert_eq!(
            first.expect("replayed request must succeed"),
            json!({ "ok": true })
        );
        assert_eq!(
            second.expect("rotated token must be reused"),
            json!({ "ok": true })
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(tokens.access_token, "rotated-access");
        assert_eq!(tokens.refresh_token, "refresh-two");
        assert_eq!(tokens.token_generation, 1);
    }

    /// A replayed 401 is surfaced without entering a second refresh loop.
    #[tokio::test]
    async fn authenticated_second_unauthorized_response_is_not_retried() {
        let refreshes = StdArc::new(AtomicUsize::new(0));
        let observed_refreshes = StdArc::clone(&refreshes);
        let server = MockHttpServer::start(3, move |request| match request.path.as_str() {
            "/api/auth/refresh" => {
                observed_refreshes.fetch_add(1, Ordering::SeqCst);
                MockResponse::json(
                    200,
                    refresh_response("rejected-access", "refresh-two", "user-1"),
                )
            }
            "/api/test" => MockResponse::json(401, json!({ "error": "unauthorized" })),
            unexpected => panic!("unexpected authenticated path: {unexpected}"),
        });
        let client = authenticated_client(&server.endpoint, "expired-access", "refresh-one");

        let result = get_json::<Value>(&client, "api/test").await;
        server.finish();

        assert!(matches!(result, Err(RiftError::Authentication)));
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(!client.is_active());
    }

    /// A rejected refresh invalidates the session without replaying the request.
    #[tokio::test]
    async fn authenticated_rejected_refresh_invalidates_session() {
        let server = MockHttpServer::start(2, |request| match request.path.as_str() {
            "/api/test" => MockResponse::json(401, json!({ "error": "expired" })),
            "/api/auth/refresh" => MockResponse::json(401, json!({ "error": "invalid refresh" })),
            unexpected => panic!("unexpected authenticated path: {unexpected}"),
        });
        let client = authenticated_client(&server.endpoint, "expired-access", "refresh-one");

        let result = get_json::<Value>(&client, "api/test").await;
        server.finish();

        assert!(matches!(result, Err(RiftError::Authentication)));
        assert!(!client.is_active());
    }

    /// Concurrent 401 responses consume one refresh token and share its rotation.
    #[tokio::test]
    async fn authenticated_concurrent_callers_refresh_once() {
        let old_requests = StdArc::new(Barrier::new(2));
        let observed_old_requests = StdArc::clone(&old_requests);
        let refreshes = StdArc::new(AtomicUsize::new(0));
        let observed_refreshes = StdArc::clone(&refreshes);
        let rotated_requests = StdArc::new(AtomicUsize::new(0));
        let observed_rotated_requests = StdArc::clone(&rotated_requests);
        let server = MockHttpServer::start(5, move |request| {
            match (
                request.method.as_str(),
                request.path.as_str(),
                request.authorization.as_deref(),
            ) {
                ("GET", "/api/test", Some("Bearer expired-access")) => {
                    observed_old_requests.wait();
                    MockResponse::json(401, json!({ "error": "expired" }))
                }
                ("POST", "/api/auth/refresh", None) => {
                    observed_refreshes.fetch_add(1, Ordering::SeqCst);
                    MockResponse::json(
                        200,
                        refresh_response("rotated-access", "refresh-two", "user-1"),
                    )
                }
                ("GET", "/api/test", Some("Bearer rotated-access")) => {
                    observed_rotated_requests.fetch_add(1, Ordering::SeqCst);
                    MockResponse::json(200, json!({ "ok": true }))
                }
                unexpected => panic!("unexpected concurrent request: {unexpected:?}"),
            }
        });
        let client = authenticated_client(&server.endpoint, "expired-access", "refresh-one");
        let first_client = client.clone();
        let second_client = client.clone();

        let first = tokio::spawn(async move { get_json::<Value>(&first_client, "api/test").await });
        let second =
            tokio::spawn(async move { get_json::<Value>(&second_client, "api/test").await });
        let first = first.await.expect("first authenticated task must join");
        let second = second.await.expect("second authenticated task must join");
        server.finish();

        assert_eq!(
            first.expect("first replay must succeed"),
            json!({ "ok": true })
        );
        assert_eq!(
            second.expect("second replay must succeed"),
            json!({ "ok": true })
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(rotated_requests.load(Ordering::SeqCst), 2);
    }

    /// Non-authentication remote failures return immediately without refresh.
    #[tokio::test]
    async fn authenticated_non_unauthorized_error_is_not_retried() {
        let server = MockHttpServer::start(1, |request| {
            assert_eq!(request.path, "/api/test");
            MockResponse::json(500, json!({ "error": "temporary failure" }))
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let result = get_json::<Value>(&client, "api/test").await;
        server.finish();

        assert!(matches!(
            result,
            Err(RiftError::Remote {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                ..
            })
        ));
    }

    /// A refresh response cannot silently switch the authenticated human.
    #[tokio::test]
    async fn authenticated_refresh_rejects_identity_change() {
        let server = MockHttpServer::start(2, |request| match request.path.as_str() {
            "/api/test" => MockResponse::json(401, json!({ "error": "expired" })),
            "/api/auth/refresh" => MockResponse::json(
                200,
                refresh_response("other-access", "other-refresh", "user-2"),
            ),
            unexpected => panic!("unexpected authenticated path: {unexpected}"),
        });
        let client = authenticated_client(&server.endpoint, "expired-access", "refresh-one");

        let result = get_json::<Value>(&client, "api/test").await;
        let tokens = client.final_token_snapshot().await;
        server.finish();

        assert!(matches!(result, Err(RiftError::Authentication)));
        assert!(!client.is_active());
        assert_eq!(tokens.access_token, "expired-access");
        assert_eq!(tokens.refresh_token, "refresh-one");
        assert_eq!(tokens.token_generation, 0);
    }

    /// Logout refreshes an expired access token once before remote revocation.
    #[tokio::test]
    async fn authenticated_logout_refreshes_expired_access_before_revocation() {
        let refreshes = StdArc::new(AtomicUsize::new(0));
        let observed_refreshes = StdArc::clone(&refreshes);
        let logout_attempts = StdArc::new(AtomicUsize::new(0));
        let observed_logout_attempts = StdArc::clone(&logout_attempts);
        let server = MockHttpServer::start(3, move |request| {
            match (
                request.method.as_str(),
                request.path.as_str(),
                request.authorization.as_deref(),
            ) {
                ("POST", "/api/auth/logout", Some("Bearer expired-access")) => {
                    observed_logout_attempts.fetch_add(1, Ordering::SeqCst);
                    MockResponse::json(401, json!({ "error": "expired" }))
                }
                ("POST", "/api/auth/refresh", None) => {
                    observed_refreshes.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        serde_json::from_str::<Value>(&request.body)
                            .expect("logout refresh request JSON must parse"),
                        json!({ "refresh_token": "refresh-one" })
                    );
                    MockResponse::json(
                        200,
                        refresh_response("rotated-access", "refresh-two", "user-1"),
                    )
                }
                ("POST", "/api/auth/logout", Some("Bearer rotated-access")) => {
                    observed_logout_attempts.fetch_add(1, Ordering::SeqCst);
                    MockResponse::json(200, json!({ "ok": true }))
                }
                unexpected => panic!("unexpected logout request: {unexpected:?}"),
            }
        });
        let client = authenticated_client(&server.endpoint, "expired-access", "refresh-one");

        logout(&client).await;
        let tokens = client.final_token_snapshot().await;
        server.finish();

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(logout_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(tokens.access_token, "rotated-access");
        assert_eq!(tokens.refresh_token, "refresh-two");
        assert_eq!(tokens.token_generation, 1);
        assert!(!client.is_active());
    }

    /// Clearing state during refresh still revokes the final rotated session.
    #[tokio::test]
    async fn authenticated_clear_during_refresh_logs_out_rotated_session() {
        let refresh_started = StdArc::new(AtomicBool::new(false));
        let observed_refresh_started = StdArc::clone(&refresh_started);
        let allow_refresh = StdArc::new(AtomicBool::new(false));
        let observed_allow_refresh = StdArc::clone(&allow_refresh);
        let server = MockHttpServer::start(3, move |request| {
            match (
                request.method.as_str(),
                request.path.as_str(),
                request.authorization.as_deref(),
            ) {
                ("GET", "/api/test", Some("Bearer expired-access")) => {
                    MockResponse::json(401, json!({ "error": "expired" }))
                }
                ("POST", "/api/auth/refresh", None) => {
                    observed_refresh_started.store(true, Ordering::Release);
                    let deadline = Instant::now() + StdDuration::from_secs(5);
                    while !observed_allow_refresh.load(Ordering::Acquire) {
                        assert!(
                            Instant::now() < deadline,
                            "test must release the in-flight refresh"
                        );
                        thread::sleep(StdDuration::from_millis(1));
                    }
                    MockResponse::json(
                        200,
                        refresh_response("rotated-access", "refresh-two", "user-1"),
                    )
                }
                ("POST", "/api/auth/logout", Some("Bearer rotated-access")) => {
                    MockResponse::json(200, json!({ "ok": true }))
                }
                unexpected => panic!("unexpected clear-during-refresh request: {unexpected:?}"),
            }
        });
        let client = authenticated_client(&server.endpoint, "expired-access", "refresh-one");
        let state = crate::state::AppState::new();
        state
            .set_session(client.clone())
            .expect("authenticated client must install before refresh");
        let request_client = client.clone();
        let request =
            tokio::spawn(async move { get_json::<Value>(&request_client, "api/test").await });

        let wait_deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        while !refresh_started.load(Ordering::Acquire) {
            assert!(
                tokio::time::Instant::now() < wait_deadline,
                "refresh request must reach the mock server"
            );
            tokio::time::sleep(StdDuration::from_millis(1)).await;
        }
        state
            .clear_session()
            .expect("authenticated client must clear during refresh");
        let logout_client = client.clone();
        let logout_task = tokio::spawn(async move { logout(&logout_client).await });
        allow_refresh.store(true, Ordering::Release);

        let request_result = request
            .await
            .expect("in-flight authenticated request must join");
        logout_task.await.expect("serialized logout task must join");
        let tokens = client.final_token_snapshot().await;
        server.finish();

        assert!(matches!(request_result, Err(RiftError::Authentication)));
        assert_eq!(tokens.access_token, "rotated-access");
        assert_eq!(tokens.refresh_token, "refresh-two");
        assert_eq!(tokens.token_generation, 1);
        assert!(!client.is_active());
        assert!(
            state
                .session()
                .expect("cleared authenticated state must remain readable")
                .is_none()
        );
    }

    /// AppState invalidates handles that are replaced or explicitly cleared.
    #[tokio::test]
    async fn authenticated_state_invalidates_replaced_and_cleared_clients() {
        let endpoint = Url::parse("http://127.0.0.1:9/").expect("test endpoint must parse");
        let first = authenticated_client(&endpoint, "first-access", "first-refresh");
        let second = authenticated_client(&endpoint, "second-access", "second-refresh");
        let state = crate::state::AppState::new();

        state
            .set_session(first.clone())
            .expect("first authenticated client must install");
        state
            .set_session(second.clone())
            .expect("replacement authenticated client must install");
        assert!(!first.is_active());
        assert!(second.is_active());

        state
            .clear_session()
            .expect("authenticated client must clear");
        assert!(!second.is_active());
        assert!(matches!(
            second.token_snapshot().await,
            Err(RiftError::Authentication)
        ));
    }

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
