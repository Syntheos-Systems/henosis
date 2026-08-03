//! Native Rift HTTP client and room-summary aggregation.

use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;
use tokio::sync::{Mutex, RwLock};
use url::Url;

use crate::model::{
    CommandError, CommandErrorKind, ConnectionProfile, DirectorySource, MessagePage,
    PendingRoomAttachment, RiftConnectionInput, RoomAttachment, RoomDirectorySnapshot, RoomMessage,
    RoomParticipant, RoomStatus, RoomSummary, SanitizedConnection,
};

/// Hard cap on retained file bytes for one replayable native upload request.
const MAX_NATIVE_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

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

/// One non-serializable access-token snapshot consumed only by the native gateway.
pub(crate) struct GatewayAuthentication {
    /// Short-lived token sent in Rift's first WebSocket command.
    access_token: String,
    /// Monotonic generation used to avoid consuming a rotated refresh token twice.
    token_generation: u64,
}

/// Native gateway accessors that never implement serialization or debug output.
impl GatewayAuthentication {
    /// Construct one gateway-only authentication snapshot.
    fn new(access_token: String, token_generation: u64) -> Self {
        Self {
            access_token,
            token_generation,
        }
    }

    /// Construct one fake authentication snapshot for native gateway tests.
    #[cfg(test)]
    pub(crate) fn for_gateway_test(access_token: String, token_generation: u64) -> Self {
        Self::new(access_token, token_generation)
    }

    /// Borrow the access token only while serializing Rift's Identify command.
    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    /// Return the observed token generation for single-flight refresh coordination.
    pub(crate) fn token_generation(&self) -> u64 {
        self.token_generation
    }
}

/// Rebuildable authenticated request metadata safe to replay once.
#[derive(Clone)]
struct ReplayableRequest {
    /// HTTP method used for every attempt.
    method: Method,
    /// Relative Rift API path resolved against the validated service root.
    path: String,
    /// Owned request content rebuilt for each authorized attempt.
    body: ReplayableBody,
}

/// Native request content that can construct a fresh reqwest body after refresh.
#[derive(Clone)]
enum ReplayableBody {
    /// Request carries no entity body.
    Empty,
    /// Pre-serialized JSON bytes safe to clone for one replay.
    Json(Vec<u8>),
    /// Selected attachment bytes retained only inside the native process.
    Multipart(Vec<NativeUploadPart>),
}

/// One selected attachment stripped to a display filename and owned native bytes.
#[derive(Clone)]
struct NativeUploadPart {
    /// Base filename sent as multipart metadata without its local directory.
    filename: String,
    /// Reference-counted bytes read once and shared by each bounded retry body.
    bytes: Bytes,
    /// Exact byte count supplied to reqwest's fresh multipart stream.
    size_bytes: u64,
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
    /// Rift rejected a message cursor that no longer belongs to the requested room.
    #[error("Rift no longer recognizes the saved message cursor.")]
    InvalidMessageCursor,
    /// HTTP transport could not reach Rift.
    #[error("Rift could not be reached.")]
    Network(#[source] reqwest::Error),
    /// A selected attachment could not be read from native storage.
    #[error("Henosis could not read one selected attachment.")]
    FileRead(#[source] std::io::Error),
    /// A background native file read could not complete.
    #[error("Henosis could not complete an attachment read.")]
    FileTask(#[source] tokio::task::JoinError),
    /// A native request body could not be serialized.
    #[error("Henosis could not encode a Rift request.")]
    Encoding(#[source] serde_json::Error),
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
            RiftError::InvalidMessageCursor => Self::new(
                CommandErrorKind::Protocol,
                "Rift no longer recognizes the saved message cursor.",
            ),
            RiftError::Network(_) => Self::new(
                CommandErrorKind::Network,
                "Henosis could not reach Rift. Check the endpoint and service status.",
            ),
            RiftError::FileRead(_) | RiftError::FileTask(_) => Self::new(
                CommandErrorKind::Storage,
                "Henosis could not read one selected attachment.",
            ),
            RiftError::Encoding(_) => Self::new(
                CommandErrorKind::Protocol,
                "Henosis could not encode the Rift request.",
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

/// Full Rift attachment returned beneath a conversation message.
#[derive(Deserialize)]
struct ApiRoomAttachment {
    /// Stable attachment identifier.
    id: String,
    /// Message identifier that must match the containing response.
    message_id: String,
    /// Original display filename supplied during upload.
    filename: String,
    /// Relative or absolute attachment URL supplied by Rift.
    url: String,
    /// Optional declared media type.
    content_type: Option<String>,
    /// Stored byte count when available.
    size_bytes: Option<i64>,
}

/// Full Rift message returned by room conversation endpoints.
#[derive(Deserialize)]
pub(crate) struct ApiRoomMessage {
    /// Stable message identifier.
    id: String,
    /// Rift channel identifier normalized as a Henosis room identifier.
    channel_id: String,
    /// Stable author identifier.
    author_id: String,
    /// Message body, possibly empty for attachment-only messages.
    content: String,
    /// Latest edit timestamp when present.
    edited_at: Option<DateTime<Utc>>,
    /// Creation timestamp used for deterministic oldest-first ordering.
    created_at: DateTime<Utc>,
    /// Rift message discriminator.
    message_type: String,
    /// Unambiguous Rift login handle.
    author_username: String,
    /// Optional human-facing author name.
    author_display_name: Option<String>,
    /// Optional author avatar URL.
    author_avatar_url: Option<String>,
    /// Attachments returned with the message.
    attachments: Vec<ApiRoomAttachment>,
}

/// Rift upload response retained only until sanitized pending metadata is built.
#[derive(Deserialize)]
struct ApiPendingRoomAttachment {
    /// Opaque identifier accepted by the subsequent create-message request.
    upload_id: String,
    /// Original display filename returned by Rift.
    filename: String,
    /// Same-origin object URL validated before the response is accepted.
    url: String,
    /// Optional declared media type.
    content_type: Option<String>,
    /// Stored byte count returned by Rift.
    size_bytes: i64,
}

/// JSON body used to create one room message.
#[derive(Serialize)]
struct CreateRoomMessageRequest<'a> {
    /// Optional text omitted for attachment-only messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    /// Optional pending upload identifiers omitted when no files are attached.
    #[serde(skip_serializing_if = "Option::is_none")]
    attachment_ids: Option<&'a [String]>,
}

/// JSON body used to replace one message's text.
#[derive(Serialize)]
struct EditRoomMessageRequest<'a> {
    /// Replacement message text.
    content: &'a str,
}

/// Rift acknowledgement returned after deleting one message.
#[derive(Deserialize)]
struct DeleteRoomMessageResponse {
    /// True only when Rift completed the deletion.
    ok: bool,
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
    /// Stable machine-readable error code when Rift supplies one.
    #[serde(default)]
    code: Option<String>,
}

/// Configure bounded Rift transport behavior shared by production and deterministic tests.
fn rift_client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(StdDuration::from_secs(8))
        .timeout(StdDuration::from_secs(20))
}

/// Build a bounded Rift client that never redirects authenticated native request bodies.
fn build_client() -> Result<Client, RiftError> {
    rift_client_builder().build().map_err(RiftError::Network)
}

/// Fresh request-builder construction for each replayable body shape.
impl ReplayableBody {
    /// Apply owned native content to one newly constructed request attempt.
    fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::Empty => builder,
            Self::Json(bytes) => builder
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes.clone()),
            Self::Multipart(parts) => builder.multipart(multipart_form(parts)),
        }
    }
}

/// Conversion of retained attachment bytes into a fresh multipart part.
impl NativeUploadPart {
    /// Rebuild one multipart part by shallow-cloning bounded bytes, never its local path.
    fn to_multipart_part(&self) -> Part {
        Part::stream_with_length(self.bytes.clone(), self.size_bytes)
            .file_name(self.filename.clone())
    }
}

/// Build a fresh multipart form so a 401 retry never reuses a consumed stream.
fn multipart_form(parts: &[NativeUploadPart]) -> Form {
    parts.iter().fold(Form::new(), |form, part| {
        form.part("files", part.to_multipart_part())
    })
}

/// Construction and URL validation for replayable native requests.
impl ReplayableRequest {
    /// Describe one authenticated request with explicitly owned replayable content.
    fn new(method: Method, path: impl Into<String>, body: ReplayableBody) -> Self {
        Self {
            method,
            path: path.into(),
            body,
        }
    }

    /// Describe one replayable authenticated GET request.
    fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path, ReplayableBody::Empty)
    }

    /// Describe one replayable authenticated POST request without a body.
    fn post(path: impl Into<String>) -> Self {
        Self::new(Method::POST, path, ReplayableBody::Empty)
    }

    /// Serialize one replayable authenticated JSON request.
    fn json<T: Serialize>(
        method: Method,
        path: impl Into<String>,
        body: &T,
    ) -> Result<Self, RiftError> {
        Ok(Self::new(
            method,
            path,
            ReplayableBody::Json(serde_json::to_vec(body).map_err(RiftError::Encoding)?),
        ))
    }

    /// Describe one replayable authenticated multipart upload.
    fn multipart(path: impl Into<String>, parts: Vec<NativeUploadPart>) -> Self {
        Self::new(Method::POST, path, ReplayableBody::Multipart(parts))
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

    /// Construct an isolated active client for native gateway ownership tests.
    #[cfg(test)]
    pub(crate) fn gateway_test_client(base_url: Url, user_id: &str) -> Self {
        let endpoint = base_url.as_str().trim_end_matches('/').to_owned();
        Self::new(
            Client::new(),
            base_url,
            SanitizedConnection {
                endpoint,
                username: "gateway-test-user".into(),
                user_id: user_id.into(),
                display_name: "Gateway Test User".into(),
            },
            "gateway-test-access".into(),
            "gateway-test-refresh".into(),
        )
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

    /// Snapshot only the access token and generation needed by one gateway attempt.
    pub(crate) async fn gateway_authentication(&self) -> Result<GatewayAuthentication, RiftError> {
        let session = self.token_snapshot().await?;
        Ok(GatewayAuthentication::new(
            session.access_token,
            session.token_generation,
        ))
    }

    /// Refresh one rejected gateway generation through the existing single-flight lock.
    pub(crate) async fn refresh_gateway_authentication(
        &self,
        observed_generation: u64,
    ) -> Result<(), RiftError> {
        self.ensure_active()?;
        let refresh_guard = self.inner.refresh.lock().await;
        let current = self.token_snapshot().await?;
        if current.token_generation == observed_generation {
            self.refresh_tokens(&current).await?;
        }
        drop(refresh_guard);
        self.ensure_active()
    }

    /// Derive Rift's fixed WebSocket endpoint without accepting a redirect target.
    pub(crate) fn gateway_websocket_url(&self) -> Result<Url, RiftError> {
        let mut url = self.inner.base_url.clone();
        let websocket_scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            _ => return Err(RiftError::ProtocolContract),
        };
        url.set_scheme(websocket_scheme)
            .map_err(|_| RiftError::ProtocolContract)?;
        url.set_path("/ws");
        url.set_query(None);
        url.set_fragment(None);
        Ok(url)
    }

    /// Clone the authenticated user identifier expected in Rift's Ready event.
    pub(crate) fn gateway_user_id(&self) -> String {
        self.inner.connection.user_id.clone()
    }

    /// Report whether two handles point at the same native authenticated session.
    pub(crate) fn same_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Report whether this session may still emit native gateway events.
    pub(crate) fn gateway_is_active(&self) -> bool {
        self.inner.active.load(Ordering::Acquire)
    }

    /// Reuse the HTTP boundary's message and attachment validation for gateway events.
    pub(crate) fn sanitize_gateway_message(
        &self,
        message: ApiRoomMessage,
        expected_room_id: &str,
    ) -> Result<RoomMessage, RiftError> {
        self.ensure_active()?;
        message.into_room_message(&self.inner.base_url, expected_room_id)
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
        let builder = self
            .inner
            .http
            .request(
                request.method.clone(),
                request.resolve(&self.inner.base_url)?,
            )
            .bearer_auth(access_token);
        request
            .body
            .apply(builder)
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
        let body = response.json::<ApiError>().await.ok();
        if status == StatusCode::NOT_FOUND
            && body.as_ref().and_then(|body| body.code.as_deref()) == Some("invalid_message_cursor")
        {
            return Err(RiftError::InvalidMessageCursor);
        }
        let message = body
            .map(|body| body.error)
            .unwrap_or_else(|| "Unexpected service error".into());
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

/// Percent-encode one opaque identifier so it remains exactly one URL path segment.
fn encode_path_segment(value: &str) -> Result<String, RiftError> {
    if value.is_empty() {
        return Err(RiftError::Validation(
            "Rift room and message identifiers cannot be empty.".into(),
        ));
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    Ok(encoded)
}

/// Build the collection route for one opaque Rift room identifier.
fn room_messages_path(room_id: &str) -> Result<String, RiftError> {
    Ok(format!(
        "api/channels/{}/messages",
        encode_path_segment(room_id)?
    ))
}

/// Build the room-bound route for one opaque Rift message identifier.
fn room_message_path(room_id: &str, message_id: &str) -> Result<String, RiftError> {
    Ok(format!(
        "{}/{}",
        room_messages_path(room_id)?,
        encode_path_segment(message_id)?
    ))
}

/// Reject page limits outside Rift's positive server-capped range.
fn validate_page_limit(limit: u32) -> Result<(), RiftError> {
    if !(1..=100).contains(&limit) {
        return Err(RiftError::Validation(
            "Room message page size must be between 1 and 100.".into(),
        ));
    }
    Ok(())
}

/// Build one latest, before, or after message page path with encoded query values.
fn room_message_page_path(
    room_id: &str,
    cursor: Option<(&str, &str)>,
    limit: u32,
) -> Result<String, RiftError> {
    validate_page_limit(limit)?;
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if let Some((name, value)) = cursor {
        if value.is_empty() {
            return Err(RiftError::Validation(
                "Rift message cursors cannot be empty.".into(),
            ));
        }
        query.append_pair(name, value);
    }
    query.append_pair("limit", &limit.to_string());
    Ok(format!(
        "{}?{}",
        room_messages_path(room_id)?,
        query.finish()
    ))
}

/// Resolve one attachment reference without allowing a scheme or origin change.
fn resolve_attachment_url(base_url: &Url, raw_url: &str) -> Result<String, RiftError> {
    if raw_url.is_empty() {
        return Err(RiftError::ProtocolContract);
    }
    let resolved = base_url
        .join(raw_url)
        .map_err(|_| RiftError::ProtocolContract)?;
    if !matches!(resolved.scheme(), "http" | "https")
        || resolved.host_str().is_none()
        || !resolved.username().is_empty()
        || resolved.password().is_some()
        || resolved.origin() != base_url.origin()
    {
        return Err(RiftError::ProtocolContract);
    }
    Ok(resolved.to_string())
}

/// Reject server filenames that could expose a local or remote directory component.
fn validate_display_filename(filename: &str) -> Result<(), RiftError> {
    if filename.is_empty()
        || filename
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '\0' | '\r' | '\n'))
    {
        return Err(RiftError::ProtocolContract);
    }
    Ok(())
}

/// Convert a non-negative Rift byte count into the native unsigned DTO contract.
fn checked_size_bytes(size_bytes: i64) -> Result<u64, RiftError> {
    u64::try_from(size_bytes).map_err(|_| RiftError::ProtocolContract)
}

/// Sanitization of one Rift attachment before it crosses into shared model state.
impl ApiRoomAttachment {
    /// Resolve its URL, strip server-only fields, and validate optional byte count.
    fn into_room_attachment(
        self,
        base_url: &Url,
        expected_message_id: &str,
    ) -> Result<RoomAttachment, RiftError> {
        if self.id.is_empty()
            || self.message_id != expected_message_id
            || expected_message_id.is_empty()
        {
            return Err(RiftError::ProtocolContract);
        }
        validate_display_filename(&self.filename)?;
        Ok(RoomAttachment {
            id: self.id,
            filename: self.filename,
            url: resolve_attachment_url(base_url, &self.url)?,
            content_type: self.content_type,
            size_bytes: self.size_bytes.map(checked_size_bytes).transpose()?,
        })
    }
}

/// Sanitization of one Rift message before timeline insertion.
impl ApiRoomMessage {
    /// Validate its room binding and convert all nested attachment metadata.
    pub(crate) fn into_room_message(
        self,
        base_url: &Url,
        expected_room_id: &str,
    ) -> Result<RoomMessage, RiftError> {
        if self.id.is_empty()
            || self.channel_id != expected_room_id
            || expected_room_id.is_empty()
            || self.author_id.is_empty()
            || self.author_username.is_empty()
            || self.message_type.is_empty()
        {
            return Err(RiftError::ProtocolContract);
        }
        let attachments = self
            .attachments
            .into_iter()
            .map(|attachment| attachment.into_room_attachment(base_url, &self.id))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RoomMessage {
            id: self.id,
            room_id: self.channel_id,
            author_id: self.author_id,
            author_username: self.author_username,
            author_display_name: self.author_display_name,
            author_avatar_url: self.author_avatar_url,
            content: self.content,
            edited_at: self.edited_at.map(|timestamp| timestamp.to_rfc3339()),
            created_at: self.created_at.to_rfc3339(),
            message_type: self.message_type,
            attachments,
        })
    }
}

/// Sanitization of one staged upload before returning safe pending metadata.
impl ApiPendingRoomAttachment {
    /// Validate its display name, URL, and byte count while discarding the URL.
    fn into_pending_attachment(self, base_url: &Url) -> Result<PendingRoomAttachment, RiftError> {
        validate_display_filename(&self.filename)?;
        let _validated_url = resolve_attachment_url(base_url, &self.url)?;
        Ok(PendingRoomAttachment {
            upload_id: self.upload_id,
            filename: self.filename,
            content_type: self.content_type,
            size_bytes: checked_size_bytes(self.size_bytes)?,
        })
    }
}

/// Send one replayable request and deserialize its successful JSON response.
async fn send_json_request<T: DeserializeOwned>(
    client: &AuthenticatedRiftClient,
    request: &ReplayableRequest,
) -> Result<T, RiftError> {
    parse_response(client.send_with_refresh(request).await?).await
}

/// Convert one server message vector into a stable oldest-first native page.
fn room_message_page(
    mut messages: Vec<ApiRoomMessage>,
    client: &AuthenticatedRiftClient,
    room_id: &str,
    limit: u32,
    exposes_older_cursor: bool,
) -> Result<MessagePage, RiftError> {
    messages.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let has_older = exposes_older_cursor && messages.len() == limit as usize;
    let messages = messages
        .into_iter()
        .map(|message| message.into_room_message(&client.inner.base_url, room_id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MessagePage {
        messages,
        has_older,
    })
}

/// Fetch one message page and normalize its response for direct timeline insertion.
async fn fetch_message_page(
    client: &AuthenticatedRiftClient,
    room_id: &str,
    cursor: Option<(&str, &str)>,
    limit: u32,
    exposes_older_cursor: bool,
) -> Result<MessagePage, RiftError> {
    let path = room_message_page_path(room_id, cursor, limit)?;
    let messages: Vec<ApiRoomMessage> = get_json(client, &path).await?;
    room_message_page(messages, client, room_id, limit, exposes_older_cursor)
}

/// Fetch the newest bounded room page and return it oldest-first.
pub(crate) async fn latest_messages(
    client: &AuthenticatedRiftClient,
    room_id: &str,
    limit: u32,
) -> Result<MessagePage, RiftError> {
    fetch_message_page(client, room_id, None, limit, true).await
}

/// Fetch messages strictly older than one opaque cursor and return them oldest-first.
pub(crate) async fn messages_before(
    client: &AuthenticatedRiftClient,
    room_id: &str,
    before_message_id: &str,
    limit: u32,
) -> Result<MessagePage, RiftError> {
    fetch_message_page(
        client,
        room_id,
        Some(("before", before_message_id)),
        limit,
        true,
    )
    .await
}

/// Fetch messages strictly newer than one opaque cursor and return them oldest-first.
pub(crate) async fn messages_after(
    client: &AuthenticatedRiftClient,
    room_id: &str,
    after_message_id: &str,
    limit: u32,
) -> Result<MessagePage, RiftError> {
    fetch_message_page(
        client,
        room_id,
        Some(("after", after_message_id)),
        limit,
        false,
    )
    .await
}

/// Create one text, attachment-only, or combined room message.
pub(crate) async fn create_message(
    client: &AuthenticatedRiftClient,
    room_id: &str,
    content: &str,
    upload_ids: &[String],
) -> Result<RoomMessage, RiftError> {
    if upload_ids
        .iter()
        .enumerate()
        .any(|(index, upload_id)| upload_ids[..index].contains(upload_id))
    {
        return Err(RiftError::Validation(
            "A pending attachment can be used only once per message.".into(),
        ));
    }
    let content = (!content.trim().is_empty()).then_some(content);
    let attachment_ids = (!upload_ids.is_empty()).then_some(upload_ids);
    if content.is_none() && attachment_ids.is_none() {
        return Err(RiftError::Validation(
            "A room message needs text or at least one attachment.".into(),
        ));
    }
    let body = CreateRoomMessageRequest {
        content,
        attachment_ids,
    };
    let request = ReplayableRequest::json(Method::POST, room_messages_path(room_id)?, &body)?;
    let message: ApiRoomMessage = send_json_request(client, &request).await?;
    let message = message.into_room_message(&client.inner.base_url, room_id)?;
    if message.attachments.len() != upload_ids.len() {
        return Err(RiftError::ProtocolContract);
    }
    Ok(message)
}

/// Replace the text of one room-bound message.
pub(crate) async fn edit_message(
    client: &AuthenticatedRiftClient,
    room_id: &str,
    message_id: &str,
    content: &str,
) -> Result<RoomMessage, RiftError> {
    if content.trim().is_empty() {
        return Err(RiftError::Validation(
            "Edited room messages cannot be empty.".into(),
        ));
    }
    let request = ReplayableRequest::json(
        Method::PATCH,
        room_message_path(room_id, message_id)?,
        &EditRoomMessageRequest { content },
    )?;
    let message: ApiRoomMessage = send_json_request(client, &request).await?;
    message.into_room_message(&client.inner.base_url, room_id)
}

/// Delete one room-bound message and require Rift's positive acknowledgement.
pub(crate) async fn delete_message(
    client: &AuthenticatedRiftClient,
    room_id: &str,
    message_id: &str,
) -> Result<(), RiftError> {
    let request = ReplayableRequest::new(
        Method::DELETE,
        room_message_path(room_id, message_id)?,
        ReplayableBody::Empty,
    );
    let deleted: DeleteRoomMessageResponse = send_json_request(client, &request).await?;
    if !deleted.ok {
        return Err(RiftError::ProtocolContract);
    }
    Ok(())
}

/// Read one selected path within the remaining aggregate allowance and discard its directory.
async fn read_upload_part(
    path: &Path,
    remaining_bytes: u64,
) -> Result<NativeUploadPart, RiftError> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| RiftError::Validation("Select a file with a valid display name.".into()))?
        .to_owned();
    validate_display_filename(&filename)
        .map_err(|_| RiftError::Validation("Select a file with a valid display name.".into()))?;
    let owned_path = path.to_owned();
    let (bytes, size_bytes) = tokio::task::spawn_blocking(move || {
        use std::io::Read as _;

        let file = std::fs::File::open(owned_path).map_err(RiftError::FileRead)?;
        let declared_size = file.metadata().map_err(RiftError::FileRead)?.len();
        if declared_size == 0 {
            return Err(RiftError::Validation(
                "Selected attachments cannot be empty.".into(),
            ));
        }
        if declared_size > remaining_bytes {
            return Err(RiftError::Validation(
                "Selected attachments cannot exceed 100 MiB in one upload.".into(),
            ));
        }
        let capacity = usize::try_from(declared_size).map_err(|_| {
            RiftError::Validation(
                "Selected attachments cannot exceed 100 MiB in one upload.".into(),
            )
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(remaining_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(RiftError::FileRead)?;
        let size_bytes = u64::try_from(bytes.len()).map_err(|_| {
            RiftError::Validation(
                "Selected attachments cannot exceed 100 MiB in one upload.".into(),
            )
        })?;
        if size_bytes == 0 {
            return Err(RiftError::Validation(
                "Selected attachments cannot be empty.".into(),
            ));
        }
        if size_bytes > remaining_bytes {
            return Err(RiftError::Validation(
                "Selected attachments cannot exceed 100 MiB in one upload.".into(),
            ));
        }
        Ok((Bytes::from(bytes), size_bytes))
    })
    .await
    .map_err(RiftError::FileTask)??;
    Ok(NativeUploadPart {
        filename,
        bytes,
        size_bytes,
    })
}

/// Read selected paths once while keeping retained retry bytes under one fixed aggregate cap.
async fn read_upload_parts(paths: &[PathBuf]) -> Result<Vec<NativeUploadPart>, RiftError> {
    if paths.is_empty() {
        return Err(RiftError::Validation(
            "Select at least one attachment to upload.".into(),
        ));
    }
    let mut parts = Vec::with_capacity(paths.len());
    let mut remaining_bytes = MAX_NATIVE_UPLOAD_BYTES;
    for path in paths {
        let part = read_upload_part(path, remaining_bytes).await?;
        remaining_bytes = remaining_bytes
            .checked_sub(part.size_bytes)
            .ok_or(RiftError::ProtocolContract)?;
        parts.push(part);
    }
    Ok(parts)
}

/// Upload selected native files and return only opaque, path-free pending metadata.
pub(crate) async fn upload_attachments(
    client: &AuthenticatedRiftClient,
    paths: &[PathBuf],
) -> Result<Vec<PendingRoomAttachment>, RiftError> {
    let parts = read_upload_parts(paths).await?;
    let expected_parts = parts
        .iter()
        .map(|part| (part.filename.clone(), part.size_bytes))
        .collect::<Vec<_>>();
    let request = ReplayableRequest::multipart("api/upload", parts);
    let uploaded: Vec<ApiPendingRoomAttachment> = send_json_request(client, &request).await?;
    if uploaded.len() != expected_parts.len()
        || uploaded.iter().enumerate().any(|(index, attachment)| {
            uploaded[..index]
                .iter()
                .any(|prior| prior.upload_id == attachment.upload_id)
        })
        || uploaded
            .iter()
            .zip(&expected_parts)
            .any(|(attachment, (filename, size_bytes))| {
                attachment.filename != *filename
                    || u64::try_from(attachment.size_bytes).ok() != Some(*size_bytes)
            })
    {
        return Err(RiftError::ProtocolContract);
    }
    uploaded
        .into_iter()
        .map(|attachment| attachment.into_pending_attachment(&client.inner.base_url))
        .collect()
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
        /// Content type supplied for JSON or multipart requests.
        content_type: Option<String>,
        /// Exact request bytes retained for JSON and multipart assertions.
        body: Vec<u8>,
    }

    /// One JSON response returned by the bounded local mock server.
    struct MockResponse {
        /// HTTP response status.
        status: u16,
        /// Optional redirect destination emitted as a Location header.
        location: Option<String>,
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
            Self {
                status,
                location: None,
                body,
            }
        }

        /// Build one redirect response whose destination must never receive native bytes.
        fn redirect(status: u16, location: impl Into<String>) -> Self {
            Self {
                status,
                location: Some(location.into()),
                body: json!({ "error": "redirects are not accepted" }),
            }
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
        let mut content_type = None;
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
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.trim().to_owned());
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
            content_type,
            body,
        });
        let body = response.body.to_string();
        let reason = match response.status {
            200 => "OK",
            307 => "Temporary Redirect",
            401 => "Unauthorized",
            500 => "Internal Server Error",
            _ => "Test Response",
        };
        let location = response
            .location
            .map(|value| format!("Location: {value}\r\n"))
            .unwrap_or_default();
        let encoded = format!(
            "HTTP/1.1 {} {}\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.status,
            reason,
            location,
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
            rift_client_builder()
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

    /// Build one complete Rift message response for native HTTP contract tests.
    fn room_message_response(id: &str, created_at: &str, attachment_url: Option<&str>) -> Value {
        let attachments = attachment_url
            .map(|url| {
                vec![json!({
                    "id": format!("attachment-{id}"),
                    "message_id": id,
                    "filename": "notes.txt",
                    "url": url,
                    "content_type": "text/plain",
                    "size_bytes": 5,
                    "created_at": created_at,
                })]
            })
            .unwrap_or_default();
        json!({
            "id": id,
            "channel_id": "room-1",
            "author_id": "user-1",
            "content": format!("content-{id}"),
            "edited_at": null,
            "created_at": created_at,
            "message_type": "user",
            "author_username": "operator",
            "author_display_name": "Operator",
            "author_avatar_url": null,
            "attachments": attachments,
        })
    }

    /// Report whether one byte slice contains another without assuming UTF-8 file data.
    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
    }

    /// Latest, before, and after responses all cross the native boundary oldest-first.
    #[tokio::test]
    async fn room_http_pages_are_oldest_first_with_exact_cursor_paths() {
        let requests = StdArc::new(AtomicUsize::new(0));
        let observed_requests = StdArc::clone(&requests);
        let server = MockHttpServer::start(3, move |request| {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer current-access")
            );
            match observed_requests.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(request.path, "/api/channels/room-1/messages?limit=2");
                    MockResponse::json(
                        200,
                        json!([
                            room_message_response("message-3", "2026-08-03T12:03:00Z", None),
                            room_message_response("message-2", "2026-08-03T12:03:00Z", None),
                        ]),
                    )
                }
                1 => {
                    assert_eq!(
                        request.path,
                        "/api/channels/room-1/messages?before=message-2&limit=2"
                    );
                    MockResponse::json(
                        200,
                        json!([
                            room_message_response("message-1", "2026-08-03T12:01:00Z", None),
                            room_message_response("message-0", "2026-08-03T12:00:00Z", None),
                        ]),
                    )
                }
                2 => {
                    assert_eq!(
                        request.path,
                        "/api/channels/room-1/messages?after=message-3&limit=2"
                    );
                    MockResponse::json(
                        200,
                        json!([
                            room_message_response("message-4", "2026-08-03T12:04:00Z", None),
                            room_message_response("message-5", "2026-08-03T12:05:00Z", None),
                        ]),
                    )
                }
                unexpected => panic!("unexpected page request index: {unexpected}"),
            }
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let latest = latest_messages(&client, "room-1", 2)
            .await
            .expect("latest room page must parse");
        let before = messages_before(&client, "room-1", "message-2", 2)
            .await
            .expect("older room page must parse");
        let after = messages_after(&client, "room-1", "message-3", 2)
            .await
            .expect("newer room page must parse");
        server.finish();

        assert_eq!(
            latest
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["message-2", "message-3"]
        );
        assert_eq!(
            before
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["message-0", "message-1"]
        );
        assert_eq!(
            after
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["message-4", "message-5"]
        );
        assert!(latest.has_older);
        assert!(before.has_older);
        assert!(!after.has_older);
    }

    /// Rift's stable invalid-cursor code remains distinguishable from other missing resources.
    #[tokio::test]
    async fn room_http_preserves_invalid_after_cursor_code() {
        let server = MockHttpServer::start(1, |request| {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.path,
                "/api/channels/room-1/messages?after=missing&limit=100"
            );
            MockResponse::json(
                404,
                json!({
                    "error": "Message cursor does not exist in this channel",
                    "code": "invalid_message_cursor",
                }),
            )
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let result = messages_after(&client, "room-1", "missing", 100).await;
        server.finish();

        assert!(matches!(result, Err(RiftError::InvalidMessageCursor)));
    }

    /// Other missing-resource codes remain ordinary remote failures.
    #[tokio::test]
    async fn room_http_does_not_reclassify_other_not_found_codes() {
        let server = MockHttpServer::start(1, |_| {
            MockResponse::json(
                404,
                json!({
                    "error": "Channel not found",
                    "code": "channel_not_found",
                }),
            )
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let result = messages_after(&client, "room-1", "missing", 100).await;
        server.finish();

        assert!(matches!(
            result,
            Err(RiftError::Remote {
                status: StatusCode::NOT_FOUND,
                message,
            }) if message == "Channel not found"
        ));
    }

    /// Invalid local page limits never reach Rift's underspecified negative-limit path.
    #[tokio::test]
    async fn room_http_rejects_page_limits_outside_rift_bounds() {
        let endpoint = Url::parse("http://127.0.0.1:9/").expect("test endpoint must parse");
        let client = authenticated_client(&endpoint, "current-access", "refresh-one");

        assert!(matches!(
            latest_messages(&client, "room-1", 0).await,
            Err(RiftError::Validation(_))
        ));
        assert!(matches!(
            latest_messages(&client, "room-1", 101).await,
            Err(RiftError::Validation(_))
        ));
    }

    /// Create omits an empty upload list, preserves multiple IDs, and rejects duplicate IDs.
    #[tokio::test]
    async fn room_http_create_supports_zero_or_multiple_upload_ids() {
        let requests = StdArc::new(AtomicUsize::new(0));
        let observed_requests = StdArc::clone(&requests);
        let server = MockHttpServer::start(2, move |request| {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/api/channels/room-1/messages");
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer current-access")
            );
            let body = serde_json::from_slice::<Value>(&request.body)
                .expect("create request JSON must parse");
            match observed_requests.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(body, json!({ "content": "hello" }));
                    MockResponse::json(
                        200,
                        room_message_response("message-text", "2026-08-03T12:00:00Z", None),
                    )
                }
                1 => {
                    assert_eq!(
                        body,
                        json!({ "attachment_ids": ["upload-alpha", "upload-beta"] })
                    );
                    let mut response = room_message_response(
                        "message-files",
                        "2026-08-03T12:01:00Z",
                        Some("/uploads/staged-alpha"),
                    );
                    response["attachments"]
                        .as_array_mut()
                        .expect("message attachments must be an array")
                        .push(json!({
                            "id": "attachment-beta",
                            "message_id": "message-files",
                            "filename": "diagram.png",
                            "url": "/uploads/staged-beta",
                            "content_type": "image/png",
                            "size_bytes": 7,
                            "created_at": "2026-08-03T12:01:00Z",
                        }));
                    MockResponse::json(200, response)
                }
                unexpected => panic!("unexpected create request index: {unexpected}"),
            }
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let text_message = create_message(&client, "room-1", "hello", &[])
            .await
            .expect("text message create must succeed");
        let upload_ids = vec!["upload-alpha".to_owned(), "upload-beta".to_owned()];
        let file_message = create_message(&client, "room-1", "", &upload_ids)
            .await
            .expect("attachment-only message create must succeed");
        server.finish();

        assert_eq!(text_message.id, "message-text");
        assert_eq!(file_message.id, "message-files");
        assert_eq!(file_message.attachments.len(), 2);

        let duplicate_ids = vec!["upload-alpha".to_owned(), "upload-alpha".to_owned()];
        assert!(matches!(
            create_message(&client, "room-1", "", &duplicate_ids).await,
            Err(RiftError::Validation(_))
        ));
    }

    /// Create rejects Rift's silent partial linkage of pending attachment identifiers.
    #[tokio::test]
    async fn room_http_create_rejects_incomplete_attachment_linkage() {
        let server = MockHttpServer::start(1, |request| {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/api/channels/room-1/messages");
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body)
                    .expect("attachment create JSON must parse"),
                json!({ "attachment_ids": ["upload-alpha", "upload-beta"] })
            );
            MockResponse::json(
                200,
                room_message_response(
                    "message-files",
                    "2026-08-03T12:01:00Z",
                    Some("/uploads/staged-alpha"),
                ),
            )
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");
        let upload_ids = vec!["upload-alpha".to_owned(), "upload-beta".to_owned()];

        let result = create_message(&client, "room-1", "", &upload_ids).await;
        server.finish();

        assert!(matches!(result, Err(RiftError::ProtocolContract)));
    }

    /// Edit and delete bind the message identifier beneath its owning room route.
    #[tokio::test]
    async fn room_http_edit_and_delete_use_room_bound_routes() {
        let requests = StdArc::new(AtomicUsize::new(0));
        let observed_requests = StdArc::clone(&requests);
        let server = MockHttpServer::start(2, move |request| {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer current-access")
            );
            assert_eq!(request.path, "/api/channels/room-1/messages/message-1");
            match observed_requests.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(request.method, "PATCH");
                    assert_eq!(
                        serde_json::from_slice::<Value>(&request.body)
                            .expect("edit request JSON must parse"),
                        json!({ "content": "updated" })
                    );
                    MockResponse::json(
                        200,
                        room_message_response("message-1", "2026-08-03T12:00:00Z", None),
                    )
                }
                1 => {
                    assert_eq!(request.method, "DELETE");
                    assert!(request.body.is_empty());
                    MockResponse::json(200, json!({ "ok": true }))
                }
                unexpected => panic!("unexpected mutation request index: {unexpected}"),
            }
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let edited = edit_message(&client, "room-1", "message-1", "updated")
            .await
            .expect("message edit must succeed");
        delete_message(&client, "room-1", "message-1")
            .await
            .expect("message delete must succeed");
        server.finish();

        assert_eq!(edited.id, "message-1");
    }

    /// Attachment URLs become absolute only when they remain on the configured Rift origin.
    #[tokio::test]
    async fn room_http_resolves_relative_attachment_urls_and_rejects_escapes() {
        let requests = StdArc::new(AtomicUsize::new(0));
        let observed_requests = StdArc::clone(&requests);
        let server = MockHttpServer::start(2, move |request| {
            assert_eq!(request.path, "/api/channels/room-1/messages?limit=1");
            let url = match observed_requests.fetch_add(1, Ordering::SeqCst) {
                0 => "/uploads/safe-object",
                1 => "//outside.example/uploads/escaped-object",
                unexpected => panic!("unexpected attachment request index: {unexpected}"),
            };
            MockResponse::json(
                200,
                json!([room_message_response(
                    "message-1",
                    "2026-08-03T12:00:00Z",
                    Some(url),
                )]),
            )
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");
        let expected_attachment_url = server
            .endpoint
            .join("uploads/safe-object")
            .expect("expected attachment URL must resolve")
            .to_string();

        let page = latest_messages(&client, "room-1", 1)
            .await
            .expect("same-origin attachment page must succeed");
        let escaped = latest_messages(&client, "room-1", 1).await;
        server.finish();

        assert_eq!(page.messages[0].attachments[0].url, expected_attachment_url);
        assert!(matches!(escaped, Err(RiftError::ProtocolContract)));
        assert!(matches!(
            resolve_attachment_url(&client.inner.base_url, "file:///tmp/not-an-attachment"),
            Err(RiftError::ProtocolContract)
        ));
    }

    /// A nested attachment cannot claim ownership by a message other than its container.
    #[tokio::test]
    async fn room_http_rejects_attachment_bound_to_another_message() {
        let server = MockHttpServer::start(1, |request| {
            assert_eq!(request.path, "/api/channels/room-1/messages?limit=1");
            let mut response = room_message_response(
                "message-1",
                "2026-08-03T12:00:00Z",
                Some("/uploads/safe-object"),
            );
            response["attachments"][0]["message_id"] = json!("message-other");
            MockResponse::json(200, json!([response]))
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let result = latest_messages(&client, "room-1", 1).await;
        server.finish();

        assert!(matches!(result, Err(RiftError::ProtocolContract)));
    }

    /// Opaque room, message, and cursor identifiers cannot inject path or query structure.
    #[test]
    fn room_http_encodes_opaque_identifiers_in_routes() {
        assert_eq!(
            room_message_path("room/with space", "message?#")
                .expect("opaque room mutation path must build"),
            "api/channels/room%2Fwith%20space/messages/message%3F%23"
        );
        assert_eq!(
            room_message_page_path(
                "room/with space",
                Some(("before", "message/with?query")),
                25,
            )
            .expect("opaque room page path must build"),
            "api/channels/room%2Fwith%20space/messages?before=message%2Fwith%3Fquery&limit=25"
        );
    }

    /// Upload refuses a cross-origin redirect before selected native bytes can be replayed there.
    #[tokio::test]
    async fn room_http_upload_does_not_follow_cross_origin_redirects() {
        let selected = tempfile::tempdir().expect("upload fixture directory must create");
        let upload_path = selected.path().join("private.txt");
        std::fs::write(&upload_path, b"native-only-body")
            .expect("redirect upload fixture must write");
        let server = MockHttpServer::start(1, |request| {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/api/upload");
            assert!(contains_bytes(&request.body, b"native-only-body"));
            MockResponse::redirect(307, "http://127.0.0.1:9/redirected-upload")
        });
        let client = authenticated_client(&server.endpoint, "current-access", "refresh-one");

        let result = upload_attachments(&client, &[upload_path]).await;
        server.finish();

        assert!(matches!(
            result,
            Err(RiftError::Remote {
                status: StatusCode::TEMPORARY_REDIRECT,
                ..
            })
        ));
    }

    /// One selected file over Rift's hard request ceiling is rejected before it is read.
    #[tokio::test]
    async fn room_http_upload_rejects_oversized_file_before_reading() {
        let selected = tempfile::tempdir().expect("upload fixture directory must create");
        let upload_path = selected.path().join("oversized.bin");
        std::fs::File::create(&upload_path)
            .expect("oversized fixture must create")
            .set_len((100 * 1024 * 1024) + 1)
            .expect("oversized sparse fixture must resize");

        let result = read_upload_parts(&[upload_path]).await;

        assert!(matches!(result, Err(RiftError::Validation(_))));
    }

    /// Multiple selected files cannot exceed Rift's hard request ceiling in aggregate.
    #[tokio::test]
    async fn room_http_upload_rejects_oversized_aggregate_before_reading_it_all() {
        let selected = tempfile::tempdir().expect("upload fixture directory must create");
        let first_path = selected.path().join("first.bin");
        let second_path = selected.path().join("second.bin");
        for path in [&first_path, &second_path] {
            std::fs::File::create(path)
                .expect("aggregate fixture must create")
                .set_len(60 * 1024 * 1024)
                .expect("aggregate sparse fixture must resize");
        }

        let result = read_upload_parts(&[first_path, second_path]).await;

        assert!(matches!(result, Err(RiftError::Validation(_))));
    }

    /// Multipart upload retries rebuild filenames and exact bytes after one token refresh.
    #[tokio::test]
    async fn room_http_upload_rebuilds_multipart_after_refresh_without_paths() {
        let selected = tempfile::tempdir().expect("upload fixture directory must create");
        let text_path = selected.path().join("alpha.txt");
        let binary_path = selected.path().join("beta.bin");
        std::fs::write(&text_path, b"alpha-body").expect("text upload fixture must write");
        std::fs::write(&binary_path, [0_u8, 1, 2, 3, 255])
            .expect("binary upload fixture must write");
        let hidden_parent = selected.path().to_string_lossy().into_owned();
        let requests = StdArc::new(AtomicUsize::new(0));
        let observed_requests = StdArc::clone(&requests);
        let server = MockHttpServer::start(3, move |request| {
            let request_index = observed_requests.fetch_add(1, Ordering::SeqCst);
            match request_index {
                0 | 2 => {
                    assert_eq!(request.method, "POST");
                    assert_eq!(request.path, "/api/upload");
                    let expected_token = match request_index {
                        0 => "Bearer expired-access",
                        2 => "Bearer rotated-access",
                        unexpected => panic!("unexpected upload request index: {unexpected}"),
                    };
                    assert_eq!(request.authorization.as_deref(), Some(expected_token));
                    assert!(request.content_type.as_deref().is_some_and(|value| {
                        value.starts_with("multipart/form-data; boundary=")
                    }));
                    assert!(contains_bytes(&request.body, b"alpha.txt"));
                    assert!(contains_bytes(&request.body, b"alpha-body"));
                    assert!(contains_bytes(&request.body, b"beta.bin"));
                    assert!(contains_bytes(&request.body, &[0_u8, 1, 2, 3, 255]));
                    assert!(!contains_bytes(&request.body, hidden_parent.as_bytes()));
                    if expected_token == "Bearer expired-access" {
                        MockResponse::json(401, json!({ "error": "expired" }))
                    } else {
                        MockResponse::json(
                            200,
                            json!([
                                {
                                    "upload_id": "upload-alpha",
                                    "filename": "alpha.txt",
                                    "url": "/uploads/opaque-alpha",
                                    "content_type": null,
                                    "size_bytes": 10,
                                },
                                {
                                    "upload_id": "upload-beta",
                                    "filename": "beta.bin",
                                    "url": "/uploads/opaque-beta",
                                    "content_type": null,
                                    "size_bytes": 5,
                                }
                            ]),
                        )
                    }
                }
                1 => {
                    assert_eq!(request.method, "POST");
                    assert_eq!(request.path, "/api/auth/refresh");
                    assert_eq!(request.authorization, None);
                    assert_eq!(
                        serde_json::from_slice::<Value>(&request.body)
                            .expect("upload refresh JSON must parse"),
                        json!({ "refresh_token": "refresh-one" })
                    );
                    MockResponse::json(
                        200,
                        refresh_response("rotated-access", "refresh-two", "user-1"),
                    )
                }
                unexpected => panic!("unexpected upload request index: {unexpected}"),
            }
        });
        let client = authenticated_client(&server.endpoint, "expired-access", "refresh-one");

        let pending = upload_attachments(&client, &[text_path, binary_path])
            .await
            .expect("multipart upload replay must succeed");
        server.finish();

        assert_eq!(
            pending
                .iter()
                .map(|attachment| attachment.upload_id.as_str())
                .collect::<Vec<_>>(),
            vec!["upload-alpha", "upload-beta"]
        );
        assert_eq!(pending[0].filename, "alpha.txt");
        assert_eq!(pending[0].size_bytes, 10);
        assert_eq!(pending[1].filename, "beta.bin");
        assert_eq!(pending[1].size_bytes, 5);
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
                        serde_json::from_slice::<Value>(&request.body)
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
                        serde_json::from_slice::<Value>(&request.body)
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
