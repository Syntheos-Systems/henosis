//! Authenticated WebSocket sessions and scoped Rift event fan-out.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Maximum time a newly upgraded socket may remain unauthenticated.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of servers accepted in one subscription command.
const MAX_SUBSCRIPTION_BATCH: usize = 100;

/// Events sent from server -> client
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum GatewayEvent {
    /// Authentication succeeded for the identified socket.
    Ready {
        /// Authenticated Rift user identifier.
        user_id: Uuid,
        /// Authenticated Rift username.
        username: String,
    },
    /// Requested server and channel receivers are installed for this socket.
    Subscribed {
        /// Canonical server identifiers whose receivers are now active.
        server_ids: Vec<Uuid>,
    },
    MessageCreate {
        id: Uuid,
        channel_id: Uuid,
        author_id: Uuid,
        author_username: String,
        author_display_name: Option<String>,
        author_avatar_url: Option<String>,
        content: String,
        attachments: Vec<crate::models::attachment::Attachment>,
        // The type discriminator (user/agent/stimulus/system) rides the wire:
        // the agent bridge deserializes this event into a struct that carries
        // it, and omitting it made every bridge-side parse fail silently
        // (found in the 2026-07-17 live smoke test).
        message_type: String,
        created_at: String,
    },
    MessageUpdate {
        id: Uuid,
        channel_id: Uuid,
        content: String,
        edited_at: String,
    },
    MessageDelete {
        id: Uuid,
        channel_id: Uuid,
    },
    TypingStart {
        channel_id: Uuid,
        user_id: Uuid,
        username: String,
    },
    PresenceUpdate {
        user_id: Uuid,
        status: String,
    },
    MemberJoin {
        server_id: Uuid,
        user_id: Uuid,
        username: String,
    },
    MemberLeave {
        server_id: Uuid,
        user_id: Uuid,
    },
    ChannelCreate {
        id: Uuid,
        server_id: Uuid,
        name: String,
        channel_type: String,
    },
    ChannelDelete {
        id: Uuid,
        server_id: Uuid,
    },
    RoleCreate {
        server_id: Uuid,
        role: crate::models::role::Role,
    },
    RoleUpdate {
        server_id: Uuid,
        role: crate::models::role::Role,
    },
    RoleDelete {
        server_id: Uuid,
        role_id: Uuid,
    },
}

/// Commands sent from client -> server
#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum GatewayCommand {
    Identify { token: String },
    Typing { channel_id: Uuid },
    UpdatePresence { status: String },
    Subscribe { server_ids: Vec<Uuid> },
}

/// Connected user session
struct Session {
    user_id: Uuid,
    username: String,
    /// Server IDs this user is subscribed to
    subscribed_servers: HashSet<Uuid>,
}

/// Resolves the authorization and channel inventory required by a subscription.
#[async_trait]
trait SubscriptionDirectory: Sync {
    /// Return whether the user may subscribe to the requested server.
    async fn is_member(&self, server_id: Uuid, user_id: Uuid) -> Result<bool, sqlx::Error>;

    /// Return every channel whose receiver must be installed for the server.
    async fn channel_ids(&self, server_id: Uuid) -> Result<Vec<Uuid>, sqlx::Error>;
}

/// Reads subscription authorization and channel inventory from PostgreSQL.
struct PostgresSubscriptionDirectory<'a> {
    /// Shared Rift database pool.
    pool: &'a PgPool,
}

/// Implements subscription lookup through the production database functions.
#[async_trait]
impl SubscriptionDirectory for PostgresSubscriptionDirectory<'_> {
    /// Check the authenticated user's current server membership.
    async fn is_member(&self, server_id: Uuid, user_id: Uuid) -> Result<bool, sqlx::Error> {
        crate::db::is_member(self.pool, server_id, user_id).await
    }

    /// Load the server's complete channel identifier set.
    async fn channel_ids(&self, server_id: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
        crate::db::get_server_channels(self.pool, server_id)
            .await
            .map(|channels| channels.into_iter().map(|channel| channel.id).collect())
    }
}

/// Forward broadcast events until the source ends or the destination connection closes.
fn spawn_event_forwarder(
    mut source: broadcast::Receiver<GatewayEvent>,
    destination: tokio::sync::mpsc::Sender<GatewayEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = destination.closed() => break,
                event = source.recv() => {
                    let Ok(event) = event else {
                        break;
                    };
                    if destination.send(event).await.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// Central hub for all WebSocket connections
#[derive(Clone)]
pub struct Gateway {
    /// Per-channel broadcast senders. Any event for a channel goes here.
    channel_senders: Arc<DashMap<Uuid, broadcast::Sender<GatewayEvent>>>,
    /// Per-server broadcast senders for server-wide events (member join/leave, channel create/delete)
    server_senders: Arc<DashMap<Uuid, broadcast::Sender<GatewayEvent>>>,
    /// Per-user sender for DMs and presence
    user_senders: Arc<DashMap<Uuid, broadcast::Sender<GatewayEvent>>>,
    /// Online user tracking
    online_users: Arc<DashMap<Uuid, String>>,
    /// Number of active websocket connections per user
    connection_counts: Arc<DashMap<Uuid, usize>>,
}

/// Builds an empty gateway when a default value is requested.
impl Default for Gateway {
    /// Construct the default in-memory gateway state.
    fn default() -> Self {
        Self::new()
    }
}

/// Manages Rift socket lifecycle, subscriptions, and event delivery.
impl Gateway {
    /// Construct a gateway with empty sender and presence registries.
    pub fn new() -> Self {
        Self {
            channel_senders: Arc::new(DashMap::new()),
            server_senders: Arc::new(DashMap::new()),
            user_senders: Arc::new(DashMap::new()),
            online_users: Arc::new(DashMap::new()),
            connection_counts: Arc::new(DashMap::new()),
        }
    }

    /// Broadcast an event to all subscribers of a channel
    pub fn broadcast_to_channel(&self, channel_id: Uuid, event: GatewayEvent) {
        if let Some(sender) = self.channel_senders.get(&channel_id) {
            let _ = sender.send(event);
        }
    }

    /// Broadcast an event to all members of a server
    pub fn broadcast_to_server(&self, server_id: Uuid, event: GatewayEvent) {
        if let Some(sender) = self.server_senders.get(&server_id) {
            let _ = sender.send(event);
        }
    }

    /// Send an event to a specific user
    pub fn send_to_user(&self, user_id: Uuid, event: GatewayEvent) {
        if let Some(sender) = self.user_senders.get(&user_id) {
            let _ = sender.send(event);
        }
    }

    /// Subscribe to events published for one channel.
    fn subscribe_channel(&self, channel_id: Uuid) -> broadcast::Receiver<GatewayEvent> {
        self.channel_senders
            .entry(channel_id)
            .or_insert_with(|| broadcast::channel(256).0)
            .subscribe()
    }

    /// Subscribe to events published for one server.
    fn subscribe_server(&self, server_id: Uuid) -> broadcast::Receiver<GatewayEvent> {
        self.server_senders
            .entry(server_id)
            .or_insert_with(|| broadcast::channel(64).0)
            .subscribe()
    }

    /// Subscribe to events addressed directly to one user.
    fn subscribe_user(&self, user_id: Uuid) -> broadcast::Receiver<GatewayEvent> {
        self.user_senders
            .entry(user_id)
            .or_insert_with(|| broadcast::channel(64).0)
            .subscribe()
    }

    /// Record an authenticated connection and expose the user as online.
    fn mark_connection_open(&self, user_id: Uuid, username: &str) {
        self.online_users.insert(user_id, username.to_string());
        self.connection_counts
            .entry(user_id)
            .and_modify(|count| *count += 1)
            .or_insert(1);
    }

    /// Remove one connection and clear online state after the final socket closes.
    fn mark_connection_closed(&self, user_id: Uuid) {
        let remove_online = if let Some(mut count) = self.connection_counts.get_mut(&user_id) {
            if *count > 1 {
                *count -= 1;
                false
            } else {
                true
            }
        } else {
            true
        };

        if remove_online {
            self.connection_counts.remove(&user_id);
            self.online_users.remove(&user_id);
            if let Some(sender) = self.user_senders.get(&user_id)
                && sender.receiver_count() == 0
            {
                drop(sender);
                self.user_senders.remove(&user_id);
            }
        }
    }

    /// Handle a new WebSocket connection
    pub async fn handle_connection(&self, socket: WebSocket, jwt_secret: String, pool: PgPool) {
        let (mut ws_tx, mut ws_rx) = socket.split();
        let gateway = self.clone();

        // Bound the unauthenticated phase and require Identify as the first text command.
        let identify = tokio::time::timeout(IDENTIFY_TIMEOUT, async {
            loop {
                match ws_rx.next().await {
                    Some(Ok(WsMessage::Text(text))) => break parse_identify(&text, &jwt_secret),
                    Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => continue,
                    _ => break None,
                }
            }
        })
        .await;
        let mut session = match identify {
            Ok(Some(session)) => session,
            Ok(None) | Err(_) => {
                let _ = ws_tx.send(WsMessage::Close(None)).await;
                return;
            }
        };

        // Mark user online
        gateway.mark_connection_open(session.user_id, &session.username);

        // Send Ready event
        let ready = GatewayEvent::Ready {
            user_id: session.user_id,
            username: session.username.clone(),
        };
        let _ = ws_tx
            .send(WsMessage::Text(
                serde_json::to_string(&ready).unwrap().into(),
            ))
            .await;

        // Subscribe to user-specific events
        let user_rx = gateway.subscribe_user(session.user_id);

        let user_id = session.user_id;
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::channel::<GatewayEvent>(256);

        let user_forwarder = spawn_event_forwarder(user_rx, internal_tx.clone());

        // Main loop: read from client and forward events to client
        loop {
            tokio::select! {
                // Client -> Server
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            if let Ok(cmd) = serde_json::from_str::<GatewayCommand>(&text) {
                                match cmd {
                                    GatewayCommand::Typing { channel_id } => {
                                        if can_access_channel(&pool, channel_id, session.user_id).await {
                                            gateway.broadcast_to_channel(channel_id, GatewayEvent::TypingStart {
                                                channel_id,
                                                user_id: session.user_id,
                                                username: session.username.clone(),
                                            });
                                        }
                                    }
                                    GatewayCommand::UpdatePresence { status } => {
                                        // Persist the new status so it survives reconnect, then
                                        // broadcast to all servers this user is in. Persistence is
                                        // best-effort: a DB error is logged but still broadcast,
                                        // since presence is ephemeral UX rather than authoritative.
                                        if let Err(e) = crate::db::update_user_status(&pool, session.user_id, &status).await {
                                            tracing::warn!(user_id = %session.user_id, error = %e, "failed to persist presence update");
                                        }
                                        let event = GatewayEvent::PresenceUpdate {
                                            user_id: session.user_id,
                                            status,
                                        };
                                        for server_id in &session.subscribed_servers {
                                            gateway.broadcast_to_server(*server_id, event.clone());
                                        }
                                    }
                                    GatewayCommand::Subscribe { server_ids } => {
                                        let directory = PostgresSubscriptionDirectory { pool: &pool };
                                        if install_subscriptions(
                                            &gateway,
                                            &mut session,
                                            &server_ids,
                                            &directory,
                                            &internal_tx,
                                            &mut ws_tx,
                                        )
                                        .await
                                        .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) | None => break,
                        _ => {}
                    }
                }
                // Server -> Client
                Some(event) = internal_rx.recv() => {
                    let json = serde_json::to_string(&event).unwrap();
                    if ws_tx.send(WsMessage::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        }

        // Release the connection queue and join the user forwarder before the
        // last-connection cleanup inspects its broadcast receiver count.
        drop(internal_rx);
        drop(internal_tx);
        let _ = user_forwarder.await;

        // Cleanup: mark offline
        gateway.mark_connection_closed(user_id);
    }

    /// Subscribe a connection to receive events for a specific channel
    pub fn subscribe_connection_to_channel(
        &self,
        channel_id: Uuid,
        tx: tokio::sync::mpsc::Sender<GatewayEvent>,
    ) {
        let receiver = self.subscribe_channel(channel_id);
        drop(spawn_event_forwarder(receiver, tx));
    }
}

/// Install every accepted receiver before acknowledging one Subscribe command.
async fn install_subscriptions<D, S>(
    gateway: &Gateway,
    session: &mut Session,
    server_ids: &[Uuid],
    directory: &D,
    internal_tx: &tokio::sync::mpsc::Sender<GatewayEvent>,
    outbound: &mut S,
) -> Result<(), S::Error>
where
    D: SubscriptionDirectory,
    S: futures::Sink<WsMessage> + Unpin,
{
    if !subscriptions_fit(&session.subscribed_servers, server_ids) {
        tracing::warn!(
            user_id = %session.user_id,
            requested = server_ids.len(),
            maximum = MAX_SUBSCRIPTION_BATCH,
            "refusing oversized server subscription batch"
        );
        return Ok(());
    }

    let requested_server_ids = canonical_subscription_ids(server_ids);
    let mut acknowledged_server_ids = Vec::with_capacity(requested_server_ids.len());
    for server_id in requested_server_ids {
        if session.subscribed_servers.contains(&server_id) {
            acknowledged_server_ids.push(server_id);
            continue;
        }

        match directory.is_member(server_id, session.user_id).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    server_id = %server_id,
                    user_id = %session.user_id,
                    username = %session.username,
                    "refusing server subscription: user is not a member"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    server_id = %server_id,
                    user_id = %session.user_id,
                    error = %error,
                    "refusing server subscription: membership check failed"
                );
                continue;
            }
        }

        // Install the server receiver before querying channels. ChannelCreate
        // events raised during that query remain queued until forwarding begins.
        let mut server_rx = gateway.subscribe_server(server_id);
        let channel_ids = match directory.channel_ids(server_id).await {
            Ok(channel_ids) => channel_ids,
            Err(error) => {
                tracing::warn!(
                    server_id = %server_id,
                    user_id = %session.user_id,
                    error = %error,
                    "refusing server subscription: channel lookup failed"
                );
                continue;
            }
        };
        let channel_receivers = channel_ids
            .into_iter()
            .map(|channel_id| gateway.subscribe_channel(channel_id))
            .collect::<Vec<_>>();
        let server_tx = internal_tx.clone();
        let channel_gateway = gateway.clone();
        let dynamic_channel_tx = internal_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = server_tx.closed() => break,
                    event = server_rx.recv() => {
                        let Ok(event) = event else {
                            break;
                        };
                        if let GatewayEvent::ChannelCreate { id: channel_id, .. } = &event {
                            let receiver = channel_gateway.subscribe_channel(*channel_id);
                            drop(spawn_event_forwarder(
                                receiver,
                                dynamic_channel_tx.clone(),
                            ));
                        }
                        if server_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        for channel_rx in channel_receivers {
            drop(spawn_event_forwarder(channel_rx, internal_tx.clone()));
        }

        session.subscribed_servers.insert(server_id);
        acknowledged_server_ids.push(server_id);
    }

    let subscribed = GatewayEvent::Subscribed {
        server_ids: acknowledged_server_ids,
    };
    let subscribed =
        serde_json::to_string(&subscribed).expect("gateway control event must serialize");
    outbound.send(WsMessage::Text(subscribed.into())).await
}

/// Parse and validate the required first application command for a socket.
fn parse_identify(text: &str, jwt_secret: &str) -> Option<Session> {
    let GatewayCommand::Identify { token } = serde_json::from_str(text).ok()? else {
        return None;
    };
    let claims = crate::auth::jwt::validate_token(&token, jwt_secret).ok()?;
    Some(Session {
        user_id: claims.sub,
        username: claims.username,
        subscribed_servers: HashSet::new(),
    })
}

/// Check current server membership before publishing a channel-scoped client event.
async fn can_access_channel(pool: &PgPool, channel_id: Uuid, user_id: Uuid) -> bool {
    let Ok(Some(channel)) = crate::db::get_channel_by_id(pool, channel_id).await else {
        return false;
    };
    matches!(
        crate::db::is_member(pool, channel.server_id, user_id).await,
        Ok(true)
    )
}

/// Check both per-command and per-connection server subscription ceilings.
fn subscriptions_fit(existing: &HashSet<Uuid>, requested: &[Uuid]) -> bool {
    if requested.len() > MAX_SUBSCRIPTION_BATCH {
        return false;
    }
    let unique_new = requested
        .iter()
        .filter(|server_id| !existing.contains(server_id))
        .collect::<HashSet<_>>()
        .len();
    existing.len().saturating_add(unique_new) <= MAX_SUBSCRIPTION_BATCH
}

/// Sort and deduplicate one accepted subscription request for a stable acknowledgement.
fn canonical_subscription_ids(requested: &[Uuid]) -> Vec<Uuid> {
    let mut canonical = requested.to_vec();
    canonical.sort_unstable();
    canonical.dedup();
    canonical
}

#[cfg(test)]
/// Exercises WebSocket command bounds that do not require a live database.
mod tests {
    use super::{
        Gateway, GatewayEvent, MAX_SUBSCRIPTION_BATCH, Session, SubscriptionDirectory,
        canonical_subscription_ids, install_subscriptions, parse_identify, spawn_event_forwarder,
        subscriptions_fit,
    };
    use crate::auth::jwt;
    use async_trait::async_trait;
    use axum::extract::ws::Message as WsMessage;
    use futures::{Sink, SinkExt};
    use std::collections::{HashMap, HashSet};
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    /// Supplies deterministic membership and channel results without PostgreSQL.
    struct FakeSubscriptionDirectory {
        /// Servers accepted for the test user.
        members: HashSet<Uuid>,
        /// Channel identifiers returned for each accepted server.
        channels: HashMap<Uuid, Vec<Uuid>>,
        /// Membership lookups observed by the fake.
        membership_calls: Mutex<Vec<Uuid>>,
        /// Channel inventory lookups observed by the fake.
        channel_calls: Mutex<Vec<Uuid>>,
    }

    /// Implements the subscription directory contract with isolated in-memory data.
    #[async_trait]
    impl SubscriptionDirectory for FakeSubscriptionDirectory {
        /// Record and answer one membership lookup.
        async fn is_member(&self, server_id: Uuid, _user_id: Uuid) -> Result<bool, sqlx::Error> {
            self.membership_calls.lock().unwrap().push(server_id);
            Ok(self.members.contains(&server_id))
        }

        /// Record and answer one channel inventory lookup.
        async fn channel_ids(&self, server_id: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
            self.channel_calls.lock().unwrap().push(server_id);
            Ok(self.channels.get(&server_id).cloned().unwrap_or_default())
        }
    }

    /// Signals the deterministic outbound failure used by lifecycle tests.
    #[derive(Debug, Eq, PartialEq)]
    struct SubscriptionProbeError;

    /// Records outbound frames and injects one event at the acknowledgement boundary.
    struct SubscriptionProbeSink {
        /// Gateway whose installed receivers are observed and exercised.
        gateway: Gateway,
        /// Accepted servers that must have live receivers before acknowledgement.
        expected_servers: Vec<Uuid>,
        /// Accepted channels that must have live receivers before acknowledgement.
        expected_channels: Vec<Uuid>,
        /// Channel that receives the event injected during acknowledgement.
        injection_channel: Uuid,
        /// Event injected after receiver checks and before the ACK frame is recorded.
        injection_event: GatewayEvent,
        /// Whether the deterministic injection hook has already run.
        injected: bool,
        /// Whether the first outbound write must fail after the injection hook.
        fail_first_send: bool,
        /// Serialized text frames observed by the sink.
        frames: Vec<String>,
    }

    /// Implements a ready in-memory WebSocket sink for subscription ordering tests.
    impl Sink<WsMessage> for SubscriptionProbeSink {
        /// This sink fails only when a lifecycle test requests it.
        type Error = SubscriptionProbeError;

        /// Report immediate capacity for every test frame.
        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        /// Verify readiness, inject one queued event, and record the outbound frame.
        fn start_send(mut self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
            if !self.injected {
                for server_id in &self.expected_servers {
                    assert!(
                        self.gateway
                            .server_senders
                            .get(server_id)
                            .is_some_and(|sender| sender.receiver_count() > 0),
                        "server receiver must exist before acknowledgement"
                    );
                }
                for channel_id in &self.expected_channels {
                    assert!(
                        self.gateway
                            .channel_senders
                            .get(channel_id)
                            .is_some_and(|sender| sender.receiver_count() > 0),
                        "channel receiver must exist before acknowledgement"
                    );
                }
                self.gateway
                    .broadcast_to_channel(self.injection_channel, self.injection_event.clone());
                self.injected = true;
                if self.fail_first_send {
                    return Err(SubscriptionProbeError);
                }
            }
            let WsMessage::Text(text) = item else {
                panic!("subscription test only accepts text frames");
            };
            self.frames.push(text.to_string());
            Ok(())
        }

        /// Flush immediately because frames are stored synchronously.
        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        /// Close immediately because the sink owns no external transport.
        fn poll_close(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Accepts a valid Identify command and rejects other first commands.
    #[test]
    fn identify_must_be_first_and_valid() {
        let secret = "correct horse battery staple correct horse";
        let user_id = Uuid::new_v4();
        let token = jwt::create_access_token(user_id, "tester", secret).unwrap();
        let identify = serde_json::json!({"type": "Identify", "data": {"token": token}});

        let session = parse_identify(&identify.to_string(), secret).unwrap();
        assert_eq!(session.user_id, user_id);
        assert!(
            parse_identify(
                r#"{"type":"UpdatePresence","data":{"status":"online"}}"#,
                secret
            )
            .is_none()
        );
        assert!(parse_identify("not-json", secret).is_none());
    }

    /// Keeps a single subscription command within its fixed work ceiling.
    #[test]
    fn subscription_batch_has_a_fixed_ceiling() {
        let existing = HashSet::new();
        let allowed = (0..MAX_SUBSCRIPTION_BATCH)
            .map(|_| Uuid::new_v4())
            .collect::<Vec<_>>();
        let oversized = (0..=MAX_SUBSCRIPTION_BATCH)
            .map(|_| Uuid::new_v4())
            .collect::<Vec<_>>();
        assert!(subscriptions_fit(&existing, &allowed));
        assert!(!subscriptions_fit(&existing, &oversized));

        let existing = allowed.into_iter().collect::<HashSet<_>>();
        assert!(subscriptions_fit(
            &existing,
            &existing.iter().copied().collect::<Vec<_>>()
        ));
        assert!(!subscriptions_fit(&existing, &[Uuid::new_v4()]));
    }

    /// Subscription acknowledgements are canonical and retain their typed wire shape.
    #[test]
    fn subscribed_acknowledgement_is_canonical_and_typed() {
        let lower = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let higher = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let server_ids = canonical_subscription_ids(&[higher, lower, higher]);
        assert_eq!(server_ids, vec![lower, higher]);

        assert_eq!(
            serde_json::to_value(GatewayEvent::Subscribed { server_ids }).unwrap(),
            serde_json::json!({
                "type": "Subscribed",
                "data": {
                    "server_ids": [
                        "11111111-1111-1111-1111-111111111111",
                        "22222222-2222-2222-2222-222222222222"
                    ]
                }
            })
        );
    }

    /// Prevents a quiet connection forwarder from surviving its destination receiver.
    #[tokio::test(flavor = "current_thread")]
    async fn connection_forwarder_delivers_then_releases_its_receiver() {
        let gateway = Gateway::new();
        let channel_id = Uuid::new_v4();
        let event = GatewayEvent::MessageDelete {
            id: Uuid::new_v4(),
            channel_id,
        };
        let (destination, mut connection_rx) = mpsc::channel(1);
        gateway.subscribe_connection_to_channel(channel_id, destination);
        assert_eq!(
            gateway
                .channel_senders
                .get(&channel_id)
                .map_or(0, |sender| sender.receiver_count()),
            1
        );

        gateway.broadcast_to_channel(channel_id, event.clone());
        let delivered = tokio::time::timeout(Duration::from_secs(1), connection_rx.recv())
            .await
            .expect("forwarder must deliver without sleeping")
            .expect("connection destination must remain open");
        assert_eq!(
            serde_json::to_value(delivered).unwrap(),
            serde_json::to_value(&event).unwrap()
        );

        let second_event = GatewayEvent::MessageDelete {
            id: Uuid::new_v4(),
            channel_id,
        };
        gateway.broadcast_to_channel(channel_id, second_event.clone());
        let second_delivered = tokio::time::timeout(Duration::from_secs(1), connection_rx.recv())
            .await
            .expect("forwarder must deliver the second event without sleeping")
            .expect("connection destination must remain open");
        assert_eq!(
            serde_json::to_value(second_delivered).unwrap(),
            serde_json::to_value(second_event).unwrap()
        );
        tokio::task::yield_now().await;
        assert!(matches!(
            connection_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        drop(connection_rx);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let receiver_count = gateway
                    .channel_senders
                    .get(&channel_id)
                    .map_or(0, |sender| sender.receiver_count());
                if receiver_count == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed connection must wake its quiet forwarder");
    }

    /// Prevents last-connection cleanup from leaving an empty per-user sender entry.
    #[tokio::test(flavor = "current_thread")]
    async fn user_forwarder_joins_before_sender_registry_cleanup() {
        let gateway = Gateway::new();
        let user_id = Uuid::new_v4();
        gateway.mark_connection_open(user_id, "subscriber");
        let user_rx = gateway.subscribe_user(user_id);
        let (destination, connection_rx) = mpsc::channel(1);
        let user_forwarder = spawn_event_forwarder(user_rx, destination);
        assert_eq!(
            gateway
                .user_senders
                .get(&user_id)
                .map_or(0, |sender| sender.receiver_count()),
            1
        );

        drop(connection_rx);
        tokio::time::timeout(Duration::from_secs(1), user_forwarder)
            .await
            .expect("closed connection must wake the user forwarder")
            .expect("user forwarder must not panic");
        gateway.mark_connection_closed(user_id);

        assert!(!gateway.online_users.contains_key(&user_id));
        assert!(!gateway.user_senders.contains_key(&user_id));
    }

    /// Prevents a subscription ACK from escaping before every accepted receiver is live.
    #[tokio::test(flavor = "current_thread")]
    async fn subscription_acknowledges_installed_receivers_before_queued_events() {
        let accepted_lower = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let refused = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let accepted_higher = Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap();
        let lower_channel = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1").unwrap();
        let second_lower_channel = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa2").unwrap();
        let higher_channel = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbb1").unwrap();
        let deleted_message = Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();
        let injected_event = GatewayEvent::MessageDelete {
            id: deleted_message,
            channel_id: lower_channel,
        };
        let directory = FakeSubscriptionDirectory {
            members: HashSet::from([accepted_lower, accepted_higher]),
            channels: HashMap::from([
                (accepted_lower, vec![lower_channel, second_lower_channel]),
                (accepted_higher, vec![higher_channel]),
            ]),
            membership_calls: Mutex::new(Vec::new()),
            channel_calls: Mutex::new(Vec::new()),
        };
        let gateway = Gateway::new();
        let mut session = Session {
            user_id: Uuid::new_v4(),
            username: "subscriber".to_string(),
            subscribed_servers: HashSet::new(),
        };
        let (internal_tx, mut internal_rx) = mpsc::channel(8);
        let mut sink = SubscriptionProbeSink {
            gateway: gateway.clone(),
            expected_servers: vec![accepted_lower, accepted_higher],
            expected_channels: vec![lower_channel, second_lower_channel, higher_channel],
            injection_channel: lower_channel,
            injection_event: injected_event.clone(),
            injected: false,
            fail_first_send: false,
            frames: Vec::new(),
        };

        install_subscriptions(
            &gateway,
            &mut session,
            &[accepted_higher, refused, accepted_lower, accepted_higher],
            &directory,
            &internal_tx,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(
            session.subscribed_servers,
            HashSet::from([accepted_lower, accepted_higher])
        );
        assert_eq!(
            *directory.membership_calls.lock().unwrap(),
            vec![accepted_lower, refused, accepted_higher]
        );
        assert_eq!(
            *directory.channel_calls.lock().unwrap(),
            vec![accepted_lower, accepted_higher]
        );
        assert_eq!(sink.frames.len(), 1, "ACK must use the direct sink");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&sink.frames[0]).unwrap(),
            serde_json::json!({
                "type": "Subscribed",
                "data": {
                    "server_ids": [
                        "11111111-1111-1111-1111-111111111111",
                        "33333333-3333-3333-3333-333333333333"
                    ]
                }
            })
        );

        let queued_event = tokio::time::timeout(Duration::from_secs(1), internal_rx.recv())
            .await
            .expect("installed channel receiver must forward without sleeping")
            .expect("subscription forwarding channel must remain open");
        assert_eq!(
            serde_json::to_value(&queued_event).unwrap(),
            serde_json::to_value(&injected_event).unwrap()
        );
        sink.send(WsMessage::Text(
            serde_json::to_string(&queued_event).unwrap().into(),
        ))
        .await
        .unwrap();
        assert_eq!(sink.frames.len(), 2);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&sink.frames[1]).unwrap(),
            serde_json::to_value(injected_event).unwrap()
        );
    }

    /// Prevents quiet forwarding tasks from retaining receivers after an ACK write fails.
    #[tokio::test(flavor = "current_thread")]
    async fn failed_subscription_ack_releases_installed_receivers() {
        let server_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let channel_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let injected_event = GatewayEvent::MessageDelete {
            id: Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap(),
            channel_id,
        };
        let directory = FakeSubscriptionDirectory {
            members: HashSet::from([server_id]),
            channels: HashMap::from([(server_id, vec![channel_id])]),
            membership_calls: Mutex::new(Vec::new()),
            channel_calls: Mutex::new(Vec::new()),
        };
        let gateway = Gateway::new();
        let mut session = Session {
            user_id: Uuid::new_v4(),
            username: "subscriber".to_string(),
            subscribed_servers: HashSet::new(),
        };
        let (internal_tx, internal_rx) = mpsc::channel(8);
        let mut sink = SubscriptionProbeSink {
            gateway: gateway.clone(),
            expected_servers: vec![server_id],
            expected_channels: vec![channel_id],
            injection_channel: channel_id,
            injection_event: injected_event,
            injected: false,
            fail_first_send: true,
            frames: Vec::new(),
        };

        let error = install_subscriptions(
            &gateway,
            &mut session,
            &[server_id],
            &directory,
            &internal_tx,
            &mut sink,
        )
        .await
        .expect_err("configured ACK write must fail");
        assert_eq!(error, SubscriptionProbeError);
        assert!(sink.frames.is_empty());

        drop(internal_rx);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let server_receivers = gateway
                    .server_senders
                    .get(&server_id)
                    .map_or(0, |sender| sender.receiver_count());
                let channel_receivers = gateway
                    .channel_senders
                    .get(&channel_id)
                    .map_or(0, |sender| sender.receiver_count());
                if server_receivers == 0 && channel_receivers == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed connection must wake and stop every quiet forwarder");
    }
}
