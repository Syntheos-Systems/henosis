//! Native ownership of Rift WebSocket sessions and room event translation.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message as WebSocketMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::model::{RoomConnectionStatus, RoomConversationEvent, RoomMessage};
use crate::reconcile::{MessagePageSource, ReconcileError, reconcile_open_room};
use crate::rift::{ApiRoomMessage, AuthenticatedRiftClient, GatewayAuthentication, RiftError};
use crate::state::{AppState, RoomLease};

/// Fixed Tauri event used for every sanitized room conversation update.
pub(crate) const ROOM_CONVERSATION_EVENT: &str = "henosis://room-conversation";

/// Maximum unique server identifiers accepted by Rift in one subscription command.
const MAX_SUBSCRIPTION_BATCH: usize = 100;

/// Maximum accepted WebSocket message and frame size before JSON parsing.
const MAX_INBOUND_MESSAGE_BYTES: usize = 2 * 1024 * 1024;

/// Maximum time allowed for DNS, TCP, TLS, and WebSocket setup.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time allowed for Rift to confirm Identify and the exact subscription set.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time allowed for one complete WebSocket protocol write.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// One pending typing signal coalesces every additional signal until the actor receives it.
const OUTBOUND_TYPING_CAPACITY: usize = 1;

/// Initial reconnect delay before bounded jitter.
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);

/// Maximum reconnect delay including jitter.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);

/// Maximum positive jitter is one quarter of the uncapped exponential delay.
const RECONNECT_JITTER_DIVISOR: u32 = 4;

/// Boxed future used by private gateway test seams without another macro dependency.
type GatewayFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Safe setup failures returned before a native gateway actor starts.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RiftGatewayError {
    /// Server subscription identifiers were malformed or exceeded Rift's fixed bound.
    #[error("Rift gateway subscriptions were invalid.")]
    InvalidSubscriptions,
    /// The gateway was started outside a Tokio runtime.
    #[error("The native async runtime is unavailable.")]
    RuntimeUnavailable,
    /// The initial sanitized message could not supply a safe reconnect ordering key.
    #[error("The initial native room message was invalid.")]
    InvalidInitialMessage,
    /// The deferred production actor ended before its snapshot barrier was released.
    #[error("The native room connection ended before startup completed.")]
    StartUnavailable,
}

/// Cancellable owner for exactly one native Rift gateway actor.
pub(crate) struct RiftGateway {
    /// Shared stop signal checked around every transport wait and emission.
    cancellation: CancellationToken,
    /// Bounded coalescing mailbox read only by the sole socket actor.
    typing: mpsc::Sender<()>,
    /// One-shot production barrier released only after the opening snapshot is sealed.
    start: Option<oneshot::Sender<()>>,
    /// Sole actor task aborted if synchronous state teardown cannot await it.
    task: Option<JoinHandle<()>>,
}

/// Stable enqueue failure that exposes no socket or native channel details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RiftGatewayEnqueueError;

/// Lifecycle operations for the sole native gateway actor.
impl RiftGateway {
    /// Release a deferred actor after AppState seals its initial snapshot boundary.
    pub(crate) fn start(&mut self) -> Result<(), RiftGatewayError> {
        let Some(start) = self.start.take() else {
            return Ok(());
        };
        start
            .send(())
            .map_err(|_| RiftGatewayError::StartUnavailable)
    }

    /// Signal cancellation without waiting or holding AppState's mutex.
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Coalesce one typing request into the actor's single bounded mailbox slot.
    pub(crate) fn enqueue_typing(&self) -> Result<(), RiftGatewayEnqueueError> {
        if self.cancellation.is_cancelled() {
            return Err(RiftGatewayEnqueueError);
        }
        match self.typing.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(())) => Err(RiftGatewayEnqueueError),
        }
    }

    /// Cancel and await the actor during deterministic tests or async teardown.
    #[cfg(test)]
    async fn shutdown(mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// Synchronous teardown guarantees no actor remains detached from AppState.
impl Drop for RiftGateway {
    /// Cancel and abort the actor when its sole owner is dropped.
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// WebSocket send and receive operations owned by one actor connection.
trait GatewaySocket: Send {
    /// Send one protocol frame.
    fn send(&mut self, message: WebSocketMessage) -> GatewayFuture<'_, Result<(), WebSocketError>>;

    /// Receive the next protocol frame or end-of-stream marker.
    fn receive(&mut self) -> GatewayFuture<'_, Option<Result<WebSocketMessage, WebSocketError>>>;
}

/// Production tokio-tungstenite socket hidden behind the deterministic test seam.
struct TokioGatewaySocket {
    /// Connected TLS or plain TCP WebSocket stream.
    inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

/// Adapt tokio-tungstenite into the actor's minimal socket contract.
impl GatewaySocket for TokioGatewaySocket {
    /// Forward one complete WebSocket message.
    fn send(&mut self, message: WebSocketMessage) -> GatewayFuture<'_, Result<(), WebSocketError>> {
        Box::pin(async move { self.inner.send(message).await })
    }

    /// Await one complete WebSocket message.
    fn receive(&mut self) -> GatewayFuture<'_, Option<Result<WebSocketMessage, WebSocketError>>> {
        Box::pin(async move { self.inner.next().await })
    }
}

/// Establishes a WebSocket without exposing transport internals to actor tests.
trait GatewayConnector: Send + Sync {
    /// Connect directly to one already validated WebSocket URL.
    fn connect(
        &self,
        url: Url,
    ) -> GatewayFuture<'_, Result<Box<dyn GatewaySocket>, WebSocketError>>;
}

/// Production direct connector with bounded inbound frame allocation.
struct TokioGatewayConnector;

/// Open one direct Rift socket without HTTP redirect handling.
impl GatewayConnector for TokioGatewayConnector {
    /// Connect with the fixed frame and message-size limits.
    fn connect(
        &self,
        url: Url,
    ) -> GatewayFuture<'_, Result<Box<dyn GatewaySocket>, WebSocketError>> {
        Box::pin(async move {
            let (stream, _response) =
                connect_async_with_config(url.as_str(), Some(websocket_config()), false).await?;
            Ok(Box::new(TokioGatewaySocket { inner: stream }) as Box<dyn GatewaySocket>)
        })
    }
}

/// Native session operations needed by the gateway without widening token visibility.
trait GatewaySession: Send + Sync {
    /// Derive the fixed direct WebSocket endpoint.
    fn websocket_url(&self) -> Result<Url, RiftError>;

    /// Return the authenticated user expected in Ready.
    fn expected_user_id(&self) -> String;

    /// Report whether AppState still owns this authenticated session.
    fn is_active(&self) -> bool;

    /// Snapshot the current access token and generation.
    fn authentication(&self) -> GatewayFuture<'_, Result<GatewayAuthentication, RiftError>>;

    /// Refresh one rejected token generation through native single-flight rotation.
    fn refresh(&self, observed_generation: u64) -> GatewayFuture<'_, Result<(), RiftError>>;

    /// Sanitize one message through the existing native HTTP boundary.
    fn sanitize_message(
        &self,
        message: ApiRoomMessage,
        expected_room_id: &str,
    ) -> Result<RoomMessage, RiftError>;
}

/// Keep production gateway authentication inside AuthenticatedRiftClient.
impl GatewaySession for AuthenticatedRiftClient {
    /// Derive Rift's same-service WebSocket URL.
    fn websocket_url(&self) -> Result<Url, RiftError> {
        self.gateway_websocket_url()
    }

    /// Clone the original authenticated identity.
    fn expected_user_id(&self) -> String {
        self.gateway_user_id()
    }

    /// Read the client's process-local invalidation flag.
    fn is_active(&self) -> bool {
        self.gateway_is_active()
    }

    /// Borrow a native-only authentication snapshot.
    fn authentication(&self) -> GatewayFuture<'_, Result<GatewayAuthentication, RiftError>> {
        Box::pin(async move { self.gateway_authentication().await })
    }

    /// Reuse the HTTP client's generation-aware refresh mutex.
    fn refresh(&self, observed_generation: u64) -> GatewayFuture<'_, Result<(), RiftError>> {
        Box::pin(async move {
            self.refresh_gateway_authentication(observed_generation)
                .await
        })
    }

    /// Reuse message and attachment validation from native HTTP responses.
    fn sanitize_message(
        &self,
        message: ApiRoomMessage,
        expected_room_id: &str,
    ) -> Result<RoomMessage, RiftError> {
        self.sanitize_gateway_message(message, expected_room_id)
    }
}

/// Emits only the shared sanitized event contract.
trait GatewayEventSink: Send + Sync {
    /// Deliver one room event without exposing the raw gateway envelope.
    fn emit(&self, event: RoomConversationEvent) -> Result<(), ()>;
}

/// Production adapter for Tauri's fixed application event channel.
struct TauriGatewayEventSink {
    /// Application handle used to publish to every current webview listener.
    app: AppHandle,
    /// Exact room generation that must accept an event before emission.
    lease: RoomLease,
}

/// Restrict Tauri emission to RoomConversationEvent values.
impl GatewayEventSink for TauriGatewayEventSink {
    /// Emit one sanitized event on the fixed native channel.
    fn emit(&self, event: RoomConversationEvent) -> Result<(), ()> {
        let result = self
            .app
            .state::<AppState>()
            .apply_room_event_before_release(&self.lease, &event, |envelope| {
                self.app
                    .emit(ROOM_CONVERSATION_EVENT, envelope.clone())
                    .is_ok()
            });
        match result {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => {
                self.lease.cancellation().cancel();
                Err(())
            }
        }
    }
}

/// Supplies bounded reconnect jitter independently of timing and transport.
trait GatewayJitter: Send + Sync {
    /// Return a millisecond offset no greater than the inclusive bound.
    fn sample_millis(&self, inclusive_upper: u64) -> u64;
}

/// Process-local xorshift jitter source that needs no credential or RNG dependency.
struct ProcessGatewayJitter {
    /// Mutable xorshift state updated atomically by gateway actors.
    state: AtomicU64,
}

/// Construct and sample bounded production jitter.
impl ProcessGatewayJitter {
    /// Seed one actor from the current clock while avoiding xorshift's zero lockup.
    fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15)
            | 1;
        Self {
            state: AtomicU64::new(seed),
        }
    }
}

/// Generate one bounded xorshift sample with lock-free state progression.
impl GatewayJitter for ProcessGatewayJitter {
    /// Sample a deterministic-in-process offset within the caller's cap.
    fn sample_millis(&self, inclusive_upper: u64) -> u64 {
        if inclusive_upper == 0 {
            return 0;
        }
        let mut current = self.state.load(Ordering::Relaxed);
        loop {
            let mut next = current;
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next % (inclusive_upper + 1),
                Err(observed) => current = observed,
            }
        }
    }
}

/// Exponential reconnect state reset only after a usable acknowledged subscription.
struct ReconnectBackoff {
    /// Number of consecutive unsuccessful connection cycles.
    attempt: u32,
}

/// Calculate capped exponential delays with bounded positive jitter.
impl ReconnectBackoff {
    /// Construct a backoff at the initial delay.
    fn new() -> Self {
        Self { attempt: 0 }
    }

    /// Return the next bounded delay and advance the consecutive-failure counter.
    fn next_delay(&mut self, jitter: &dyn GatewayJitter) -> Duration {
        let factor = 1_u32.checked_shl(self.attempt.min(30)).unwrap_or(u32::MAX);
        let base = INITIAL_RECONNECT_DELAY
            .checked_mul(factor)
            .unwrap_or(MAX_RECONNECT_DELAY)
            .min(MAX_RECONNECT_DELAY);
        let jitter_cap =
            (base / RECONNECT_JITTER_DIVISOR).min(MAX_RECONNECT_DELAY.saturating_sub(base));
        let jitter_cap_millis = u64::try_from(jitter_cap.as_millis()).unwrap_or(u64::MAX);
        let delay = base.saturating_add(Duration::from_millis(
            jitter.sample_millis(jitter_cap_millis),
        ));
        self.attempt = self.attempt.saturating_add(1);
        delay.min(MAX_RECONNECT_DELAY)
    }

    /// Restart the sequence after a usable acknowledged subscription.
    fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Runtime dependencies and immutable room scope owned by one actor.
struct GatewayRuntime {
    /// Open room receiving channel-scoped and presence events.
    room_id: String,
    /// Canonical server subscriptions sent after Ready.
    server_ids: Vec<String>,
    /// Authenticated native session and sanitizer.
    session: Arc<dyn GatewaySession>,
    /// HTTP page source used after every acknowledged subscription handoff.
    reconcile_source: Arc<dyn MessagePageSource>,
    /// Latest ordered message cursor known to native state before the actor begins.
    last_known_message: Option<NativeMessageCursor>,
    /// Direct WebSocket connector.
    connector: Arc<dyn GatewayConnector>,
    /// Sanitized room-event destination.
    sink: Arc<dyn GatewayEventSink>,
    /// Bounded reconnect jitter source.
    jitter: Arc<dyn GatewayJitter>,
}

/// Opaque Rift cursor paired with its channel-monotonic server creation time.
#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeMessageCursor {
    /// Opaque message identifier sent back to Rift's after endpoint.
    id: String,
    /// Server-assigned order key used only to prevent local cursor regression.
    created_at: DateTime<Utc>,
}

/// Construct and compare native message cursors without inferring order from opaque IDs.
impl NativeMessageCursor {
    /// Parse one already-sanitized room message into its native cursor state.
    fn from_message(message: &RoomMessage) -> Result<Self, RiftError> {
        if message.id.is_empty() {
            return Err(RiftError::ProtocolContract);
        }
        let created_at = DateTime::parse_from_rfc3339(&message.created_at)
            .map_err(|_| RiftError::ProtocolContract)?
            .with_timezone(&Utc);
        Ok(Self {
            id: message.id.clone(),
            created_at,
        })
    }
}

/// Advance only when Rift's per-channel creation timestamp proves the candidate is newer.
fn advance_message_cursor(
    current: &mut Option<NativeMessageCursor>,
    candidate: NativeMessageCursor,
) {
    if current
        .as_ref()
        .is_none_or(|current| candidate.created_at > current.created_at)
    {
        *current = Some(candidate);
    }
}

/// Client commands accepted by Rift's gateway.
#[derive(Serialize)]
#[serde(tag = "type", content = "data")]
enum GatewayCommand<'a> {
    /// Authenticate the socket before any other text command.
    Identify {
        /// Current short-lived native access token.
        token: &'a str,
    },
    /// Subscribe the authenticated session to its room's Rift servers.
    Subscribe {
        /// Canonical unique server identifiers.
        server_ids: &'a [String],
    },
    /// Notify Rift that the signed-in human is typing in the open room.
    Typing {
        /// Exact channel identifier owned by this gateway actor.
        channel_id: &'a str,
    },
}

/// Minimal envelope header used to distinguish unknown event types safely.
#[derive(Deserialize)]
struct GatewayEnvelopeHeader {
    /// Rift event discriminator.
    #[serde(rename = "type")]
    event_type: String,
}

/// Supported server events parsed only after the discriminator is known.
#[derive(Deserialize)]
#[serde(tag = "type", content = "data")]
enum GatewayInboundEvent {
    /// Confirms authenticated socket identity.
    Ready {
        /// Stable Rift identity for the token.
        user_id: String,
        /// Rift login handle retained only to validate the wire shape.
        username: String,
    },
    /// Confirms the exact canonical server receivers installed for this socket.
    Subscribed {
        /// Canonical server identifiers accepted by Rift without hidden refusals.
        server_ids: Vec<String>,
    },
    /// Carries one complete conversation message.
    MessageCreate(Box<ApiRoomMessage>),
    /// Carries the editable fields of one message.
    MessageUpdate {
        /// Stable message identifier.
        id: String,
        /// Rift channel mapped to the open room.
        channel_id: String,
        /// Replacement message body.
        content: String,
        /// Validated edit timestamp.
        edited_at: DateTime<Utc>,
    },
    /// Removes one message from a room.
    MessageDelete {
        /// Stable message identifier.
        id: String,
        /// Rift channel mapped to the open room.
        channel_id: String,
    },
    /// Starts or refreshes one typing indicator.
    TypingStart {
        /// Rift channel mapped to the open room.
        channel_id: String,
        /// Stable typing user identifier.
        user_id: String,
        /// Typing user's Rift login handle.
        username: String,
    },
    /// Updates one participant across the currently subscribed open room.
    PresenceUpdate {
        /// Stable Rift user identifier.
        user_id: String,
        /// Bounded presence discriminator.
        status: String,
    },
}

/// Result of parsing one text envelope without retaining raw payloads.
enum ParsedGatewayEvent {
    /// One supported event parsed successfully.
    Supported(GatewayInboundEvent),
    /// A safe discriminator identifies an unsupported event.
    Unsupported,
    /// JSON or a supported event's data shape was invalid.
    Malformed,
}

/// Explicit progress through one authenticated subscription handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionPhase {
    /// Identify was sent and the actor is waiting for a valid Ready identity.
    AwaitingReady,
    /// Ready matched and one Subscribe was sent, but Rift has not acknowledged it.
    AwaitingSubscribed,
    /// The exact subscription was acknowledged and reconciliation completed.
    Connected,
}

/// Reason one connected socket returned to the reconnect supervisor.
enum ConnectionOutcome {
    /// AppState canceled or invalidated the actor.
    Cancelled,
    /// Transport ended and may be retried.
    Reconnect {
        /// True only after identity, exact subscription ACK, reconciliation, and Connected.
        reached_connected: bool,
        /// Generation rejected by a clean close before Ready.
        refresh_generation: Option<u64>,
    },
    /// Ready identified a different user and retrying would be unsafe.
    IdentityMismatch,
    /// Reconciliation proved that the native authenticated session is no longer usable.
    AuthenticationFailed,
}

/// Canonicalize subscriptions before opening a token-bearing transport.
fn canonical_server_ids(server_ids: Vec<String>) -> Result<Vec<String>, RiftGatewayError> {
    let mut canonical = BTreeSet::new();
    for server_id in server_ids {
        if server_id.is_empty() || server_id.trim() != server_id {
            return Err(RiftGatewayError::InvalidSubscriptions);
        }
        canonical.insert(server_id);
        if canonical.len() > MAX_SUBSCRIPTION_BATCH {
            return Err(RiftGatewayError::InvalidSubscriptions);
        }
    }
    Ok(canonical.into_iter().collect())
}

/// Build the production WebSocket allocation limits.
fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_INBOUND_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_INBOUND_MESSAGE_BYTES))
}

/// Spawn the sole production gateway for one open room and its latest native cursor.
pub(crate) fn spawn_rift_gateway(
    app: AppHandle,
    lease: RoomLease,
    server_ids: Vec<String>,
    last_known_message: Option<&RoomMessage>,
) -> Result<RiftGateway, RiftGatewayError> {
    let last_known_message = last_known_message
        .map(NativeMessageCursor::from_message)
        .transpose()
        .map_err(|_| RiftGatewayError::InvalidInitialMessage)?;
    let room_id = lease.room_id().to_owned();
    let actor_cancellation = lease.cancellation().clone();
    let session = lease.session().clone();
    let native_session = Arc::new(session);
    let reconcile_source: Arc<dyn MessagePageSource> = native_session.clone();
    let session: Arc<dyn GatewaySession> = native_session;
    let runtime = GatewayRuntime {
        room_id,
        server_ids,
        session,
        reconcile_source,
        last_known_message,
        connector: Arc::new(TokioGatewayConnector),
        sink: Arc::new(TauriGatewayEventSink { app, lease }),
        jitter: Arc::new(ProcessGatewayJitter::new()),
    };
    spawn_gateway_actor_with_cancellation(runtime, actor_cancellation, true)
}

/// Start one actor on the current Tokio runtime.
fn spawn_gateway_actor(runtime: GatewayRuntime) -> Result<RiftGateway, RiftGatewayError> {
    spawn_gateway_actor_with_cancellation(runtime, CancellationToken::new(), false)
}

/// Start one actor using a room generation's shared cancellation signal.
fn spawn_gateway_actor_with_cancellation(
    mut runtime: GatewayRuntime,
    cancellation: CancellationToken,
    deferred_start: bool,
) -> Result<RiftGateway, RiftGatewayError> {
    runtime.server_ids = canonical_server_ids(runtime.server_ids)?;
    let handle =
        tokio::runtime::Handle::try_current().map_err(|_| RiftGatewayError::RuntimeUnavailable)?;
    let actor_cancellation = cancellation.clone();
    let (typing, typing_receiver) = mpsc::channel(OUTBOUND_TYPING_CAPACITY);
    let (start, start_receiver) = if deferred_start {
        let (sender, receiver) = oneshot::channel();
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    let task = handle.spawn(async move {
        if let Some(start_receiver) = start_receiver {
            tokio::select! {
                biased;
                _ = actor_cancellation.cancelled() => return,
                result = start_receiver => {
                    if result.is_err() {
                        return;
                    }
                }
            }
        }
        run_gateway(runtime, actor_cancellation, typing_receiver).await;
    });
    Ok(RiftGateway {
        cancellation,
        typing,
        start,
        task: Some(task),
    })
}

/// Supervise authentication, connection attempts, refresh, and bounded reconnects.
async fn run_gateway(
    runtime: GatewayRuntime,
    cancellation: CancellationToken,
    mut typing: mpsc::Receiver<()>,
) {
    if !emit_status(&runtime, &cancellation, RoomConnectionStatus::Connecting) {
        return;
    }
    let mut backoff = ReconnectBackoff::new();
    let mut last_known_message = runtime.last_known_message.clone();

    loop {
        discard_pending_typing(&mut typing);
        if cancellation.is_cancelled() || !runtime.session.is_active() {
            return;
        }

        let authentication = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = runtime.session.authentication() => match result {
                Ok(authentication) => authentication,
                Err(_) => {
                    let _ = emit_status(
                        &runtime,
                        &cancellation,
                        RoomConnectionStatus::Disconnected,
                    );
                    return;
                }
            },
        };
        let url = match runtime.session.websocket_url() {
            Ok(url) => url,
            Err(_) => {
                let _ = emit_status(&runtime, &cancellation, RoomConnectionStatus::Disconnected);
                return;
            }
        };
        let connection = tokio::time::timeout(CONNECT_TIMEOUT, runtime.connector.connect(url));
        let mut socket = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = connection => match result {
                Ok(Ok(socket)) => socket,
                Ok(Err(_)) => {
                    tracing::debug!(phase = "connect", outcome = "transport", "Rift gateway transport failed");
                    if !wait_for_reconnect(&runtime, &cancellation, &mut backoff).await {
                        return;
                    }
                    continue;
                }
                Err(_) => {
                    tracing::debug!(phase = "connect", outcome = "timeout", "Rift gateway transport timed out");
                    if !wait_for_reconnect(&runtime, &cancellation, &mut backoff).await {
                        return;
                    }
                    continue;
                }
            },
        };

        match run_connection(
            &runtime,
            &cancellation,
            socket.as_mut(),
            &authentication,
            &mut last_known_message,
            &mut typing,
        )
        .await
        {
            ConnectionOutcome::Cancelled => return,
            ConnectionOutcome::IdentityMismatch => {
                tracing::warn!(
                    phase = "ready",
                    "Rift gateway Ready identity did not match the native session"
                );
                let _ = emit_status(&runtime, &cancellation, RoomConnectionStatus::Disconnected);
                return;
            }
            ConnectionOutcome::AuthenticationFailed => {
                tracing::debug!(
                    phase = "reconcile",
                    "Rift gateway reconciliation authentication failed"
                );
                let _ = emit_status(&runtime, &cancellation, RoomConnectionStatus::Disconnected);
                return;
            }
            ConnectionOutcome::Reconnect {
                reached_connected,
                refresh_generation,
            } => {
                if reached_connected {
                    backoff.reset();
                }
                if let Some(generation) = refresh_generation {
                    let refresh_result = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return,
                        result = runtime.session.refresh(generation) => result,
                    };
                    if let Err(error) = refresh_result {
                        if matches!(error, RiftError::Authentication) {
                            let _ = emit_status(
                                &runtime,
                                &cancellation,
                                RoomConnectionStatus::Disconnected,
                            );
                            return;
                        }
                        tracing::debug!(
                            phase = "refresh",
                            "Rift gateway authentication refresh did not complete"
                        );
                    }
                }
                if !wait_for_reconnect(&runtime, &cancellation, &mut backoff).await {
                    return;
                }
            }
        }
    }
}

/// Run one Identify-to-disconnect WebSocket lifecycle.
async fn run_connection(
    runtime: &GatewayRuntime,
    cancellation: &CancellationToken,
    socket: &mut dyn GatewaySocket,
    authentication: &GatewayAuthentication,
    last_known_message: &mut Option<NativeMessageCursor>,
    typing: &mut mpsc::Receiver<()>,
) -> ConnectionOutcome {
    discard_pending_typing(typing);
    let identify = match serde_json::to_string(&GatewayCommand::Identify {
        token: authentication.access_token(),
    }) {
        Ok(identify) => identify,
        Err(_) => {
            tracing::warn!(phase = "identify", "Rift gateway could not encode Identify");
            return ConnectionOutcome::Reconnect {
                reached_connected: false,
                refresh_generation: None,
            };
        }
    };
    if send_frame(
        socket,
        cancellation,
        WebSocketMessage::Text(identify.into()),
    )
    .await
    .is_err()
    {
        return if cancellation.is_cancelled() {
            ConnectionOutcome::Cancelled
        } else {
            ConnectionOutcome::Reconnect {
                reached_connected: false,
                refresh_generation: None,
            }
        };
    }

    let handshake_deadline = tokio::time::sleep(HANDSHAKE_TIMEOUT);
    tokio::pin!(handshake_deadline);
    let mut phase = ConnectionPhase::AwaitingReady;
    loop {
        let received = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return ConnectionOutcome::Cancelled,
            _ = &mut handshake_deadline, if phase != ConnectionPhase::Connected => {
                tracing::debug!(phase = "handshake", outcome = "timeout", "Rift gateway handshake timed out");
                return ConnectionOutcome::Reconnect {
                    reached_connected: false,
                    refresh_generation: None,
                };
            }
            typing_request = typing.recv() => {
                if typing_request.is_none() {
                    return ConnectionOutcome::Cancelled;
                }
                if phase != ConnectionPhase::Connected {
                    continue;
                }
                let typing_command = match serde_json::to_string(&GatewayCommand::Typing {
                    channel_id: &runtime.room_id,
                }) {
                    Ok(command) => command,
                    Err(_) => {
                        return ConnectionOutcome::Reconnect {
                            reached_connected: true,
                            refresh_generation: None,
                        };
                    }
                };
                if send_frame(
                    socket,
                    cancellation,
                    WebSocketMessage::Text(typing_command.into()),
                )
                .await
                .is_err()
                {
                    return if cancellation.is_cancelled() {
                        ConnectionOutcome::Cancelled
                    } else {
                        ConnectionOutcome::Reconnect {
                            reached_connected: true,
                            refresh_generation: None,
                        }
                    };
                }
                continue;
            }
            received = socket.receive() => received,
        };
        let message = match received {
            Some(Ok(message)) => message,
            Some(Err(_)) => {
                tracing::debug!(phase = "read", "Rift gateway transport failed");
                return ConnectionOutcome::Reconnect {
                    reached_connected: phase == ConnectionPhase::Connected,
                    refresh_generation: None,
                };
            }
            None => {
                return ConnectionOutcome::Reconnect {
                    reached_connected: phase == ConnectionPhase::Connected,
                    refresh_generation: (phase == ConnectionPhase::AwaitingReady)
                        .then_some(authentication.token_generation()),
                };
            }
        };

        match message {
            WebSocketMessage::Text(text) => {
                let parsed = parse_gateway_text(text.as_str());
                match parsed {
                    ParsedGatewayEvent::Malformed => {
                        tracing::debug!(
                            phase = "parse",
                            text_bytes = text.len(),
                            "Ignored malformed Rift gateway text"
                        );
                    }
                    ParsedGatewayEvent::Unsupported => {
                        tracing::debug!(
                            phase = "parse",
                            text_bytes = text.len(),
                            "Ignored unsupported Rift gateway event"
                        );
                    }
                    ParsedGatewayEvent::Supported(GatewayInboundEvent::Ready {
                        user_id,
                        username,
                    }) => {
                        if phase != ConnectionPhase::AwaitingReady {
                            tracing::debug!(
                                phase = "ready",
                                "Ignored duplicate Rift gateway Ready"
                            );
                            continue;
                        }
                        if user_id.is_empty() || username.is_empty() {
                            tracing::debug!(
                                phase = "ready",
                                "Ignored malformed Rift gateway Ready"
                            );
                            continue;
                        }
                        if user_id != runtime.session.expected_user_id() {
                            return ConnectionOutcome::IdentityMismatch;
                        }
                        let subscribe = match serde_json::to_string(&GatewayCommand::Subscribe {
                            server_ids: &runtime.server_ids,
                        }) {
                            Ok(subscribe) => subscribe,
                            Err(_) => {
                                return ConnectionOutcome::Reconnect {
                                    reached_connected: false,
                                    refresh_generation: None,
                                };
                            }
                        };
                        if send_frame(
                            socket,
                            cancellation,
                            WebSocketMessage::Text(subscribe.into()),
                        )
                        .await
                        .is_err()
                        {
                            return if cancellation.is_cancelled() {
                                ConnectionOutcome::Cancelled
                            } else {
                                ConnectionOutcome::Reconnect {
                                    reached_connected: false,
                                    refresh_generation: None,
                                }
                            };
                        }
                        phase = ConnectionPhase::AwaitingSubscribed;
                    }
                    ParsedGatewayEvent::Supported(GatewayInboundEvent::Subscribed {
                        server_ids,
                    }) => {
                        if phase == ConnectionPhase::AwaitingReady {
                            tracing::debug!(
                                phase = "subscribed",
                                "Rift gateway acknowledged subscriptions before Ready"
                            );
                            return ConnectionOutcome::Reconnect {
                                reached_connected: false,
                                refresh_generation: None,
                            };
                        }
                        if phase == ConnectionPhase::Connected {
                            tracing::debug!(
                                phase = "subscribed",
                                "Ignored duplicate Rift gateway subscription acknowledgement"
                            );
                            continue;
                        }
                        if server_ids != runtime.server_ids {
                            tracing::debug!(
                                phase = "subscribed",
                                "Rift gateway acknowledged a different subscription set"
                            );
                            return ConnectionOutcome::Reconnect {
                                reached_connected: false,
                                refresh_generation: None,
                            };
                        }
                        if let Err(outcome) = reconcile_subscription_handoff(
                            runtime,
                            cancellation,
                            last_known_message,
                        )
                        .await
                        {
                            return outcome;
                        }
                        discard_pending_typing(typing);
                        phase = ConnectionPhase::Connected;
                        if !emit_status(runtime, cancellation, RoomConnectionStatus::Connected) {
                            return ConnectionOutcome::Cancelled;
                        }
                    }
                    ParsedGatewayEvent::Supported(event) if phase != ConnectionPhase::Connected => {
                        tracing::debug!(
                            phase = "pre_connected",
                            "Ignored Rift gateway event before subscription acknowledgement"
                        );
                        drop(event);
                    }
                    ParsedGatewayEvent::Supported(event) => {
                        if let Some(event) = translate_event(runtime, event) {
                            let created_message_cursor = match &event {
                                RoomConversationEvent::MessageCreate { message, .. } => {
                                    match NativeMessageCursor::from_message(message) {
                                        Ok(cursor) => Some(cursor),
                                        Err(_) => {
                                            tracing::debug!(
                                                phase = "event",
                                                "Rift gateway message had an invalid cursor"
                                            );
                                            return ConnectionOutcome::Reconnect {
                                                reached_connected: true,
                                                refresh_generation: None,
                                            };
                                        }
                                    }
                                }
                                _ => None,
                            };
                            if !emit_event(runtime, cancellation, event) {
                                return ConnectionOutcome::Cancelled;
                            }
                            if let Some(created_message_cursor) = created_message_cursor {
                                advance_message_cursor(last_known_message, created_message_cursor);
                            }
                        }
                    }
                }
            }
            WebSocketMessage::Ping(payload) => {
                if send_frame(socket, cancellation, WebSocketMessage::Pong(payload))
                    .await
                    .is_err()
                {
                    return if cancellation.is_cancelled() {
                        ConnectionOutcome::Cancelled
                    } else {
                        ConnectionOutcome::Reconnect {
                            reached_connected: phase == ConnectionPhase::Connected,
                            refresh_generation: None,
                        }
                    };
                }
            }
            WebSocketMessage::Close(_) => {
                return ConnectionOutcome::Reconnect {
                    reached_connected: phase == ConnectionPhase::Connected,
                    refresh_generation: (phase == ConnectionPhase::AwaitingReady)
                        .then_some(authentication.token_generation()),
                };
            }
            WebSocketMessage::Binary(_)
            | WebSocketMessage::Pong(_)
            | WebSocketMessage::Frame(_) => {}
        }
    }
}

/// Reconcile every acknowledged subscription from the latest native or room-start cursor.
async fn reconcile_subscription_handoff(
    runtime: &GatewayRuntime,
    cancellation: &CancellationToken,
    last_known_message: &mut Option<NativeMessageCursor>,
) -> Result<(), ConnectionOutcome> {
    let after_cursor = last_known_message
        .as_ref()
        .map_or(runtime.room_id.as_str(), |cursor| cursor.id.as_str());
    let reconciliation = match reconcile_open_room(
        runtime.reconcile_source.as_ref(),
        &runtime.room_id,
        Some(after_cursor),
        cancellation,
    )
    .await
    {
        Ok(reconciliation) => reconciliation,
        Err(ReconcileError::Cancelled) => return Err(ConnectionOutcome::Cancelled),
        Err(ReconcileError::Rift(RiftError::Authentication)) => {
            return Err(ConnectionOutcome::AuthenticationFailed);
        }
        Err(ReconcileError::Rift(_)) => {
            tracing::debug!(
                phase = "reconcile",
                "Rift gateway reconciliation did not complete"
            );
            return Err(ConnectionOutcome::Reconnect {
                reached_connected: false,
                refresh_generation: None,
            });
        }
    };
    let next_cursor = match reconciliation
        .page
        .messages
        .last()
        .map(NativeMessageCursor::from_message)
        .transpose()
    {
        Ok(next_cursor) => next_cursor,
        Err(_) => {
            tracing::debug!(
                phase = "reconcile",
                "Rift gateway reconciliation returned an invalid cursor"
            );
            return Err(ConnectionOutcome::Reconnect {
                reached_connected: false,
                refresh_generation: None,
            });
        }
    };
    let should_emit =
        reconciliation.replace_live_window || !reconciliation.page.messages.is_empty();
    if should_emit
        && !emit_event(
            runtime,
            cancellation,
            RoomConversationEvent::Reconciliation {
                room_id: runtime.room_id.clone(),
                page: reconciliation.page,
                replace_live_window: reconciliation.replace_live_window,
            },
        )
    {
        return Err(ConnectionOutcome::Cancelled);
    }
    if reconciliation.replace_live_window {
        *last_known_message = next_cursor;
    } else if let Some(next_cursor) = next_cursor {
        advance_message_cursor(last_known_message, next_cursor);
    }
    Ok(())
}

/// Drop every typing signal captured outside the current acknowledged connection.
fn discard_pending_typing(typing: &mut mpsc::Receiver<()>) {
    while typing.try_recv().is_ok() {}
}

/// Send one frame with cancellation priority.
async fn send_frame(
    socket: &mut dyn GatewaySocket,
    cancellation: &CancellationToken,
    message: WebSocketMessage,
) -> Result<(), ()> {
    let send = tokio::time::timeout(WRITE_TIMEOUT, socket.send(message));
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(()),
        result = send => result.map_err(|_| ())?.map_err(|_| ()),
    }
}

/// Parse one known envelope while distinguishing unsupported discriminators.
fn parse_gateway_text(text: &str) -> ParsedGatewayEvent {
    let header = match serde_json::from_str::<GatewayEnvelopeHeader>(text) {
        Ok(header) => header,
        Err(_) => return ParsedGatewayEvent::Malformed,
    };
    if !matches!(
        header.event_type.as_str(),
        "Ready"
            | "Subscribed"
            | "MessageCreate"
            | "MessageUpdate"
            | "MessageDelete"
            | "TypingStart"
            | "PresenceUpdate"
    ) {
        return ParsedGatewayEvent::Unsupported;
    }
    serde_json::from_str::<GatewayInboundEvent>(text)
        .map(ParsedGatewayEvent::Supported)
        .unwrap_or(ParsedGatewayEvent::Malformed)
}

/// Convert one supported post-subscription event into the sanitized shared model.
fn translate_event(
    runtime: &GatewayRuntime,
    event: GatewayInboundEvent,
) -> Option<RoomConversationEvent> {
    match event {
        GatewayInboundEvent::Ready { .. } | GatewayInboundEvent::Subscribed { .. } => {
            tracing::debug!(
                phase = "handshake",
                "Ignored duplicate Rift gateway handshake event"
            );
            None
        }
        GatewayInboundEvent::MessageCreate(message) => runtime
            .session
            .sanitize_message(*message, &runtime.room_id)
            .map(|message| RoomConversationEvent::MessageCreate {
                room_id: runtime.room_id.clone(),
                message,
            })
            .map_err(|_| {
                tracing::debug!(
                    event_type = "MessageCreate",
                    "Ignored invalid Rift gateway event"
                );
            })
            .ok(),
        GatewayInboundEvent::MessageUpdate {
            id,
            channel_id,
            content,
            edited_at,
        } if valid_scoped_fields(&runtime.room_id, &channel_id, &[&id]) => {
            Some(RoomConversationEvent::MessageUpdate {
                room_id: channel_id,
                message_id: id,
                content,
                edited_at: edited_at.to_rfc3339(),
            })
        }
        GatewayInboundEvent::MessageDelete { id, channel_id }
            if valid_scoped_fields(&runtime.room_id, &channel_id, &[&id]) =>
        {
            Some(RoomConversationEvent::MessageDelete {
                room_id: channel_id,
                message_id: id,
            })
        }
        GatewayInboundEvent::TypingStart {
            channel_id,
            user_id,
            username,
        } if valid_scoped_fields(&runtime.room_id, &channel_id, &[&user_id, &username]) => {
            Some(RoomConversationEvent::TypingStart {
                room_id: channel_id,
                user_id,
                username,
            })
        }
        GatewayInboundEvent::PresenceUpdate { user_id, status }
            if !user_id.is_empty() && valid_presence_status(&status) =>
        {
            Some(RoomConversationEvent::PresenceUpdate {
                room_id: runtime.room_id.clone(),
                user_id,
                status,
            })
        }
        _ => {
            tracing::debug!(
                phase = "translate",
                "Ignored out-of-scope Rift gateway event"
            );
            None
        }
    }
}

/// Validate channel scope and required opaque string fields.
fn valid_scoped_fields(expected_room_id: &str, channel_id: &str, fields: &[&str]) -> bool {
    channel_id == expected_room_id && fields.iter().all(|field| !field.is_empty())
}

/// Bound an extensible presence discriminator to safe metadata characters.
fn valid_presence_status(status: &str) -> bool {
    !status.is_empty()
        && status.len() <= 32
        && status
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Emit one event only while the actor and authenticated session remain current.
fn emit_event(
    runtime: &GatewayRuntime,
    cancellation: &CancellationToken,
    event: RoomConversationEvent,
) -> bool {
    if cancellation.is_cancelled() || !runtime.session.is_active() {
        return false;
    }
    if runtime.sink.emit(event).is_err() {
        tracing::debug!(phase = "emit", "Rift gateway event had no active recipient");
        cancellation.cancel();
        return false;
    }
    true
}

/// Emit one typed room connection status through the same fixed event contract.
fn emit_status(
    runtime: &GatewayRuntime,
    cancellation: &CancellationToken,
    status: RoomConnectionStatus,
) -> bool {
    emit_event(
        runtime,
        cancellation,
        RoomConversationEvent::ConnectionChanged {
            room_id: runtime.room_id.clone(),
            status,
        },
    )
}

/// Emit Reconnecting and await one cancellable bounded delay.
async fn wait_for_reconnect(
    runtime: &GatewayRuntime,
    cancellation: &CancellationToken,
    backoff: &mut ReconnectBackoff,
) -> bool {
    if !emit_status(runtime, cancellation, RoomConnectionStatus::Reconnecting) {
        return false;
    }
    let delay = backoff.next_delay(runtime.jitter.as_ref());
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        _ = tokio::time::sleep(delay) => {
            !cancellation.is_cancelled() && runtime.session.is_active()
        }
    }
}

#[cfg(test)]
/// Construct an inert gateway whose cancellation can be observed by AppState tests.
pub(crate) fn test_rift_gateway() -> (RiftGateway, CancellationToken) {
    let cancellation = CancellationToken::new();
    let actor_cancellation = cancellation.clone();
    let (typing, typing_receiver) = mpsc::channel(OUTBOUND_TYPING_CAPACITY);
    let task = tokio::spawn(async move {
        let _typing_receiver = typing_receiver;
        actor_cancellation.cancelled().await;
    });
    (
        RiftGateway {
            cancellation: cancellation.clone(),
            typing,
            start: None,
            task: Some(task),
        },
        cancellation,
    )
}

#[cfg(test)]
/// Construct a current-looking gateway whose outbound mailbox has closed.
pub(crate) fn test_closed_rift_gateway() -> RiftGateway {
    let cancellation = CancellationToken::new();
    let (typing, typing_receiver) = mpsc::channel(OUTBOUND_TYPING_CAPACITY);
    drop(typing_receiver);
    RiftGateway {
        cancellation,
        typing,
        start: None,
        task: None,
    }
}

#[cfg(test)]
/// Exercises the native gateway protocol and lifecycle contract.
mod tests {
    use std::collections::VecDeque;
    use std::future::pending;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use futures_util::future::BoxFuture;
    use serde_json::{Value, json};
    use tokio::sync::{Notify, mpsc};

    use super::*;
    use crate::model::MessagePage;
    use crate::reconcile::MessagePageSource;

    /// One in-memory socket endpoint owned by the gateway actor.
    struct FakeSocket {
        /// Frames supplied by the fake Rift peer.
        inbound: mpsc::UnboundedReceiver<Result<WebSocketMessage, WebSocketError>>,
        /// Frames written by the gateway actor.
        outbound: mpsc::UnboundedSender<WebSocketMessage>,
        /// True when every socket write must remain pending.
        pending_send: bool,
    }

    /// Adapt in-memory channels to the gateway socket contract.
    impl GatewaySocket for FakeSocket {
        /// Record one outbound frame for protocol assertions.
        fn send(
            &mut self,
            message: WebSocketMessage,
        ) -> GatewayFuture<'_, Result<(), WebSocketError>> {
            if self.pending_send {
                return Box::pin(std::future::pending());
            }
            let result = self
                .outbound
                .send(message)
                .map_err(|_| WebSocketError::ConnectionClosed);
            Box::pin(async move { result })
        }

        /// Await one frame supplied by the fake peer.
        fn receive(
            &mut self,
        ) -> GatewayFuture<'_, Option<Result<WebSocketMessage, WebSocketError>>> {
            Box::pin(async move { self.inbound.recv().await })
        }
    }

    /// Test-side peer used to drive and inspect one in-memory socket.
    struct FakePeer {
        /// Channel used to supply frames to the gateway.
        inbound: mpsc::UnboundedSender<Result<WebSocketMessage, WebSocketError>>,
        /// Channel receiving frames written by the gateway.
        outbound: mpsc::UnboundedReceiver<WebSocketMessage>,
    }

    /// Protocol-driving operations for one fake Rift peer.
    impl FakePeer {
        /// Send one raw WebSocket message to the gateway actor.
        fn send(&self, message: WebSocketMessage) {
            self.inbound
                .send(Ok(message))
                .expect("gateway socket must remain open");
        }

        /// Serialize and send one Rift JSON envelope.
        fn send_json(&self, value: Value) {
            self.send(WebSocketMessage::Text(value.to_string().into()));
        }

        /// Receive and parse the next outbound text command.
        async fn next_json(&mut self) -> Value {
            let message = self
                .outbound
                .recv()
                .await
                .expect("gateway must send a protocol frame");
            let WebSocketMessage::Text(text) = message else {
                panic!("gateway frame must be text");
            };
            serde_json::from_str(text.as_str()).expect("gateway text must be valid JSON")
        }

        /// Receive the next outbound frame without assuming its type.
        async fn next_frame(&mut self) -> WebSocketMessage {
            self.outbound
                .recv()
                .await
                .expect("gateway must send a protocol frame")
        }
    }

    /// Construct one connected in-memory socket and its controlling peer.
    fn fake_socket_pair() -> (FakeSocket, FakePeer) {
        let (inbound_sender, inbound) = mpsc::unbounded_channel();
        let (outbound, outbound_receiver) = mpsc::unbounded_channel();
        (
            FakeSocket {
                inbound,
                outbound,
                pending_send: false,
            },
            FakePeer {
                inbound: inbound_sender,
                outbound: outbound_receiver,
            },
        )
    }

    /// Send one valid Ready envelope for the fake authenticated identity.
    fn send_ready(peer: &FakePeer) {
        peer.send_json(json!({
            "type": "Ready",
            "data": {"user_id": "user-1", "username": "gateway-user"}
        }));
    }

    /// Send one typed subscription acknowledgement with the supplied exact wire order.
    fn send_subscription_ack(peer: &FakePeer, server_ids: &[&str]) {
        peer.send_json(json!({
            "type": "Subscribed",
            "data": {"server_ids": server_ids}
        }));
    }

    /// Complete one exact Ready, Subscribe, and Subscribed exchange.
    async fn complete_subscription_handshake(peer: &mut FakePeer, server_ids: &[&str]) {
        send_ready(peer);
        assert_eq!(
            peer.next_json().await,
            json!({"type": "Subscribe", "data": {"server_ids": server_ids}})
        );
        send_subscription_ack(peer, server_ids);
    }

    /// One scripted result for a connector invocation.
    enum ConnectScript {
        /// Return one connected in-memory socket.
        Socket(FakeSocket),
        /// Return a transport failure immediately.
        Fail,
        /// Remain pending until actor cancellation drops the future.
        Pending,
    }

    /// Deterministic connector that records every requested URL.
    struct FakeConnector {
        /// Ordered connection outcomes.
        scripts: StdMutex<VecDeque<ConnectScript>>,
        /// Requested URLs visible to each test.
        attempts: mpsc::UnboundedSender<Url>,
    }

    /// Replay scripted connection outcomes without a network listener.
    impl GatewayConnector for FakeConnector {
        /// Record the URL and return the next scripted outcome.
        fn connect(
            &self,
            url: Url,
        ) -> GatewayFuture<'_, Result<Box<dyn GatewaySocket>, WebSocketError>> {
            self.attempts
                .send(url)
                .expect("test must retain its attempt receiver");
            let script = self
                .scripts
                .lock()
                .expect("connector script lock must remain healthy")
                .pop_front()
                .unwrap_or(ConnectScript::Fail);
            Box::pin(async move {
                match script {
                    ConnectScript::Socket(socket) => Ok(Box::new(socket) as Box<dyn GatewaySocket>),
                    ConnectScript::Fail => Err(WebSocketError::ConnectionClosed),
                    ConnectScript::Pending => std::future::pending().await,
                }
            })
        }
    }

    /// Construct one scripted connector and its attempt receiver.
    fn fake_connector(
        scripts: Vec<ConnectScript>,
    ) -> (Arc<FakeConnector>, mpsc::UnboundedReceiver<Url>) {
        let (attempts, attempt_receiver) = mpsc::unbounded_channel();
        (
            Arc::new(FakeConnector {
                scripts: StdMutex::new(scripts.into()),
                attempts,
            }),
            attempt_receiver,
        )
    }

    /// Mutable token state retained only by a fake native session.
    struct FakeTokenState {
        /// Current short-lived access token.
        access_token: String,
        /// Current monotonic token generation.
        generation: u64,
    }

    /// Native session fake supporting token rotation and real message sanitization.
    struct FakeSession {
        /// HTTP origin used by attachment sanitization.
        base_url: Url,
        /// Identity that Ready must confirm.
        user_id: String,
        /// Token pair state observed by Identify and refresh.
        tokens: StdMutex<FakeTokenState>,
        /// False after a test simulates AppState invalidation.
        active: AtomicBool,
        /// Count of generation-aware refresh calls.
        refreshes: AtomicUsize,
        /// Notification that one refresh call completed.
        refresh_completed: Notify,
    }

    /// Construction and observations for one fake native session.
    impl FakeSession {
        /// Construct an active HTTPS session with generation zero.
        fn new() -> Self {
            Self {
                base_url: Url::parse("https://rift.example/").expect("fake Rift origin must parse"),
                user_id: "user-1".into(),
                tokens: StdMutex::new(FakeTokenState {
                    access_token: "access-one".into(),
                    generation: 0,
                }),
                active: AtomicBool::new(true),
                refreshes: AtomicUsize::new(0),
                refresh_completed: Notify::new(),
            }
        }

        /// Wait until the actor completes one refresh attempt.
        async fn wait_for_refresh(&self) {
            self.refresh_completed.notified().await;
        }

        /// Return the number of refresh calls made by the actor.
        fn refresh_count(&self) -> usize {
            self.refreshes.load(Ordering::Acquire)
        }
    }

    /// Supply native authentication and sanitizer behavior to actor tests.
    impl GatewaySession for FakeSession {
        /// Return the direct fixed test WebSocket endpoint.
        fn websocket_url(&self) -> Result<Url, RiftError> {
            Url::parse("wss://rift.example/ws").map_err(|_| RiftError::ProtocolContract)
        }

        /// Clone the fake authenticated user identifier.
        fn expected_user_id(&self) -> String {
            self.user_id.clone()
        }

        /// Read the fake invalidation flag.
        fn is_active(&self) -> bool {
            self.active.load(Ordering::Acquire)
        }

        /// Clone the current fake access token and generation.
        fn authentication(&self) -> GatewayFuture<'_, Result<GatewayAuthentication, RiftError>> {
            let tokens = self
                .tokens
                .lock()
                .expect("fake token lock must remain healthy");
            let authentication = GatewayAuthentication::for_gateway_test(
                tokens.access_token.clone(),
                tokens.generation,
            );
            Box::pin(async move { Ok(authentication) })
        }

        /// Rotate generation zero to the second test token exactly once.
        fn refresh(&self, observed_generation: u64) -> GatewayFuture<'_, Result<(), RiftError>> {
            Box::pin(async move {
                self.refreshes.fetch_add(1, Ordering::AcqRel);
                let mut tokens = self
                    .tokens
                    .lock()
                    .expect("fake token lock must remain healthy");
                if tokens.generation == observed_generation {
                    tokens.generation = tokens.generation.saturating_add(1);
                    tokens.access_token = "access-two".into();
                }
                drop(tokens);
                self.refresh_completed.notify_one();
                Ok(())
            })
        }

        /// Apply the production message and attachment sanitizer to fake events.
        fn sanitize_message(
            &self,
            message: ApiRoomMessage,
            expected_room_id: &str,
        ) -> Result<RoomMessage, RiftError> {
            message.into_room_message(&self.base_url, expected_room_id)
        }
    }

    /// Typed in-memory event sink with one optional simulated delivery failure.
    struct RecordingSink {
        /// Channel receiving successfully delivered room events.
        events: mpsc::UnboundedSender<RoomConversationEvent>,
        /// True when the next delivery must fail.
        fail_next: AtomicBool,
    }

    /// Capture only RoomConversationEvent values for assertions.
    impl GatewayEventSink for RecordingSink {
        /// Record one event or consume the configured one-shot failure.
        fn emit(&self, event: RoomConversationEvent) -> Result<(), ()> {
            if self.fail_next.swap(false, Ordering::AcqRel) {
                return Err(());
            }
            self.events.send(event).map_err(|_| ())
        }
    }

    /// Apply actor events through real native room state before recording deliveries.
    struct StateBackedRecordingSink {
        /// Native room state that performs ordering and identifier deduplication.
        state: Arc<AppState>,
        /// Exact room generation allowed to accept actor events.
        lease: RoomLease,
        /// Effective events released after native state mutation.
        events: mpsc::UnboundedSender<RoomConversationEvent>,
    }

    /// Exercise the production state-before-release ordering contract without Tauri.
    impl GatewayEventSink for StateBackedRecordingSink {
        /// Apply one actor event and record only its effective released form.
        fn emit(&self, event: RoomConversationEvent) -> Result<(), ()> {
            match self
                .state
                .apply_room_event_before_release(&self.lease, &event, |envelope| {
                    self.events.send(envelope.event.clone()).is_ok()
                }) {
                Ok(true) => Ok(()),
                Ok(false) | Err(_) => Err(()),
            }
        }
    }

    /// Construct a recording sink and its event receiver.
    fn recording_sink() -> (
        Arc<RecordingSink>,
        mpsc::UnboundedReceiver<RoomConversationEvent>,
    ) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(RecordingSink {
                events,
                fail_next: AtomicBool::new(false),
            }),
            receiver,
        )
    }

    /// Construct a state-backed event sink and its effective-event receiver.
    fn state_backed_recording_sink(
        state: Arc<AppState>,
        lease: RoomLease,
    ) -> (
        Arc<StateBackedRecordingSink>,
        mpsc::UnboundedReceiver<RoomConversationEvent>,
    ) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(StateBackedRecordingSink {
                state,
                lease,
                events,
            }),
            receiver,
        )
    }

    /// Deterministic reconnect jitter fixed to one bounded sample.
    struct FixedJitter {
        /// Requested sample before clamping to the supplied bound.
        millis: u64,
    }

    /// Return deterministic bounded jitter to paused-time tests.
    impl GatewayJitter for FixedJitter {
        /// Clamp the fixed sample to the actor's inclusive upper bound.
        fn sample_millis(&self, inclusive_upper: u64) -> u64 {
            self.millis.min(inclusive_upper)
        }
    }

    /// One page request observed by the gateway reconciliation seam.
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum GatewayPageRequest {
        /// One newest-window request.
        Latest {
            /// Requested room identifier.
            room_id: String,
            /// Requested bounded page size.
            limit: u32,
        },
        /// One forward request after an opaque message cursor.
        After {
            /// Requested room identifier.
            room_id: String,
            /// Opaque cursor supplied by the gateway actor.
            cursor: String,
            /// Requested bounded page size.
            limit: u32,
        },
    }

    /// One deterministic reconciliation response used by gateway lifecycle tests.
    enum GatewayPageReply {
        /// Return one sanitized page immediately.
        Page(MessagePage),
        /// Remain pending until room replacement cancels the actor.
        Pending,
    }

    /// Scripted page source that records the gateway actor's cursor progression.
    struct FakeMessagePageSource {
        /// Ordered replies consumed by latest and after requests alike.
        replies: StdMutex<VecDeque<GatewayPageReply>>,
        /// Ordered requests made by the gateway actor.
        requests: StdMutex<Vec<GatewayPageRequest>>,
        /// Notification released whenever one request is constructed.
        request_started: Notify,
    }

    /// Construct and inspect one gateway-specific reconciliation source.
    impl FakeMessagePageSource {
        /// Create a source from its ordered replies.
        fn new(replies: impl IntoIterator<Item = GatewayPageReply>) -> Self {
            Self {
                replies: StdMutex::new(replies.into_iter().collect()),
                requests: StdMutex::new(Vec::new()),
                request_started: Notify::new(),
            }
        }

        /// Wait until the gateway constructs its next page request.
        async fn wait_for_request(&self) {
            self.request_started.notified().await;
        }

        /// Clone every request observed so far.
        fn requests(&self) -> Vec<GatewayPageRequest> {
            self.requests
                .lock()
                .expect("gateway page request lock must remain healthy")
                .clone()
        }

        /// Record one request and return its scripted future.
        fn respond<'a>(
            &'a self,
            request: GatewayPageRequest,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            self.requests
                .lock()
                .expect("gateway page request lock must remain healthy")
                .push(request);
            let reply = self
                .replies
                .lock()
                .expect("gateway page reply lock must remain healthy")
                .pop_front()
                .expect("every gateway page request must have a scripted reply");
            self.request_started.notify_one();
            match reply {
                GatewayPageReply::Page(page) => Box::pin(async move { Ok(page) }),
                GatewayPageReply::Pending => {
                    Box::pin(async { pending::<Result<MessagePage, RiftError>>().await })
                }
            }
        }
    }

    /// Supply newest and forward pages to the gateway without a backward-history method.
    impl MessagePageSource for FakeMessagePageSource {
        /// Answer one newest-window request.
        fn latest<'a>(
            &'a self,
            room_id: &'a str,
            limit: u32,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            self.respond(GatewayPageRequest::Latest {
                room_id: room_id.to_owned(),
                limit,
            })
        }

        /// Answer one forward request after the supplied cursor.
        fn after<'a>(
            &'a self,
            room_id: &'a str,
            after_message_id: &'a str,
            limit: u32,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            self.respond(GatewayPageRequest::After {
                room_id: room_id.to_owned(),
                cursor: after_message_id.to_owned(),
                limit,
            })
        }
    }

    /// Assemble one deterministic actor runtime for the open test room.
    fn fake_runtime(
        session: Arc<FakeSession>,
        connector: Arc<FakeConnector>,
        sink: Arc<RecordingSink>,
        server_ids: Vec<String>,
    ) -> GatewayRuntime {
        let reconcile_source = Arc::new(FakeMessagePageSource::new((0..8).map(|_| {
            GatewayPageReply::Page(MessagePage {
                messages: Vec::new(),
                has_older: false,
            })
        })));
        fake_runtime_with_reconciliation(
            session,
            connector,
            sink,
            server_ids,
            reconcile_source,
            None,
        )
    }

    /// Assemble an actor runtime with an explicit reconciliation source and cursor.
    fn fake_runtime_with_reconciliation(
        session: Arc<FakeSession>,
        connector: Arc<FakeConnector>,
        sink: Arc<dyn GatewayEventSink>,
        server_ids: Vec<String>,
        reconcile_source: Arc<dyn MessagePageSource>,
        last_known_message: Option<RoomMessage>,
    ) -> GatewayRuntime {
        let last_known_message = last_known_message.map(|message| {
            NativeMessageCursor::from_message(&message)
                .expect("test gateway cursor message must remain valid")
        });
        GatewayRuntime {
            room_id: "room-1".into(),
            server_ids,
            session,
            connector,
            sink,
            jitter: Arc::new(FixedJitter { millis: 0 }),
            reconcile_source,
            last_known_message,
        }
    }

    /// Receive and assert one connection-status event.
    async fn expect_status(
        events: &mut mpsc::UnboundedReceiver<RoomConversationEvent>,
        expected: RoomConnectionStatus,
    ) {
        let event = events.recv().await.expect("gateway must emit a status");
        assert!(matches!(
            event,
            RoomConversationEvent::ConnectionChanged {
                room_id,
                status,
            } if room_id == "room-1" && status == expected
        ));
    }

    /// Receive the next non-status room event.
    async fn next_room_event(
        events: &mut mpsc::UnboundedReceiver<RoomConversationEvent>,
    ) -> RoomConversationEvent {
        loop {
            let event = events.recv().await.expect("gateway must emit a room event");
            if !matches!(event, RoomConversationEvent::ConnectionChanged { .. }) {
                return event;
            }
        }
    }

    /// Construct a complete valid MessageCreate gateway envelope.
    fn message_create(url: &str) -> Value {
        message_create_at(url, "message-1", "2026-08-03T12:00:00Z")
    }

    /// Construct a valid MessageCreate envelope with an explicit order key.
    fn message_create_at(url: &str, message_id: &str, created_at: &str) -> Value {
        json!({
            "type": "MessageCreate",
            "data": {
                "id": message_id,
                "channel_id": "room-1",
                "author_id": "agent-1",
                "author_username": "planner",
                "author_display_name": "Planner",
                "author_avatar_url": null,
                "content": "A bounded update",
                "attachments": [{
                    "id": "attachment-1",
                    "message_id": message_id,
                    "filename": "notes.txt",
                    "url": url,
                    "content_type": "text/plain",
                    "size_bytes": 12,
                    "created_at": "2026-08-03T12:00:00Z"
                }],
                "message_type": "agent",
                "created_at": created_at
            }
        })
    }

    /// Construct one sanitized message returned by the reconciliation source.
    fn reconciled_message(id: &str) -> RoomMessage {
        reconciled_message_at(id, "2026-08-03T12:01:00+00:00")
    }

    /// Construct one sanitized reconciliation message with an explicit order key.
    fn reconciled_message_at(id: &str, created_at: &str) -> RoomMessage {
        RoomMessage {
            id: id.into(),
            room_id: "room-1".into(),
            author_id: "user-2".into(),
            author_username: "collaborator".into(),
            author_display_name: Some("Collaborator".into()),
            author_avatar_url: None,
            content: format!("reconciled {id}"),
            edited_at: None,
            created_at: created_at.into(),
            message_type: "user".into(),
            attachments: Vec::new(),
        }
    }

    /// Subscription input is canonicalized before any token-bearing connection begins.
    #[test]
    fn subscriptions_are_deduplicated_validated_and_bounded() {
        assert_eq!(
            canonical_server_ids(vec![
                "server-2".to_owned(),
                "server-1".to_owned(),
                "server-2".to_owned(),
            ])
            .expect("valid server identifiers must canonicalize"),
            vec!["server-1".to_owned(), "server-2".to_owned()]
        );
        assert!(canonical_server_ids(vec![String::new()]).is_err());
        assert!(
            canonical_server_ids(
                (0..=MAX_SUBSCRIPTION_BATCH)
                    .map(|index| format!("server-{index}"))
                    .collect(),
            )
            .is_err()
        );
        assert!(canonical_server_ids(vec![" server-1".into()]).is_err());
        assert_eq!(ROOM_CONVERSATION_EVENT, "henosis://room-conversation");
    }

    /// Transport allocation and reconnect timing remain bounded.
    #[test]
    fn websocket_limits_and_backoff_are_capped() {
        let config = websocket_config();
        assert_eq!(config.max_message_size, Some(MAX_INBOUND_MESSAGE_BYTES));
        assert_eq!(config.max_frame_size, Some(MAX_INBOUND_MESSAGE_BYTES));
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(HANDSHAKE_TIMEOUT, Duration::from_secs(10));
        assert_eq!(WRITE_TIMEOUT, Duration::from_secs(10));
        assert_eq!(OUTBOUND_TYPING_CAPACITY, 1);

        let mut backoff = ReconnectBackoff::new();
        let no_jitter = FixedJitter { millis: 0 };
        let delays = (0..8)
            .map(|_| backoff.next_delay(&no_jitter))
            .collect::<Vec<_>>();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(15),
                Duration::from_secs(15),
            ]
        );

        backoff.reset();
        assert_eq!(
            backoff.next_delay(&FixedJitter { millis: u64::MAX }),
            Duration::from_millis(312)
        );
    }

    /// Outbound typing is coalesced before ACK and serialized only after reconciliation.
    #[tokio::test]
    async fn outbound_typing_is_ready_gated_bounded_and_exact() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime(session, connector, sink, Vec::new()))
            .expect("typing actor must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _url = attempts.recv().await.expect("typing connection attempt");
        assert_eq!(
            peer.next_json().await,
            json!({"type": "Identify", "data": {"token": "access-one"}})
        );
        gateway
            .enqueue_typing()
            .expect("first pre-ACK typing signal must enqueue");
        gateway
            .enqueue_typing()
            .expect("full mailbox must coalesce another typing signal");
        tokio::task::yield_now().await;
        assert!(peer.outbound.try_recv().is_err());

        complete_subscription_handshake(&mut peer, &[]).await;
        expect_status(&mut events, RoomConnectionStatus::Connected).await;
        tokio::task::yield_now().await;
        assert!(peer.outbound.try_recv().is_err());

        gateway
            .enqueue_typing()
            .expect("post-ACK typing signal must enqueue");
        assert_eq!(
            peer.next_json().await,
            json!({"type": "Typing", "data": {"channel_id": "room-1"}})
        );
        gateway.cancel();
        tokio::task::yield_now().await;
        drop(gateway);
    }

    /// A typing signal captured during reconnect backoff is never replayed on the next socket.
    #[tokio::test(start_paused = true)]
    async fn reconnect_discards_stale_outbound_typing() {
        let (first_socket, mut first_peer) = fake_socket_pair();
        let (second_socket, mut second_peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![
            ConnectScript::Socket(first_socket),
            ConnectScript::Socket(second_socket),
        ]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime(session, connector, sink, Vec::new()))
            .expect("reconnecting typing actor must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _first_url = attempts.recv().await.expect("first typing attempt");
        let _first_identify = first_peer.next_json().await;
        complete_subscription_handshake(&mut first_peer, &[]).await;
        expect_status(&mut events, RoomConnectionStatus::Connected).await;
        gateway
            .enqueue_typing()
            .expect("connected typing signal must enqueue");
        assert_eq!(
            first_peer.next_json().await,
            json!({"type": "Typing", "data": {"channel_id": "room-1"}})
        );

        first_peer.send(WebSocketMessage::Close(None));
        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;
        gateway
            .enqueue_typing()
            .expect("backoff typing signal must coalesce without transport details");
        tokio::time::advance(INITIAL_RECONNECT_DELAY).await;
        let _second_url = attempts.recv().await.expect("second typing attempt");
        assert_eq!(
            second_peer.next_json().await,
            json!({"type": "Identify", "data": {"token": "access-one"}})
        );
        complete_subscription_handshake(&mut second_peer, &[]).await;
        expect_status(&mut events, RoomConnectionStatus::Connected).await;
        tokio::task::yield_now().await;
        assert!(second_peer.outbound.try_recv().is_err());

        gateway
            .enqueue_typing()
            .expect("fresh typing signal must enqueue after reconnect");
        assert_eq!(
            second_peer.next_json().await,
            json!({"type": "Typing", "data": {"channel_id": "room-1"}})
        );
        gateway.cancel();
        tokio::task::yield_now().await;
        drop(gateway);
    }

    /// WebSocket URL derivation preserves only the authenticated service origin.
    #[test]
    fn websocket_url_uses_the_fixed_same_origin_endpoint() {
        let secure_client = AuthenticatedRiftClient::gateway_test_client(
            Url::parse("https://rift.example:9443/ignored?value=1#fragment")
                .expect("secure test endpoint must parse"),
            "user-1",
        );
        assert_eq!(
            secure_client
                .gateway_websocket_url()
                .expect("HTTPS must derive a secure gateway URL")
                .as_str(),
            "wss://rift.example:9443/ws"
        );

        let local_client = AuthenticatedRiftClient::gateway_test_client(
            Url::parse("http://127.0.0.1:8080/base").expect("local test endpoint must parse"),
            "user-1",
        );
        assert_eq!(
            local_client
                .gateway_websocket_url()
                .expect("HTTP must derive a plain gateway URL")
                .as_str(),
            "ws://127.0.0.1:8080/ws"
        );

        let invalid_client = AuthenticatedRiftClient::gateway_test_client(
            Url::parse("ftp://rift.example/").expect("invalid scheme fixture must parse"),
            "user-1",
        );
        assert!(invalid_client.gateway_websocket_url().is_err());
    }

    /// The exact ACK barrier, duplicate Ready handling, and five event mappings agree.
    #[tokio::test]
    async fn gateway_identifies_subscribes_and_translates_supported_events() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime(
            session,
            connector,
            sink,
            vec!["server-2".into(), "server-1".into()],
        ))
        .expect("test runtime must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        assert_eq!(
            attempts.recv().await.expect("connection attempt"),
            Url::parse("wss://rift.example/ws").expect("expected URL must parse")
        );
        assert_eq!(
            peer.next_json().await,
            json!({"type": "Identify", "data": {"token": "access-one"}})
        );

        peer.send(WebSocketMessage::Ping(vec![1, 2, 3].into()));
        assert!(matches!(
            peer.next_frame().await,
            WebSocketMessage::Pong(payload) if payload.as_ref() == [1, 2, 3]
        ));
        peer.send_json(json!({
            "type": "MessageDelete",
            "data": {"id": "pre-ready", "channel_id": "room-1"}
        }));
        peer.send_json(json!({
            "type": "Ready",
            "data": {"user_id": "user-1", "username": ""}
        }));
        tokio::task::yield_now().await;
        assert!(peer.outbound.try_recv().is_err());
        send_ready(&peer);
        assert_eq!(
            peer.next_json().await,
            json!({
                "type": "Subscribe",
                "data": {"server_ids": ["server-1", "server-2"]}
            })
        );
        tokio::task::yield_now().await;
        assert!(events.try_recv().is_err());

        send_ready(&peer);
        tokio::task::yield_now().await;
        assert!(peer.outbound.try_recv().is_err());
        assert!(events.try_recv().is_err());

        send_subscription_ack(&peer, &["server-1", "server-2"]);
        expect_status(&mut events, RoomConnectionStatus::Connected).await;

        peer.send_json(json!({"type": "FutureEvent", "data": {"secret": "ignored"}}));
        peer.send(WebSocketMessage::Text("{not-json".into()));
        peer.send(WebSocketMessage::Binary(vec![9, 9].into()));
        peer.send_json(json!({
            "type": "MessageUpdate",
            "data": {
                "id": "other-message",
                "channel_id": "room-2",
                "content": "wrong room",
                "edited_at": "2026-08-03T12:01:00Z"
            }
        }));
        peer.send_json(message_create("https://evil.example/file"));
        peer.send_json(message_create("/api/attachments/attachment-1"));
        peer.send_json(json!({
            "type": "MessageUpdate",
            "data": {
                "id": "message-1",
                "channel_id": "room-1",
                "content": "Edited",
                "edited_at": "2026-08-03T12:01:00Z"
            }
        }));
        peer.send_json(json!({
            "type": "MessageDelete",
            "data": {"id": "message-2", "channel_id": "room-1"}
        }));
        peer.send_json(json!({
            "type": "TypingStart",
            "data": {
                "channel_id": "room-1",
                "user_id": "user-2",
                "username": "collaborator"
            }
        }));
        peer.send_json(json!({
            "type": "PresenceUpdate",
            "data": {"user_id": "user-2", "status": "idle"}
        }));

        let created = next_room_event(&mut events).await;
        assert!(matches!(
            created,
            RoomConversationEvent::MessageCreate { room_id, message }
                if room_id == "room-1"
                    && message.id == "message-1"
                    && message.message_type == "agent"
                    && message.attachments.len() == 1
                    && message.attachments[0].url
                        == "https://rift.example/api/attachments/attachment-1"
        ));
        assert!(matches!(
            next_room_event(&mut events).await,
            RoomConversationEvent::MessageUpdate {
                room_id,
                message_id,
                content,
                edited_at,
            } if room_id == "room-1"
                && message_id == "message-1"
                && content == "Edited"
                && edited_at.starts_with("2026-08-03T12:01:00")
        ));
        assert!(matches!(
            next_room_event(&mut events).await,
            RoomConversationEvent::MessageDelete { room_id, message_id }
                if room_id == "room-1" && message_id == "message-2"
        ));
        assert!(matches!(
            next_room_event(&mut events).await,
            RoomConversationEvent::TypingStart {
                room_id,
                user_id,
                username,
            } if room_id == "room-1"
                && user_id == "user-2"
                && username == "collaborator"
        ));
        assert!(matches!(
            next_room_event(&mut events).await,
            RoomConversationEvent::PresenceUpdate {
                room_id,
                user_id,
                status,
            } if room_id == "room-1" && user_id == "user-2" && status == "idle"
        ));

        peer.send(WebSocketMessage::Ping(vec![7].into()));
        assert!(matches!(
            peer.next_frame().await,
            WebSocketMessage::Pong(payload) if payload.as_ref() == [7]
        ));
        gateway.shutdown().await;
    }

    /// An empty set still uses the ACK barrier and room-start reconciliation cursor.
    #[tokio::test]
    async fn empty_subscription_waits_for_ack_and_reconciles_from_room_start() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let source = Arc::new(FakeMessagePageSource::new([GatewayPageReply::Page(
            MessagePage {
                messages: Vec::new(),
                has_older: false,
            },
        )]));
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime_with_reconciliation(
            session,
            connector,
            sink,
            Vec::new(),
            source.clone(),
            None,
        ))
        .expect("test runtime must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _url = attempts.recv().await.expect("connection attempt");
        let _identify = peer.next_json().await;
        send_ready(&peer);
        assert_eq!(
            peer.next_json().await,
            json!({"type": "Subscribe", "data": {"server_ids": []}})
        );
        tokio::task::yield_now().await;
        assert!(source.requests().is_empty());
        assert!(events.try_recv().is_err());

        send_subscription_ack(&peer, &[]);
        source.wait_for_request().await;
        expect_status(&mut events, RoomConnectionStatus::Connected).await;
        assert_eq!(
            source.requests(),
            vec![GatewayPageRequest::After {
                room_id: "room-1".into(),
                cursor: "room-1".into(),
                limit: crate::reconcile::RECONCILE_PAGE_SIZE,
            }]
        );
        peer.send(WebSocketMessage::Ping(vec![5].into()));
        assert!(matches!(
            peer.next_frame().await,
            WebSocketMessage::Pong(payload) if payload.as_ref() == [5]
        ));
        gateway.shutdown().await;
    }

    /// A queued live duplicate after ACK leaves one identifier in native room state.
    #[tokio::test]
    async fn queued_live_duplicate_and_reconciliation_leave_one_native_message() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let source = Arc::new(FakeMessagePageSource::new([GatewayPageReply::Page(
            MessagePage {
                messages: vec![reconciled_message_at(
                    "message-1",
                    "2026-08-03T12:00:00+00:00",
                )],
                has_older: false,
            },
        )]));
        let owner = AuthenticatedRiftClient::gateway_test_client(
            Url::parse("https://rift.example/").expect("state-backed endpoint must parse"),
            "user-1",
        );
        let state = Arc::new(AppState::new());
        state
            .set_session(owner.clone())
            .expect("state-backed session must install");
        let lease = state
            .begin_room_open(&owner, "room-1", "stream-state-backed-0001")
            .expect("state-backed room must begin");
        state
            .install_room_page(
                &lease,
                MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                },
            )
            .expect("state-backed opening page must install");
        let (sink, mut events) = state_backed_recording_sink(Arc::clone(&state), lease.clone());
        let session = Arc::new(FakeSession::new());
        let gateway = spawn_gateway_actor(fake_runtime_with_reconciliation(
            session,
            connector,
            sink,
            vec!["server-1".into()],
            source.clone(),
            None,
        ))
        .expect("state-backed actor must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _url = attempts
            .recv()
            .await
            .expect("state-backed connection attempt");
        let _identify = peer.next_json().await;
        send_ready(&peer);
        assert_eq!(
            peer.next_json().await,
            json!({"type": "Subscribe", "data": {"server_ids": ["server-1"]}})
        );
        send_subscription_ack(&peer, &["server-1"]);
        peer.send_json(message_create_at(
            "/api/attachments/attachment-1",
            "message-1",
            "2026-08-03T12:00:00Z",
        ));

        source.wait_for_request().await;
        assert!(matches!(
            events.recv().await.expect("state-backed reconciliation event"),
            RoomConversationEvent::Reconciliation { page, .. }
                if page.messages.len() == 1 && page.messages[0].id == "message-1"
        ));
        expect_status(&mut events, RoomConnectionStatus::Connected).await;
        assert!(matches!(
            events.recv().await.expect("queued live duplicate event"),
            RoomConversationEvent::MessageCreate { message, .. }
                if message.id == "message-1"
        ));
        assert!(matches!(
            source.requests().as_slice(),
            [GatewayPageRequest::After { cursor, .. }] if cursor == "room-1"
        ));
        let messages = state
            .active_room_messages(&lease)
            .expect("state-backed messages must remain readable");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "message-1");
        gateway.shutdown().await;
    }

    /// A partial ACK reconnects without exposing Connected or starting reconciliation.
    #[tokio::test(start_paused = true)]
    async fn partial_subscription_ack_reconnects_without_reconciliation() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let source = Arc::new(FakeMessagePageSource::new([GatewayPageReply::Page(
            MessagePage {
                messages: Vec::new(),
                has_older: false,
            },
        )]));
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime_with_reconciliation(
            session.clone(),
            connector,
            sink,
            vec!["server-2".into(), "server-1".into()],
            source.clone(),
            None,
        ))
        .expect("partial-ACK actor must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _url = attempts
            .recv()
            .await
            .expect("partial-ACK connection attempt");
        let _identify = peer.next_json().await;
        send_ready(&peer);
        assert_eq!(
            peer.next_json().await,
            json!({
                "type": "Subscribe",
                "data": {"server_ids": ["server-1", "server-2"]}
            })
        );
        send_subscription_ack(&peer, &["server-1"]);

        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;
        assert!(source.requests().is_empty());
        assert_eq!(session.refresh_count(), 0);
        gateway.shutdown().await;
    }

    /// The subscription ACK deadline remains active after valid Ready.
    #[tokio::test(start_paused = true)]
    async fn missing_subscription_ack_is_bounded_without_refreshing_authentication() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime(
            session.clone(),
            connector,
            sink,
            vec!["server-1".into()],
        ))
        .expect("missing-ACK actor must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _url = attempts
            .recv()
            .await
            .expect("missing-ACK connection attempt");
        let _identify = peer.next_json().await;
        send_ready(&peer);
        assert_eq!(
            peer.next_json().await,
            json!({"type": "Subscribe", "data": {"server_ids": ["server-1"]}})
        );

        tokio::time::advance(HANDSHAKE_TIMEOUT - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(events.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;
        assert_eq!(session.refresh_count(), 0);
        gateway.shutdown().await;
    }

    /// A clean close after Ready but before ACK preserves the accepted token generation.
    #[tokio::test(start_paused = true)]
    async fn close_after_ready_before_ack_does_not_refresh_authentication() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime(
            session.clone(),
            connector,
            sink,
            vec!["server-1".into()],
        ))
        .expect("pre-ACK close actor must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _url = attempts
            .recv()
            .await
            .expect("pre-ACK close connection attempt");
        let _identify = peer.next_json().await;
        send_ready(&peer);
        let _subscribe = peer.next_json().await;
        peer.send(WebSocketMessage::Close(None));

        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;
        assert_eq!(session.refresh_count(), 0);
        gateway.shutdown().await;
    }

    /// A clean pre-Ready rejection refreshes before a connected retry resets backoff.
    #[tokio::test(start_paused = true)]
    async fn rejected_identify_refreshes_and_connected_resets_backoff() {
        let (first_socket, mut first_peer) = fake_socket_pair();
        let (second_socket, mut second_peer) = fake_socket_pair();
        let (third_socket, mut third_peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![
            ConnectScript::Socket(first_socket),
            ConnectScript::Socket(second_socket),
            ConnectScript::Socket(third_socket),
        ]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway =
            spawn_gateway_actor(fake_runtime(session.clone(), connector, sink, Vec::new()))
                .expect("test runtime must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _first_url = attempts.recv().await.expect("first attempt");
        assert_eq!(
            first_peer.next_json().await,
            json!({"type": "Identify", "data": {"token": "access-one"}})
        );
        first_peer.send(WebSocketMessage::Close(None));
        session.wait_for_refresh().await;
        assert_eq!(session.refresh_count(), 1);
        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;

        tokio::time::advance(Duration::from_millis(249)).await;
        tokio::task::yield_now().await;
        assert!(attempts.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        let _second_url = attempts.recv().await.expect("second attempt");
        assert_eq!(
            second_peer.next_json().await,
            json!({"type": "Identify", "data": {"token": "access-two"}})
        );
        complete_subscription_handshake(&mut second_peer, &[]).await;
        expect_status(&mut events, RoomConnectionStatus::Connected).await;
        second_peer.send(WebSocketMessage::Close(None));
        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;

        tokio::time::advance(Duration::from_millis(249)).await;
        tokio::task::yield_now().await;
        assert!(attempts.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        let _third_url = attempts.recv().await.expect("third attempt");
        assert_eq!(
            third_peer.next_json().await,
            json!({"type": "Identify", "data": {"token": "access-two"}})
        );
        gateway.shutdown().await;
    }

    /// Every acknowledged connection catches up from the newest native cursor.
    #[tokio::test(start_paused = true)]
    async fn reconnect_reconciles_after_the_latest_native_cursor() {
        let (first_socket, mut first_peer) = fake_socket_pair();
        let (second_socket, mut second_peer) = fake_socket_pair();
        let (third_socket, mut third_peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![
            ConnectScript::Socket(first_socket),
            ConnectScript::Socket(second_socket),
            ConnectScript::Socket(third_socket),
        ]);
        let source = Arc::new(FakeMessagePageSource::new([
            GatewayPageReply::Page(MessagePage {
                messages: Vec::new(),
                has_older: false,
            }),
            GatewayPageReply::Page(MessagePage {
                messages: vec![reconciled_message("message-2")],
                has_older: false,
            }),
            GatewayPageReply::Page(MessagePage {
                messages: Vec::new(),
                has_older: false,
            }),
        ]));
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime_with_reconciliation(
            session,
            connector,
            sink,
            Vec::new(),
            source.clone(),
            Some(reconciled_message_at(
                "snapshot-message",
                "2026-08-03T11:59:00+00:00",
            )),
        ))
        .expect("reconciling actor must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _first_url = attempts.recv().await.expect("first connection attempt");
        let _first_identify = first_peer.next_json().await;
        complete_subscription_handshake(&mut first_peer, &[]).await;
        source.wait_for_request().await;
        expect_status(&mut events, RoomConnectionStatus::Connected).await;

        first_peer.send_json(message_create("/api/attachments/attachment-1"));
        assert!(matches!(
            next_room_event(&mut events).await,
            RoomConversationEvent::MessageCreate { message, .. } if message.id == "message-1"
        ));
        first_peer.send(WebSocketMessage::Close(None));
        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;
        tokio::time::advance(INITIAL_RECONNECT_DELAY).await;

        let _second_url = attempts.recv().await.expect("second connection attempt");
        let _second_identify = second_peer.next_json().await;
        complete_subscription_handshake(&mut second_peer, &[]).await;
        source.wait_for_request().await;
        assert!(matches!(
            events.recv().await.expect("reconciliation event"),
            RoomConversationEvent::Reconciliation {
                room_id,
                page,
                replace_live_window: false,
            } if room_id == "room-1"
                && page.messages.len() == 1
                && page.messages[0].id == "message-2"
        ));
        expect_status(&mut events, RoomConnectionStatus::Connected).await;

        second_peer.send(WebSocketMessage::Close(None));
        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;
        tokio::time::advance(INITIAL_RECONNECT_DELAY).await;
        let _third_url = attempts.recv().await.expect("third connection attempt");
        let _third_identify = third_peer.next_json().await;
        complete_subscription_handshake(&mut third_peer, &[]).await;
        source.wait_for_request().await;
        expect_status(&mut events, RoomConnectionStatus::Connected).await;

        assert_eq!(
            source.requests(),
            vec![
                GatewayPageRequest::After {
                    room_id: "room-1".into(),
                    cursor: "snapshot-message".into(),
                    limit: crate::reconcile::RECONCILE_PAGE_SIZE,
                },
                GatewayPageRequest::After {
                    room_id: "room-1".into(),
                    cursor: "message-1".into(),
                    limit: crate::reconcile::RECONCILE_PAGE_SIZE,
                },
                GatewayPageRequest::After {
                    room_id: "room-1".into(),
                    cursor: "message-2".into(),
                    limit: crate::reconcile::RECONCILE_PAGE_SIZE,
                },
            ]
        );
        gateway.shutdown().await;
    }

    /// Delayed older live events never move the reconnect cursor behind a newer message.
    #[tokio::test(start_paused = true)]
    async fn out_of_order_live_create_does_not_regress_reconnect_cursor() {
        let (first_socket, mut first_peer) = fake_socket_pair();
        let (second_socket, mut second_peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![
            ConnectScript::Socket(first_socket),
            ConnectScript::Socket(second_socket),
        ]);
        let source = Arc::new(FakeMessagePageSource::new([
            GatewayPageReply::Page(MessagePage {
                messages: Vec::new(),
                has_older: false,
            }),
            GatewayPageReply::Page(MessagePage {
                messages: Vec::new(),
                has_older: false,
            }),
        ]));
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime_with_reconciliation(
            session,
            connector,
            sink,
            Vec::new(),
            source.clone(),
            Some(reconciled_message_at(
                "snapshot-message",
                "2026-08-03T11:59:00+00:00",
            )),
        ))
        .expect("ordered-cursor actor must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _first_url = attempts.recv().await.expect("first connection attempt");
        let _first_identify = first_peer.next_json().await;
        complete_subscription_handshake(&mut first_peer, &[]).await;
        source.wait_for_request().await;
        expect_status(&mut events, RoomConnectionStatus::Connected).await;

        first_peer.send_json(message_create_at(
            "/api/attachments/attachment-1",
            "newer-message",
            "2026-08-03T12:02:00Z",
        ));
        assert!(matches!(
            next_room_event(&mut events).await,
            RoomConversationEvent::MessageCreate { message, .. } if message.id == "newer-message"
        ));
        first_peer.send_json(message_create_at(
            "/api/attachments/attachment-1",
            "delayed-older-message",
            "2026-08-03T12:00:00Z",
        ));
        assert!(matches!(
            next_room_event(&mut events).await,
            RoomConversationEvent::MessageCreate { message, .. }
                if message.id == "delayed-older-message"
        ));
        first_peer.send(WebSocketMessage::Close(None));
        expect_status(&mut events, RoomConnectionStatus::Reconnecting).await;
        tokio::time::advance(INITIAL_RECONNECT_DELAY).await;

        let _second_url = attempts.recv().await.expect("second connection attempt");
        let _second_identify = second_peer.next_json().await;
        complete_subscription_handshake(&mut second_peer, &[]).await;
        source.wait_for_request().await;
        assert!(matches!(
            source.requests().as_slice(),
            [
                GatewayPageRequest::After { cursor: first, .. },
                GatewayPageRequest::After { cursor: second, .. },
            ] if first == "snapshot-message" && second == "newer-message"
        ));
        expect_status(&mut events, RoomConnectionStatus::Connected).await;
        gateway.shutdown().await;
    }

    /// Replacing a room cancels its first post-ACK reconciliation before emission.
    #[tokio::test(start_paused = true)]
    async fn room_replacement_cancels_pending_reconciliation() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let source = Arc::new(FakeMessagePageSource::new([GatewayPageReply::Pending]));
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime_with_reconciliation(
            session,
            connector,
            sink,
            Vec::new(),
            source.clone(),
            Some(reconciled_message("message-1")),
        ))
        .expect("pending reconciliation actor must spawn");
        let old_cancellation = gateway.cancellation.clone();

        let owner = AuthenticatedRiftClient::gateway_test_client(
            Url::parse("https://rift.example/").expect("owner endpoint must parse"),
            "user-1",
        );
        let state = crate::state::AppState::new();
        state
            .set_session(owner.clone())
            .expect("owner session must install");
        let owner_lease = state
            .begin_room_open(&owner, "room-1", "stream-owner-0001")
            .expect("owner room must begin");
        state
            .install_room_page(
                &owner_lease,
                MessagePage {
                    messages: vec![reconciled_message("message-1")],
                    has_older: false,
                },
            )
            .expect("owner room page must install");
        state
            .install_room_gateway(&owner_lease, gateway)
            .expect("pending gateway must install");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _url = attempts.recv().await.expect("first connection attempt");
        let _identify = peer.next_json().await;
        complete_subscription_handshake(&mut peer, &[]).await;
        source.wait_for_request().await;

        let (replacement, replacement_cancellation) = test_rift_gateway();
        let replacement_lease = state
            .begin_room_open(&owner, "room-1", "stream-owner-0002")
            .expect("same-room replacement must begin");
        state
            .install_room_page(
                &replacement_lease,
                MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                },
            )
            .expect("replacement room page must install");
        state
            .install_room_gateway(&replacement_lease, replacement)
            .expect("replacement gateway must install");
        assert!(old_cancellation.is_cancelled());
        tokio::task::yield_now().await;
        assert!(events.try_recv().is_err());
        assert!(matches!(
            source.requests().as_slice(),
            [GatewayPageRequest::After { cursor, .. }] if cursor == "message-1"
        ));

        state
            .clear_gateway()
            .expect("replacement gateway must clear");
        assert!(replacement_cancellation.is_cancelled());
    }

    /// Cancellation interrupts connector, socket-read, and backoff waits immediately.
    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_every_gateway_wait() {
        let (pending_connector, mut pending_attempts) =
            fake_connector(vec![ConnectScript::Pending]);
        let pending_session = Arc::new(FakeSession::new());
        let (pending_sink, mut pending_events) = recording_sink();
        let pending_gateway = spawn_gateway_actor(fake_runtime(
            pending_session,
            pending_connector,
            pending_sink,
            Vec::new(),
        ))
        .expect("pending actor must spawn");
        expect_status(&mut pending_events, RoomConnectionStatus::Connecting).await;
        let _pending_url = pending_attempts.recv().await.expect("pending attempt");
        tokio::time::timeout(Duration::from_millis(1), pending_gateway.shutdown())
            .await
            .expect("pending connect must cancel");

        let (read_socket, mut read_peer) = fake_socket_pair();
        let (read_connector, mut read_attempts) =
            fake_connector(vec![ConnectScript::Socket(read_socket)]);
        let read_session = Arc::new(FakeSession::new());
        let (read_sink, mut read_events) = recording_sink();
        let read_gateway = spawn_gateway_actor(fake_runtime(
            read_session,
            read_connector,
            read_sink,
            Vec::new(),
        ))
        .expect("read actor must spawn");
        expect_status(&mut read_events, RoomConnectionStatus::Connecting).await;
        let _read_url = read_attempts.recv().await.expect("read attempt");
        let _identify = read_peer.next_json().await;
        tokio::time::timeout(Duration::from_millis(1), read_gateway.shutdown())
            .await
            .expect("pending read must cancel");

        let (backoff_connector, mut backoff_attempts) = fake_connector(vec![ConnectScript::Fail]);
        let backoff_session = Arc::new(FakeSession::new());
        let (backoff_sink, mut backoff_events) = recording_sink();
        let backoff_gateway = spawn_gateway_actor(fake_runtime(
            backoff_session,
            backoff_connector,
            backoff_sink,
            Vec::new(),
        ))
        .expect("backoff actor must spawn");
        expect_status(&mut backoff_events, RoomConnectionStatus::Connecting).await;
        let _backoff_url = backoff_attempts.recv().await.expect("failed attempt");
        expect_status(&mut backoff_events, RoomConnectionStatus::Reconnecting).await;
        tokio::time::timeout(Duration::from_millis(1), backoff_gateway.shutdown())
            .await
            .expect("pending backoff must cancel");
    }

    /// Stalled transport setup and authentication handshakes enter bounded reconnects.
    #[tokio::test(start_paused = true)]
    async fn stalled_connect_and_ready_handshake_are_bounded() {
        let (pending_connector, mut pending_attempts) =
            fake_connector(vec![ConnectScript::Pending]);
        let pending_session = Arc::new(FakeSession::new());
        let (pending_sink, mut pending_events) = recording_sink();
        let pending_gateway = spawn_gateway_actor(fake_runtime(
            pending_session,
            pending_connector,
            pending_sink,
            Vec::new(),
        ))
        .expect("pending actor must spawn");
        expect_status(&mut pending_events, RoomConnectionStatus::Connecting).await;
        let _pending_url = pending_attempts.recv().await.expect("pending attempt");

        tokio::time::advance(Duration::from_millis(9_999)).await;
        tokio::task::yield_now().await;
        assert!(pending_events.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::time::timeout(
            Duration::from_millis(1),
            expect_status(&mut pending_events, RoomConnectionStatus::Reconnecting),
        )
        .await
        .expect("stalled connect must enter reconnect backoff");
        pending_gateway.shutdown().await;

        let (ready_socket, mut ready_peer) = fake_socket_pair();
        let (ready_connector, mut ready_attempts) =
            fake_connector(vec![ConnectScript::Socket(ready_socket)]);
        let ready_session = Arc::new(FakeSession::new());
        let (ready_sink, mut ready_events) = recording_sink();
        let ready_gateway = spawn_gateway_actor(fake_runtime(
            ready_session,
            ready_connector,
            ready_sink,
            Vec::new(),
        ))
        .expect("ready actor must spawn");
        expect_status(&mut ready_events, RoomConnectionStatus::Connecting).await;
        let _ready_url = ready_attempts.recv().await.expect("ready attempt");
        let _identify = ready_peer.next_json().await;

        tokio::time::advance(Duration::from_millis(9_999)).await;
        tokio::task::yield_now().await;
        assert!(ready_events.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::time::timeout(
            Duration::from_millis(1),
            expect_status(&mut ready_events, RoomConnectionStatus::Reconnecting),
        )
        .await
        .expect("stalled Ready handshake must enter reconnect backoff");
        ready_gateway.shutdown().await;

        let (mut send_socket, _send_peer) = fake_socket_pair();
        send_socket.pending_send = true;
        let (send_connector, mut send_attempts) =
            fake_connector(vec![ConnectScript::Socket(send_socket)]);
        let send_session = Arc::new(FakeSession::new());
        let (send_sink, mut send_events) = recording_sink();
        let send_gateway = spawn_gateway_actor(fake_runtime(
            send_session,
            send_connector,
            send_sink,
            Vec::new(),
        ))
        .expect("send actor must spawn");
        expect_status(&mut send_events, RoomConnectionStatus::Connecting).await;
        let _send_url = send_attempts.recv().await.expect("send attempt");

        tokio::time::advance(Duration::from_millis(9_999)).await;
        tokio::task::yield_now().await;
        assert!(send_events.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::time::timeout(
            Duration::from_millis(1),
            expect_status(&mut send_events, RoomConnectionStatus::Reconnecting),
        )
        .await
        .expect("stalled write must enter reconnect backoff");
        send_gateway.shutdown().await;
    }

    /// Ready for another user is terminal and never subscribes or refreshes.
    #[tokio::test]
    async fn ready_identity_mismatch_stops_without_subscription() {
        let (socket, mut peer) = fake_socket_pair();
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Socket(socket)]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let gateway = spawn_gateway_actor(fake_runtime(
            session.clone(),
            connector,
            sink,
            vec!["server-1".into()],
        ))
        .expect("test runtime must spawn");

        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        let _url = attempts.recv().await.expect("connection attempt");
        let _identify = peer.next_json().await;
        peer.send_json(json!({
            "type": "Ready",
            "data": {"user_id": "other-user", "username": "mallory"}
        }));
        expect_status(&mut events, RoomConnectionStatus::Disconnected).await;
        assert!(peer.outbound.recv().await.is_none());
        assert_eq!(session.refresh_count(), 0);
        gateway.shutdown().await;
    }

    /// A delivery failure cancels the actor before it can create an unobservable sequence gap.
    #[tokio::test]
    async fn event_sink_failure_cancels_gateway_before_connecting() {
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Pending]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        sink.fail_next.store(true, Ordering::Release);
        let gateway = spawn_gateway_actor(fake_runtime(session, connector, sink, Vec::new()))
            .expect("test runtime must spawn");

        tokio::time::timeout(Duration::from_secs(1), gateway.cancellation.cancelled())
            .await
            .expect("failed delivery must cancel the actor");
        assert!(attempts.try_recv().is_err());
        assert!(events.try_recv().is_err());
        gateway.shutdown().await;
    }

    /// A deferred actor neither emits nor connects until its snapshot barrier is released.
    #[tokio::test]
    async fn deferred_actor_waits_for_explicit_start() {
        let (connector, mut attempts) = fake_connector(vec![ConnectScript::Pending]);
        let session = Arc::new(FakeSession::new());
        let (sink, mut events) = recording_sink();
        let mut gateway = spawn_gateway_actor_with_cancellation(
            fake_runtime(session, connector, sink, Vec::new()),
            CancellationToken::new(),
            true,
        )
        .expect("deferred test runtime must spawn");

        tokio::task::yield_now().await;
        assert!(attempts.try_recv().is_err());
        assert!(events.try_recv().is_err());

        gateway.start().expect("snapshot barrier must release");
        expect_status(&mut events, RoomConnectionStatus::Connecting).await;
        attempts
            .recv()
            .await
            .expect("released actor must begin connecting");
        gateway.shutdown().await;
    }

    /// AppState cancels replacements and rejects gateways from stale sessions.
    #[tokio::test]
    async fn app_state_owns_exactly_one_current_gateway() {
        let endpoint = Url::parse("https://rift.example/").expect("test endpoint must parse");
        let first = AuthenticatedRiftClient::gateway_test_client(endpoint.clone(), "user-1");
        let second = AuthenticatedRiftClient::gateway_test_client(endpoint, "user-2");
        let state = crate::state::AppState::new();
        state
            .set_session(first.clone())
            .expect("first session must install");

        let first_lease = state
            .begin_room_open(&first, "room-1", "stream-first-0001")
            .expect("first room must begin");
        state
            .install_room_page(
                &first_lease,
                MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                },
            )
            .expect("first page must install");
        let (first_gateway, first_cancelled) = test_rift_gateway();
        state
            .install_room_gateway(&first_lease, first_gateway)
            .expect("first gateway must install");
        let replacement_lease = state
            .begin_room_open(&first, "room-1", "stream-first-0002")
            .expect("replacement room must begin");
        assert!(first_lease.cancellation().is_cancelled());
        state
            .install_room_page(
                &replacement_lease,
                MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                },
            )
            .expect("replacement page must install");
        let (replacement_gateway, replacement_cancelled) = test_rift_gateway();
        state
            .install_room_gateway(&replacement_lease, replacement_gateway)
            .expect("replacement gateway must install");
        assert!(first_cancelled.is_cancelled());

        state
            .set_session(second.clone())
            .expect("second session must replace first");
        assert!(replacement_cancelled.is_cancelled());
        assert!(!first.gateway_is_active());
        assert!(second.gateway_is_active());

        let (stale_gateway, stale_cancelled) = test_rift_gateway();
        assert!(
            state
                .install_room_gateway(&replacement_lease, stale_gateway)
                .is_err()
        );
        assert!(stale_cancelled.is_cancelled());

        let current_lease = state
            .begin_room_open(&second, "room-2", "stream-second-0001")
            .expect("current room must begin");
        state
            .install_room_page(
                &current_lease,
                MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                },
            )
            .expect("current page must install");
        let (current_gateway, current_cancelled) = test_rift_gateway();
        state
            .install_room_gateway(&current_lease, current_gateway)
            .expect("current gateway must install");
        state.clear_gateway().expect("current gateway must clear");
        assert!(current_cancelled.is_cancelled());
        assert!(
            state
                .session()
                .expect("session state must remain readable")
                .is_some_and(|current| current.same_session(&second))
        );

        let logout_lease = state
            .begin_room_open(&second, "room-2", "stream-second-0002")
            .expect("logout room must begin");
        state
            .install_room_page(
                &logout_lease,
                MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                },
            )
            .expect("logout page must install");
        let (logout_gateway, logout_cancelled) = test_rift_gateway();
        state
            .install_room_gateway(&logout_lease, logout_gateway)
            .expect("logout gateway must install");
        state.clear_session().expect("session must clear");
        assert!(logout_cancelled.is_cancelled());
        assert!(!second.gateway_is_active());
    }
}
