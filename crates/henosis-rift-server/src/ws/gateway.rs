//! Authenticated WebSocket sessions and scoped Rift event fan-out.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::auth::middleware::bind_claims_to_account;
use crate::models::leadership::RoomFence;

/// Maximum time a newly upgraded socket may remain unauthenticated.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time a socket write may retain shared authorization locks.
const AUTHORIZED_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time allowed for PostgreSQL to release an authorization transaction cleanly.
const AUTHORIZATION_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of servers accepted in one subscription command.
const MAX_SUBSCRIPTION_BATCH: usize = 100;

/// Events sent from server -> client
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// One channel message was durably created.
    MessageCreate {
        /// Stable identity shared by the durable outbox row and every retry.
        event_id: Uuid,
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
    /// One channel message was durably edited.
    MessageUpdate {
        /// Stable identity shared by the durable outbox row and every retry.
        event_id: Uuid,
        id: Uuid,
        channel_id: Uuid,
        content: String,
        edited_at: String,
    },
    /// One channel message was durably deleted.
    MessageDelete {
        /// Stable identity shared by the durable outbox row and every retry.
        event_id: Uuid,
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

/// Exposes durable routing identities carried by message mutation events.
impl GatewayEvent {
    /// Return the stable event and channel identifiers for a durable message mutation.
    pub(crate) fn message_outbox_identity(&self) -> Option<(Uuid, Uuid)> {
        match self {
            Self::MessageCreate {
                event_id,
                channel_id,
                ..
            }
            | Self::MessageUpdate {
                event_id,
                channel_id,
                ..
            }
            | Self::MessageDelete {
                event_id,
                channel_id,
                ..
            } => Some((*event_id, *channel_id)),
            _ => None,
        }
    }
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
#[derive(Clone)]
struct Session {
    user_id: Uuid,
    username: String,
    /// Server-confirmed agent classification; JWT claims do not control it.
    is_agent: bool,
    /// Optional room leadership capability carried by a bridge-issued token.
    managed_fence: Option<RoomFence>,
    /// Server IDs this user is subscribed to
    subscribed_servers: HashSet<Uuid>,
}

/// Provides pure managed-session scope decisions before database or receiver work.
impl Session {
    /// Return whether this session's optional managed fence targets one exact server.
    fn managed_fence_targets(&self, server_id: Uuid) -> bool {
        self.managed_fence
            .as_ref()
            .is_none_or(|fence| fence.server_id == server_id)
    }
}

/// Identifies one membership whose active sockets must stop receiving room events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MembershipRevocation {
    /// Room whose membership was removed.
    server_id: Uuid,
    /// Former member whose subscribed sockets must close.
    user_id: Uuid,
}

/// Binds a queued event to the server membership that authorizes its delivery.
#[derive(Clone, Debug)]
struct ScopedGatewayEvent {
    /// Authoritative server routing key retained through the internal queue.
    server_id: Uuid,
    /// Event whose embedded identifiers were checked against its broadcast route.
    event: GatewayEvent,
}

/// Describes the exact broadcast key under which an event was published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventRoute {
    /// A server-wide broadcast carrying one authoritative server identifier.
    Server(Uuid),
    /// A channel broadcast carrying both its parent server and channel identifiers.
    Channel {
        /// Parent server whose membership authorizes the event.
        server_id: Uuid,
        /// Channel sender on which the event arrived.
        channel_id: Uuid,
    },
}

/// Returns the server membership bound to one event route.
impl EventRoute {
    /// Extract the authoritative parent server identifier.
    fn server_id(self) -> Uuid {
        match self {
            Self::Server(server_id) | Self::Channel { server_id, .. } => server_id,
        }
    }
}

/// Owns database locks that keep an authorization decision current through its effect.
struct SessionFenceLease {
    /// Transaction holding membership, account, and applicable agent-fence locks.
    transaction: Option<Transaction<'static, Postgres>>,
}

/// Releases authorization locks explicitly after their protected side effect completes.
impl SessionFenceLease {
    /// Construct a no-lock lease for an unscoped human or standalone-agent control frame.
    fn unlocked() -> Self {
        Self { transaction: None }
    }

    /// Commit the read-only authorization transaction and report whether release succeeded.
    async fn release(mut self) -> bool {
        let Some(transaction) = self.transaction.take() else {
            return true;
        };
        match tokio::time::timeout(AUTHORIZATION_RELEASE_TIMEOUT, transaction.commit()).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::error!(error = %error, "WebSocket authorization lease release failed");
                false
            }
            Err(_) => {
                tracing::error!("WebSocket authorization lease release timed out");
                false
            }
        }
    }
}

/// Rechecks leadership authority at each WebSocket side-effect boundary.
#[async_trait]
trait SessionFenceAuthorizer: Send + Sync {
    /// Return whether the connected identity remains authorized to receive events.
    async fn session_is_current(&self, session: &Session) -> bool;

    /// Return whether a particular server receiver may be installed for this session.
    async fn subscription_is_current(&self, session: &Session, server_id: Uuid) -> bool;

    /// Return whether the session still has current membership in one server.
    async fn membership_is_current(&self, session: &Session, server_id: Uuid) -> bool;

    /// Return whether one dynamically announced channel still belongs to its routed server.
    async fn channel_belongs_to_server(&self, channel_id: Uuid, server_id: Uuid) -> bool;

    /// Acquire locks that preserve identity, membership, and fence authority through one effect.
    async fn acquire_delivery_lease(
        &self,
        session: &Session,
        server_id: Option<Uuid>,
    ) -> Option<SessionFenceLease>;
}

/// Authorizes WebSocket operations through the current PostgreSQL room state.
struct PostgresSessionFenceAuthorizer {
    /// Shared Rift database pool.
    pool: PgPool,
}

/// Implements fail-closed managed-agent authorization against PostgreSQL.
#[async_trait]
impl SessionFenceAuthorizer for PostgresSessionFenceAuthorizer {
    /// Revalidate managed agents while preserving human and standalone-agent compatibility.
    async fn session_is_current(&self, session: &Session) -> bool {
        if !session.is_agent {
            return true;
        }
        let authorized = match session.managed_fence.as_ref() {
            Some(fence) => {
                crate::db::agent_control::agent_room_fence_is_current(
                    &self.pool,
                    session.user_id,
                    fence,
                )
                .await
            }
            None => {
                crate::db::agent_control::agent_requires_room_fence(&self.pool, session.user_id)
                    .await
                    .map(|required| !required)
            }
        };
        match authorized {
            Ok(authorized) => authorized,
            Err(error) => {
                tracing::error!(user_id = %session.user_id, error = %error, "WebSocket fence lookup failed");
                false
            }
        }
    }

    /// Require an exact current room fence before installing managed-agent receivers.
    async fn subscription_is_current(&self, session: &Session, server_id: Uuid) -> bool {
        if !session.managed_fence_targets(server_id) {
            return false;
        }
        if !self.session_is_current(session).await {
            return false;
        }
        if !session.is_agent || session.managed_fence.is_none() {
            return true;
        }
        crate::db::agent_control::require_room_fence(
            &self.pool,
            server_id,
            session.managed_fence.as_ref(),
        )
        .await
        .is_ok()
    }

    /// Revalidate human and agent membership and fail closed when PostgreSQL is unavailable.
    async fn membership_is_current(&self, session: &Session, server_id: Uuid) -> bool {
        match crate::db::is_member(&self.pool, server_id, session.user_id).await {
            Ok(is_member) => is_member,
            Err(error) => {
                tracing::error!(
                    server_id = %server_id,
                    user_id = %session.user_id,
                    error = %error,
                    "WebSocket membership lookup failed"
                );
                false
            }
        }
    }

    /// Bind ChannelCreate receiver installation to canonical channel ownership.
    async fn channel_belongs_to_server(&self, channel_id: Uuid, server_id: Uuid) -> bool {
        match crate::db::get_channel_by_id(&self.pool, channel_id).await {
            Ok(Some(channel)) => channel.server_id == server_id,
            Ok(None) => false,
            Err(error) => {
                tracing::error!(
                    server_id = %server_id,
                    channel_id = %channel_id,
                    error = %error,
                    "WebSocket channel ownership lookup failed"
                );
                false
            }
        }
    }

    /// Lock membership, canonical identity, and any applicable agent fence through the effect.
    async fn acquire_delivery_lease(
        &self,
        session: &Session,
        server_id: Option<Uuid>,
    ) -> Option<SessionFenceLease> {
        let protected_server_id = match (server_id, session.managed_fence.as_ref()) {
            (Some(server_id), _) => server_id,
            (None, Some(fence)) => fence.server_id,
            (None, None) => {
                return self
                    .session_is_current(session)
                    .await
                    .then(SessionFenceLease::unlocked);
            }
        };
        if !session.managed_fence_targets(protected_server_id) {
            return None;
        }
        let presented_fence = session.managed_fence.as_ref();
        if presented_fence.is_some() && !session.is_agent {
            return None;
        }
        // Standalone agents must remain outside managed rooms while humans need
        // only their membership and canonical account rows serialized.
        if presented_fence.is_none() && session.is_agent && !self.session_is_current(session).await
        {
            return None;
        }

        let mut transaction = match self.pool.begin().await {
            Ok(transaction) => transaction,
            Err(error) => {
                tracing::error!(
                    server_id = %protected_server_id,
                    user_id = %session.user_id,
                    error = %error,
                    "WebSocket authorization transaction could not start"
                );
                return None;
            }
        };
        let fence_error = if session.is_agent {
            crate::db::agent_control::require_room_fence_locked(
                &mut transaction,
                protected_server_id,
                presented_fence,
            )
            .await
            .err()
        } else {
            None
        };
        if let Some(error) = fence_error {
            tracing::warn!(
                server_id = %protected_server_id,
                user_id = %session.user_id,
                error = %error,
                "WebSocket agent fence lease was rejected"
            );
            let _ = transaction.rollback().await;
            return None;
        }

        let canonical_is_agent = sqlx::query_scalar::<_, bool>(
            r#"SELECT users.is_agent
               FROM members
               INNER JOIN users ON users.id = members.user_id
               WHERE members.server_id = $1
                 AND members.user_id = $2
               FOR SHARE OF members, users"#,
        )
        .bind(protected_server_id)
        .bind(session.user_id)
        .fetch_optional(&mut *transaction)
        .await;
        match canonical_is_agent {
            Ok(Some(is_agent)) if is_agent == session.is_agent => Some(SessionFenceLease {
                transaction: Some(transaction),
            }),
            Ok(Some(_) | None) => {
                let _ = transaction.rollback().await;
                None
            }
            Err(error) => {
                tracing::error!(
                    server_id = %protected_server_id,
                    user_id = %session.user_id,
                    error = %error,
                    "WebSocket membership serialization failed"
                );
                let _ = transaction.rollback().await;
                None
            }
        }
    }
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
#[cfg(test)]
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

/// Return whether a session still holds both identity authority and server membership.
async fn scoped_delivery_is_current(
    authorizer: &dyn SessionFenceAuthorizer,
    session: &Session,
    server_id: Uuid,
) -> bool {
    session.managed_fence_targets(server_id)
        && authorizer.session_is_current(session).await
        && authorizer.membership_is_current(session, server_id).await
}

/// Revalidate one server scope and lock its authority through the next side effect.
async fn acquire_scoped_delivery_lease(
    authorizer: &dyn SessionFenceAuthorizer,
    session: &Session,
    server_id: Uuid,
) -> Option<SessionFenceLease> {
    if !scoped_delivery_is_current(authorizer, session, server_id).await {
        return None;
    }
    authorizer
        .acquire_delivery_lease(session, Some(server_id))
        .await
}

/// Revalidate every room membership before an unscoped client presence side effect.
async fn all_subscribed_memberships_are_current(
    authorizer: &dyn SessionFenceAuthorizer,
    session: &Session,
) -> bool {
    if !authorizer.session_is_current(session).await {
        return false;
    }
    for server_id in &session.subscribed_servers {
        if !scoped_delivery_is_current(authorizer, session, *server_id).await {
            return false;
        }
    }
    true
}

/// Revalidate one queued server scope immediately before writing its socket frame.
async fn send_scoped_event_if_current<S>(
    authorizer: &dyn SessionFenceAuthorizer,
    session: &Session,
    queued: ScopedGatewayEvent,
    outbound: &mut S,
) -> Result<bool, S::Error>
where
    S: futures::Sink<WsMessage> + Unpin,
{
    let Some(lease) = acquire_scoped_delivery_lease(authorizer, session, queued.server_id).await
    else {
        return Ok(false);
    };
    let json =
        serde_json::to_string(&queued.event).expect("gateway event serialization must succeed");
    let send_result = tokio::time::timeout(
        AUTHORIZED_SEND_TIMEOUT,
        outbound.send(WsMessage::Text(json.into())),
    )
    .await;
    let released = lease.release().await;
    match send_result {
        Ok(Ok(())) => Ok(released),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            tracing::warn!(
                user_id = %session.user_id,
                server_id = %queued.server_id,
                "WebSocket authorized event send timed out"
            );
            Ok(false)
        }
    }
}

/// Revalidate session authority immediately before a Ready or Subscribe control frame.
async fn send_session_event_if_current<S>(
    authorizer: &dyn SessionFenceAuthorizer,
    session: &Session,
    event: &GatewayEvent,
    outbound: &mut S,
) -> Result<bool, S::Error>
where
    S: futures::Sink<WsMessage> + Unpin,
{
    if !authorizer.session_is_current(session).await {
        return Ok(false);
    }
    let Some(lease) = authorizer.acquire_delivery_lease(session, None).await else {
        return Ok(false);
    };
    let json = serde_json::to_string(event).expect("gateway control serialization must succeed");
    let send_result = tokio::time::timeout(
        AUTHORIZED_SEND_TIMEOUT,
        outbound.send(WsMessage::Text(json.into())),
    )
    .await;
    let released = lease.release().await;
    match send_result {
        Ok(Ok(())) => Ok(released),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            tracing::warn!(
                user_id = %session.user_id,
                "WebSocket authorized control send timed out"
            );
            Ok(false)
        }
    }
}

/// Reject events whose embedded identifiers do not match their broadcast routing key.
fn event_matches_route(route: EventRoute, event: &GatewayEvent) -> bool {
    match (route, event) {
        (
            EventRoute::Channel { channel_id, .. },
            GatewayEvent::MessageCreate {
                channel_id: event_channel_id,
                ..
            }
            | GatewayEvent::MessageUpdate {
                channel_id: event_channel_id,
                ..
            }
            | GatewayEvent::MessageDelete {
                channel_id: event_channel_id,
                ..
            }
            | GatewayEvent::TypingStart {
                channel_id: event_channel_id,
                ..
            },
        ) => channel_id == *event_channel_id,
        (
            EventRoute::Server(server_id),
            GatewayEvent::MemberJoin {
                server_id: event_server_id,
                ..
            }
            | GatewayEvent::MemberLeave {
                server_id: event_server_id,
                ..
            }
            | GatewayEvent::ChannelCreate {
                server_id: event_server_id,
                ..
            }
            | GatewayEvent::ChannelDelete {
                server_id: event_server_id,
                ..
            }
            | GatewayEvent::RoleDelete {
                server_id: event_server_id,
                ..
            },
        ) => server_id == *event_server_id,
        (
            EventRoute::Server(server_id),
            GatewayEvent::RoleCreate {
                server_id: event_server_id,
                role,
            }
            | GatewayEvent::RoleUpdate {
                server_id: event_server_id,
                role,
            },
        ) => server_id == *event_server_id && server_id == role.server_id,
        (EventRoute::Server(_), GatewayEvent::PresenceUpdate { .. }) => true,
        _ => false,
    }
}

/// Validate canonical ownership before a ChannelCreate event installs a live receiver.
async fn dynamic_channel_route_is_current(
    authorizer: &dyn SessionFenceAuthorizer,
    server_id: Uuid,
    event: &GatewayEvent,
) -> bool {
    let channel_id = match event {
        GatewayEvent::ChannelCreate { id, .. } => *id,
        _ => return true,
    };
    if authorizer
        .channel_belongs_to_server(channel_id, server_id)
        .await
    {
        return true;
    }
    tracing::error!(
        %server_id,
        %channel_id,
        "dropping cross-wired WebSocket ChannelCreate event"
    );
    false
}

/// Forward only route-bound events while identity and membership remain current.
fn spawn_scoped_event_forwarder(
    mut source: broadcast::Receiver<GatewayEvent>,
    destination: tokio::sync::mpsc::Sender<ScopedGatewayEvent>,
    session: Session,
    route: EventRoute,
    authorizer: Arc<dyn SessionFenceAuthorizer>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = destination.closed() => break,
                event = source.recv() => {
                    let Ok(event) = event else { break; };
                    if !event_matches_route(route, &event) {
                        tracing::error!(?route, "dropping WebSocket event with mismatched routing key");
                        continue;
                    }
                    let server_id = route.server_id();
                    let Some(lease) = acquire_scoped_delivery_lease(
                        authorizer.as_ref(),
                        &session,
                        server_id,
                    ).await else {
                        break;
                    };
                    let queued = ScopedGatewayEvent { server_id, event };
                    let enqueued = destination.try_send(queued).is_ok();
                    let released = lease.release().await;
                    if !enqueued || !released { break; }
                }
            }
        }
    })
}

/// Describes whether a subscription request installed receivers or was fence-rejected.
#[derive(Debug, Eq, PartialEq)]
enum SubscriptionInstallOutcome {
    /// All authorized receivers were installed and acknowledged.
    Installed,
    /// A managed identity was stale or could not be verified.
    FenceRejected,
}

/// Central hub for all WebSocket connections
#[derive(Clone)]
pub struct Gateway {
    /// Per-channel broadcast senders. Any event for a channel goes here.
    channel_senders: Arc<DashMap<Uuid, broadcast::Sender<GatewayEvent>>>,
    /// Per-server broadcast senders for server-wide events (member join/leave, channel create/delete)
    server_senders: Arc<DashMap<Uuid, broadcast::Sender<GatewayEvent>>>,
    /// Membership revocations delivered to every active socket on this Rift instance.
    membership_revocations: broadcast::Sender<MembershipRevocation>,
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
        let (membership_revocations, _) = broadcast::channel(256);
        Self {
            channel_senders: Arc::new(DashMap::new()),
            server_senders: Arc::new(DashMap::new()),
            membership_revocations,
            online_users: Arc::new(DashMap::new()),
            connection_counts: Arc::new(DashMap::new()),
        }
    }

    /// Broadcast an event to all subscribers of a channel
    pub(crate) fn broadcast_to_channel(&self, channel_id: Uuid, event: GatewayEvent) -> bool {
        if !event_matches_route(
            EventRoute::Channel {
                server_id: Uuid::nil(),
                channel_id,
            },
            &event,
        ) {
            tracing::error!(%channel_id, "refusing WebSocket channel event with mismatched routing key");
            return false;
        }
        if let Some(sender) = self.channel_senders.get(&channel_id) {
            let _ = sender.send(event);
        }
        true
    }

    /// Broadcast an event to all members of a server
    pub(crate) fn broadcast_to_server(&self, server_id: Uuid, event: GatewayEvent) -> bool {
        if !event_matches_route(EventRoute::Server(server_id), &event) {
            tracing::error!(%server_id, "refusing WebSocket server event with mismatched routing key");
            return false;
        }
        if let Some(sender) = self.server_senders.get(&server_id) {
            let _ = sender.send(event);
        }
        true
    }

    /// Notify every local socket before and after one membership mutation.
    pub(crate) fn revoke_membership(&self, server_id: Uuid, user_id: Uuid) {
        let _ = self
            .membership_revocations
            .send(MembershipRevocation { server_id, user_id });
    }

    /// Subscribe to events published for one channel.
    pub(crate) fn subscribe_channel(&self, channel_id: Uuid) -> broadcast::Receiver<GatewayEvent> {
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
        }
    }

    /// Handle a new WebSocket connection
    pub async fn handle_connection(
        &self,
        socket: WebSocket,
        human_jwt_secret: String,
        agent_jwt_secret: String,
        pool: PgPool,
    ) {
        let (mut ws_tx, mut ws_rx) = socket.split();
        let gateway = self.clone();

        // Bound the unauthenticated phase and require Identify as the first text command.
        let identify = tokio::time::timeout(IDENTIFY_TIMEOUT, async {
            loop {
                match ws_rx.next().await {
                    Some(Ok(WsMessage::Text(text))) => {
                        break parse_identify_claims(&text, &human_jwt_secret, &agent_jwt_secret);
                    }
                    Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => continue,
                    _ => break None,
                }
            }
        })
        .await;
        let claims = match identify {
            Ok(Some(claims)) => claims,
            Ok(None) | Err(_) => {
                let _ = ws_tx.send(WsMessage::Close(None)).await;
                return;
            }
        };
        let Some(user) = crate::db::get_user_by_id(&pool, claims.sub)
            .await
            .ok()
            .flatten()
        else {
            let _ = ws_tx.send(WsMessage::Close(None)).await;
            return;
        };
        let Some(mut session) = bind_identified_session(claims, user.username, user.is_agent)
        else {
            let _ = ws_tx.send(WsMessage::Close(None)).await;
            return;
        };
        let authorizer: Arc<dyn SessionFenceAuthorizer> =
            Arc::new(PostgresSessionFenceAuthorizer { pool: pool.clone() });
        if !authorizer.session_is_current(&session).await {
            let _ = ws_tx.send(WsMessage::Close(None)).await;
            return;
        }
        let mut membership_revocations = gateway.membership_revocations.subscribe();

        // Mark user online
        gateway.mark_connection_open(session.user_id, &session.username);

        let ready = GatewayEvent::Ready {
            user_id: session.user_id,
            username: session.username.clone(),
        };
        if !matches!(
            send_session_event_if_current(authorizer.as_ref(), &session, &ready, &mut ws_tx).await,
            Ok(true)
        ) {
            gateway.mark_connection_closed(session.user_id);
            let _ = ws_tx.send(WsMessage::Close(None)).await;
            return;
        }

        let user_id = session.user_id;
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::channel::<ScopedGatewayEvent>(256);

        // Main loop: read from client and forward events to client
        loop {
            tokio::select! {
                biased;
                // Membership removal takes priority over queued room events. A
                // lagged revocation stream fails closed because its missed scope
                // cannot be reconstructed safely.
                revocation = membership_revocations.recv() => {
                    match revocation {
                        Ok(revocation)
                            if revocation.user_id == session.user_id
                                && session.subscribed_servers.contains(&revocation.server_id) =>
                        {
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                // Client -> Server
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            if let Ok(cmd) = serde_json::from_str::<GatewayCommand>(&text) {
                                match cmd {
                                    GatewayCommand::Typing { channel_id } => {
                                        if session.is_agent { continue; }
                                        if can_access_channel(&pool, channel_id, session.user_id).await {
                                            gateway.broadcast_to_channel(channel_id, GatewayEvent::TypingStart {
                                                channel_id,
                                                user_id: session.user_id,
                                                username: session.username.clone(),
                                            });
                                        }
                                    }
                                    GatewayCommand::UpdatePresence { status } => {
                                        if session.is_agent { continue; }
                                        if !all_subscribed_memberships_are_current(
                                            authorizer.as_ref(),
                                            &session,
                                        ).await {
                                            break;
                                        }
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
                                        match install_subscriptions(
                                            &gateway,
                                            &mut session,
                                            &server_ids,
                                            &directory,
                                            authorizer.clone(),
                                            &internal_tx,
                                            &mut ws_tx,
                                        )
                                        .await {
                                            Ok(SubscriptionInstallOutcome::Installed) => {}
                                            Ok(SubscriptionInstallOutcome::FenceRejected) | Err(_) => break,
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
                Some(queued) = internal_rx.recv() => {
                    match send_scoped_event_if_current(
                        authorizer.as_ref(),
                        &session,
                        queued,
                        &mut ws_tx,
                    ).await {
                        Ok(true) => {}
                        Ok(false) | Err(_) => break,
                    }
                }
            }
        }

        // Release the connection queue before clearing online state.
        drop(internal_rx);
        drop(internal_tx);

        // Cleanup: mark offline
        gateway.mark_connection_closed(user_id);
    }

    /// Subscribe a connection to receive events for a specific channel
    #[cfg(test)]
    fn subscribe_connection_to_channel(
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
    authorizer: Arc<dyn SessionFenceAuthorizer>,
    internal_tx: &tokio::sync::mpsc::Sender<ScopedGatewayEvent>,
    outbound: &mut S,
) -> Result<SubscriptionInstallOutcome, S::Error>
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
        return Ok(SubscriptionInstallOutcome::Installed);
    }

    let requested_server_ids = canonical_subscription_ids(server_ids);
    let mut acknowledged_server_ids = Vec::with_capacity(requested_server_ids.len());
    for server_id in requested_server_ids {
        if !authorizer.subscription_is_current(session, server_id).await {
            return Ok(SubscriptionInstallOutcome::FenceRejected);
        }
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
            .map(|channel_id| (channel_id, gateway.subscribe_channel(channel_id)))
            .collect::<Vec<_>>();
        let server_tx = internal_tx.clone();
        let channel_gateway = gateway.clone();
        let dynamic_channel_tx = internal_tx.clone();
        let server_session = session.clone();
        let server_authorizer = authorizer.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = server_tx.closed() => break,
                    event = server_rx.recv() => {
                        let Ok(event) = event else {
                            break;
                        };
                        let route = EventRoute::Server(server_id);
                        if !event_matches_route(route, &event) {
                            tracing::error!(%server_id, "dropping WebSocket server event with mismatched routing key");
                            continue;
                        }
                        if !dynamic_channel_route_is_current(
                            server_authorizer.as_ref(),
                            server_id,
                            &event,
                        ).await {
                            continue;
                        }
                        let Some(lease) = acquire_scoped_delivery_lease(
                            server_authorizer.as_ref(),
                            &server_session,
                            server_id,
                        ).await else {
                            break;
                        };
                        if let GatewayEvent::ChannelCreate { id: channel_id, .. } = &event {
                            let receiver = channel_gateway.subscribe_channel(*channel_id);
                            drop(spawn_scoped_event_forwarder(
                                receiver,
                                dynamic_channel_tx.clone(),
                                server_session.clone(),
                                EventRoute::Channel {
                                    server_id,
                                    channel_id: *channel_id,
                                },
                                server_authorizer.clone(),
                            ));
                        }
                        let enqueued = server_tx
                            .try_send(ScopedGatewayEvent { server_id, event })
                            .is_ok();
                        let released = lease.release().await;
                        if !enqueued || !released {
                            break;
                        }
                    }
                }
            }
        });

        for (channel_id, channel_rx) in channel_receivers {
            drop(spawn_scoped_event_forwarder(
                channel_rx,
                internal_tx.clone(),
                session.clone(),
                EventRoute::Channel {
                    server_id,
                    channel_id,
                },
                authorizer.clone(),
            ));
        }

        session.subscribed_servers.insert(server_id);
        acknowledged_server_ids.push(server_id);
    }

    if !authorizer.session_is_current(session).await {
        return Ok(SubscriptionInstallOutcome::FenceRejected);
    }
    for server_id in &session.subscribed_servers {
        if !scoped_delivery_is_current(authorizer.as_ref(), session, *server_id).await {
            return Ok(SubscriptionInstallOutcome::FenceRejected);
        }
    }
    let subscribed = GatewayEvent::Subscribed {
        server_ids: acknowledged_server_ids,
    };
    if !send_session_event_if_current(authorizer.as_ref(), session, &subscribed, outbound).await? {
        return Ok(SubscriptionInstallOutcome::FenceRejected);
    }
    Ok(SubscriptionInstallOutcome::Installed)
}

/// Parse and cryptographically validate the required first application command.
fn parse_identify_claims(
    text: &str,
    human_jwt_secret: &str,
    agent_jwt_secret: &str,
) -> Option<Claims> {
    let GatewayCommand::Identify { token } = serde_json::from_str(text).ok()? else {
        return None;
    };
    crate::auth::jwt::validate_access_token(&token, human_jwt_secret, agent_jwt_secret).ok()
}

/// Bind verified claims to a canonical database account before creating a live session.
fn bind_identified_session(
    claims: Claims,
    canonical_username: String,
    is_agent: bool,
) -> Option<Session> {
    let auth = bind_claims_to_account(claims, canonical_username, is_agent).ok()?;
    Some(Session {
        user_id: auth.user_id,
        username: auth.username,
        is_agent: auth.is_agent,
        managed_fence: auth.managed_fence,
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
        EventRoute, Gateway, GatewayEvent, MAX_SUBSCRIPTION_BATCH, PostgresSessionFenceAuthorizer,
        ScopedGatewayEvent, Session, SessionFenceAuthorizer, SubscriptionDirectory,
        SubscriptionInstallOutcome, bind_identified_session, canonical_subscription_ids,
        install_subscriptions, parse_identify_claims, send_scoped_event_if_current,
        send_session_event_if_current, spawn_scoped_event_forwarder, subscriptions_fit,
    };
    use crate::auth::jwt::{self, Claims, TokenKind};
    use crate::models::leadership::RoomFence;
    use async_trait::async_trait;
    use axum::extract::ws::Message as WsMessage;
    use chrono::{Duration as ChronoDuration, Utc};
    use futures::{Sink, SinkExt};
    use jsonwebtoken::{EncodingKey, Header, encode};
    use sqlx::postgres::PgPoolOptions;
    use std::collections::{HashMap, HashSet};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};
    use uuid::Uuid;

    /// Connect to the opt-in PostgreSQL database for authorization-lock integration tests.
    async fn live_test_pool() -> Option<sqlx::PgPool> {
        let Some(database_url) = std::env::var_os("HENOSIS_RIFT_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping live WebSocket authorization-lock test: HENOSIS_RIFT_TEST_DATABASE_URL is unset"
            );
            return None;
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url.to_string_lossy())
            .await
            .expect("WebSocket test database must be reachable");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("WebSocket test database migrations must apply");
        Some(pool)
    }

    /// Sign one explicit WebSocket test token under the selected authority key.
    fn sign_test_token(
        user_id: Uuid,
        username: &str,
        token_kind: TokenKind,
        secret: &str,
    ) -> String {
        let now = Utc::now();
        encode(
            &Header::default(),
            &Claims {
                sub: user_id,
                username: username.to_string(),
                token_kind,
                iat: now.timestamp(),
                exp: (now + ChronoDuration::hours(1)).timestamp(),
                managed_fence: None,
            },
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("WebSocket test token must encode")
    }

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

    /// Allows all operations for unfenced compatibility tests.
    struct AllowingFenceAuthorizer;

    /// Rejects all checks to model a stale or unavailable managed lease.
    struct RejectingFenceAuthorizer;

    /// Keeps identity authority current while modeling a revoked human membership.
    struct RevokedHumanMembershipAuthorizer;

    /// Accepts the session but rejects a dynamically announced channel's parent binding.
    struct CrossWiredChannelAuthorizer;

    /// Implements deterministic fence acceptance for existing socket tests.
    #[async_trait]
    impl SessionFenceAuthorizer for AllowingFenceAuthorizer {
        /// Permit delivery without a database dependency.
        async fn session_is_current(&self, _session: &Session) -> bool {
            true
        }

        /// Permit receiver installation without a database dependency.
        async fn subscription_is_current(&self, _session: &Session, _server_id: Uuid) -> bool {
            true
        }

        /// Preserve membership for ordinary subscription and delivery tests.
        async fn membership_is_current(&self, _session: &Session, _server_id: Uuid) -> bool {
            true
        }

        /// Preserve canonical channel ownership for ordinary subscription tests.
        async fn channel_belongs_to_server(&self, _channel_id: Uuid, _server_id: Uuid) -> bool {
            true
        }

        /// Provide an unlocked lease for deterministic compatibility tests.
        async fn acquire_delivery_lease(
            &self,
            _session: &Session,
            _server_id: Option<Uuid>,
        ) -> Option<super::SessionFenceLease> {
            Some(super::SessionFenceLease::unlocked())
        }
    }

    /// Implements deterministic stale-fence rejection for adversarial tests.
    #[async_trait]
    impl SessionFenceAuthorizer for RejectingFenceAuthorizer {
        /// Reject delivery to prove queued events fail closed.
        async fn session_is_current(&self, _session: &Session) -> bool {
            false
        }

        /// Reject subscriptions before membership or receiver work begins.
        async fn subscription_is_current(&self, _session: &Session, _server_id: Uuid) -> bool {
            false
        }

        /// Reject membership when the backing authority is stale or unavailable.
        async fn membership_is_current(&self, _session: &Session, _server_id: Uuid) -> bool {
            false
        }

        /// Reject dynamic channel ownership under a stale authority.
        async fn channel_belongs_to_server(&self, _channel_id: Uuid, _server_id: Uuid) -> bool {
            false
        }

        /// Reject the side-effect lease as stale or unavailable.
        async fn acquire_delivery_lease(
            &self,
            _session: &Session,
            _server_id: Option<Uuid>,
        ) -> Option<super::SessionFenceLease> {
            None
        }
    }

    /// Separates current authentication from the membership revocation under test.
    #[async_trait]
    impl SessionFenceAuthorizer for RevokedHumanMembershipAuthorizer {
        /// Preserve the human identity so only membership revocation can stop delivery.
        async fn session_is_current(&self, _session: &Session) -> bool {
            true
        }

        /// Preserve initial subscription compatibility for the pre-existing socket.
        async fn subscription_is_current(&self, _session: &Session, _server_id: Uuid) -> bool {
            true
        }

        /// Model membership removed after the socket's initial subscription.
        async fn membership_is_current(&self, _session: &Session, _server_id: Uuid) -> bool {
            false
        }

        /// Keep channel ownership unrelated to the membership failure under test.
        async fn channel_belongs_to_server(&self, _channel_id: Uuid, _server_id: Uuid) -> bool {
            true
        }

        /// Preserve the unlocked human path when membership remains the tested failure.
        async fn acquire_delivery_lease(
            &self,
            _session: &Session,
            _server_id: Option<Uuid>,
        ) -> Option<super::SessionFenceLease> {
            Some(super::SessionFenceLease::unlocked())
        }
    }

    /// Models a ChannelCreate payload whose identifier belongs to a different server.
    #[async_trait]
    impl SessionFenceAuthorizer for CrossWiredChannelAuthorizer {
        /// Keep session identity current so channel ownership is the only rejection.
        async fn session_is_current(&self, _session: &Session) -> bool {
            true
        }

        /// Permit the initial server subscription.
        async fn subscription_is_current(&self, _session: &Session, _server_id: Uuid) -> bool {
            true
        }

        /// Preserve membership while testing channel parent binding.
        async fn membership_is_current(&self, _session: &Session, _server_id: Uuid) -> bool {
            true
        }

        /// Reject the announced channel as belonging to another server.
        async fn channel_belongs_to_server(&self, _channel_id: Uuid, _server_id: Uuid) -> bool {
            false
        }

        /// Preserve an unlocked lease because canonical channel ownership rejects first.
        async fn acquire_delivery_lease(
            &self,
            _session: &Session,
            _server_id: Option<Uuid>,
        ) -> Option<super::SessionFenceLease> {
            Some(super::SessionFenceLease::unlocked())
        }
    }

    /// Builds a managed agent session for fence rejection tests.
    fn fenced_test_session(server_id: Uuid) -> Session {
        Session {
            user_id: Uuid::new_v4(),
            username: "managed-agent".to_string(),
            is_agent: true,
            managed_fence: Some(RoomFence {
                server_id,
                epoch: 1,
                lease_id: Uuid::new_v4(),
            }),
            subscribed_servers: HashSet::new(),
        }
    }

    /// A room-A capability cannot subscribe to unmanaged room B.
    #[test]
    fn managed_session_scope_is_exact() {
        let room_a = Uuid::new_v4();
        let unmanaged_room_b = Uuid::new_v4();
        let session = fenced_test_session(room_a);

        assert!(session.managed_fence_targets(room_a));
        assert!(!session.managed_fence_targets(unmanaged_room_b));
    }

    /// Human sockets carry no managed room restriction.
    #[test]
    fn human_session_has_no_managed_room_restriction() {
        let session = Session {
            user_id: Uuid::new_v4(),
            username: "human".to_string(),
            is_agent: false,
            managed_fence: None,
            subscribed_servers: HashSet::new(),
        };

        assert!(session.managed_fence_targets(Uuid::new_v4()));
    }

    /// Records text frames without coupling a test to a live WebSocket.
    #[derive(Default)]
    struct RecordingSink {
        frames: Vec<String>,
    }

    /// Implements a permanently ready in-memory WebSocket sink.
    impl Sink<WsMessage> for RecordingSink {
        /// Tests never inject a sink failure through this recorder.
        type Error = std::convert::Infallible;
        /// Report immediate capacity.
        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        /// Persist only text frames emitted by the subscription acknowledgement.
        fn start_send(mut self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
            if let WsMessage::Text(text) = item {
                self.frames.push(text.to_string());
            }
            Ok(())
        }
        /// Flush synchronously stored frames.
        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        /// Close the in-memory test sink.
        fn poll_close(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Suspends socket flush until a test explicitly releases the protected send effect.
    struct BlockingSink {
        /// Signals after the authorization lease is acquired and the frame reaches the sink.
        started: Option<tokio::sync::oneshot::Sender<()>>,
        /// Becomes true when the test permits the simulated socket flush to finish.
        released: Arc<AtomicBool>,
        /// Waker used to resume the flush after the test observes database lock contention.
        flush_waker: Arc<Mutex<Option<Waker>>>,
    }

    /// Implements a controllably backpressured WebSocket sink for lock-lifetime tests.
    impl Sink<WsMessage> for BlockingSink {
        /// The deterministic sink cannot fail.
        type Error = std::convert::Infallible;

        /// Accept one frame immediately so the test can isolate flush behavior.
        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        /// Signal that the protected frame reached the sink while the lease is held.
        fn start_send(mut self: Pin<&mut Self>, _item: WsMessage) -> Result<(), Self::Error> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            Ok(())
        }

        /// Retain backpressure until the test allows the simulated network flush to complete.
        fn poll_flush(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.released.load(Ordering::Acquire) {
                Poll::Ready(Ok(()))
            } else {
                *self.flush_waker.lock().unwrap() = Some(context.waker().clone());
                Poll::Pending
            }
        }

        /// Close only after the same explicit flush release used by the test.
        fn poll_close(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.poll_flush(context)
        }
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

    /// Accept a valid Identify command, then replace its username with database truth.
    #[test]
    fn identify_must_be_first_valid_and_canonicalized() {
        let human_secret = "correct horse battery staple correct horse";
        let agent_secret = "agent authority remains fully independent";
        let user_id = Uuid::new_v4();
        let token = jwt::create_access_token(user_id, "forged-name", human_secret).unwrap();
        let identify = serde_json::json!({"type": "Identify", "data": {"token": token}});

        let claims =
            parse_identify_claims(&identify.to_string(), human_secret, agent_secret).unwrap();
        let session = bind_identified_session(claims, "canonical-name".to_string(), false).unwrap();
        assert_eq!(session.user_id, user_id);
        assert_eq!(session.username, "canonical-name");
        assert!(!session.is_agent);
        assert!(
            parse_identify_claims(
                r#"{"type":"UpdatePresence","data":{"status":"online"}}"#,
                human_secret,
                agent_secret,
            )
            .is_none()
        );
        assert!(parse_identify_claims("not-json", human_secret, agent_secret).is_none());
    }

    /// Agent-key tokens cannot open sockets for canonical human accounts.
    #[test]
    fn agent_token_for_human_account_is_rejected() {
        let human_secret = "human authority remains fully independent";
        let agent_secret = "agent authority remains fully independent";
        let token = sign_test_token(
            Uuid::new_v4(),
            "claimed-human",
            TokenKind::Agent,
            agent_secret,
        );
        let identify = serde_json::json!({"type": "Identify", "data": {"token": token}});
        let claims =
            parse_identify_claims(&identify.to_string(), human_secret, agent_secret).unwrap();
        assert!(bind_identified_session(claims, "human".to_string(), false).is_none());
    }

    /// Human-key tokens cannot open sockets for canonical agent accounts.
    #[test]
    fn human_token_for_agent_account_is_rejected() {
        let human_secret = "human authority remains fully independent";
        let agent_secret = "agent authority remains fully independent";
        let token = jwt::create_access_token(Uuid::new_v4(), "claimed-agent", human_secret)
            .expect("human test token must encode");
        let identify = serde_json::json!({"type": "Identify", "data": {"token": token}});
        let claims =
            parse_identify_claims(&identify.to_string(), human_secret, agent_secret).unwrap();
        assert!(bind_identified_session(claims, "agent".to_string(), true).is_none());
    }

    /// A stale managed agent must not install subscription receivers.
    #[tokio::test]
    async fn stale_managed_agent_subscription_is_rejected_before_receiver_installation() {
        let server_id = Uuid::new_v4();
        let gateway = Gateway::new();
        let mut session = fenced_test_session(server_id);
        let directory = FakeSubscriptionDirectory {
            members: HashSet::from([server_id]),
            channels: HashMap::from([(server_id, vec![Uuid::new_v4()])]),
            membership_calls: Mutex::new(Vec::new()),
            channel_calls: Mutex::new(Vec::new()),
        };
        let (internal_tx, _internal_rx) = mpsc::channel(1);
        let mut sink = RecordingSink::default();

        let outcome = install_subscriptions(
            &gateway,
            &mut session,
            &[server_id],
            &directory,
            Arc::new(RejectingFenceAuthorizer),
            &internal_tx,
            &mut sink,
        )
        .await
        .expect("fence rejection is not a transport error");

        assert_eq!(outcome, SubscriptionInstallOutcome::FenceRejected);
        assert!(session.subscribed_servers.is_empty());
        assert!(directory.membership_calls.lock().unwrap().is_empty());
        assert!(sink.frames.is_empty());
    }

    /// A stale managed session drops buffered broadcast events before queue delivery.
    #[tokio::test]
    async fn stale_managed_agent_event_is_dropped_before_enqueue() {
        let server_id = Uuid::new_v4();
        let (sender, receiver) = broadcast::channel(1);
        let (destination, mut queued) = mpsc::channel(1);
        let channel_id = Uuid::new_v4();
        let forwarder = spawn_scoped_event_forwarder(
            receiver,
            destination,
            fenced_test_session(server_id),
            EventRoute::Channel {
                server_id,
                channel_id,
            },
            Arc::new(RejectingFenceAuthorizer),
        );
        sender
            .send(GatewayEvent::MessageDelete {
                event_id: Uuid::new_v4(),
                id: Uuid::new_v4(),
                channel_id,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), forwarder)
            .await
            .unwrap()
            .unwrap();
        assert!(queued.recv().await.is_none());
    }

    /// A human removed after subscribing must not receive another server event.
    #[tokio::test]
    async fn revoked_human_membership_is_rechecked_before_enqueue() {
        let server_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let session = Session {
            user_id: Uuid::new_v4(),
            username: "former-member".to_string(),
            is_agent: false,
            managed_fence: None,
            subscribed_servers: HashSet::from([server_id]),
        };
        let (sender, receiver) = broadcast::channel(1);
        let (destination, mut queued) = mpsc::channel(1);
        let forwarder = spawn_scoped_event_forwarder(
            receiver,
            destination,
            session.clone(),
            EventRoute::Channel {
                server_id,
                channel_id,
            },
            Arc::new(RevokedHumanMembershipAuthorizer),
        );
        sender
            .send(GatewayEvent::MessageDelete {
                event_id: Uuid::new_v4(),
                id: Uuid::new_v4(),
                channel_id,
            })
            .expect("test receiver must be live");

        tokio::task::yield_now().await;
        assert!(
            queued.try_recv().is_err(),
            "revoked membership must be rejected before queue delivery"
        );
        drop(queued);
        tokio::time::timeout(Duration::from_secs(1), forwarder)
            .await
            .expect("closed destination must stop the forwarder")
            .expect("forwarder must not panic");
    }

    /// A queued event is rechecked after revocation before the socket sink receives it.
    #[tokio::test]
    async fn revoked_human_membership_is_rechecked_before_final_send() {
        let server_id = Uuid::new_v4();
        let session = Session {
            user_id: Uuid::new_v4(),
            username: "former-member".to_string(),
            is_agent: false,
            managed_fence: None,
            subscribed_servers: HashSet::from([server_id]),
        };
        let mut sink = RecordingSink::default();
        let delivered = send_scoped_event_if_current(
            &RevokedHumanMembershipAuthorizer,
            &session,
            ScopedGatewayEvent {
                server_id,
                event: GatewayEvent::PresenceUpdate {
                    user_id: session.user_id,
                    status: "online".to_string(),
                },
            },
            &mut sink,
        )
        .await
        .expect("recording sink is infallible");

        assert!(!delivered);
        assert!(sink.frames.is_empty());
    }

    /// A human delivery lease keeps membership deletion behind the protected effect boundary.
    #[tokio::test]
    async fn live_human_delivery_lease_serializes_membership_removal() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let owner = crate::db::create_user(
            &pool,
            &format!("wslo_{}", &suffix[..12]),
            &format!("wslo-{suffix}@example.invalid"),
            "unusable-test-hash",
            None,
        )
        .await
        .expect("authorization-lock owner must be created");
        let member = crate::db::create_user(
            &pool,
            &format!("wslm_{}", &suffix[..12]),
            &format!("wslm-{suffix}@example.invalid"),
            "unusable-test-hash",
            None,
        )
        .await
        .expect("authorization-lock member must be created");
        let server = crate::db::create_server(
            &pool,
            &format!("WebSocket lease {}", &suffix[..12]),
            None,
            owner.id,
        )
        .await
        .expect("authorization-lock server must be created");
        crate::db::add_member(&pool, server.id, member.id)
            .await
            .expect("authorization-lock membership must be created");
        let server_id = server.id;
        let member_id = member.id;
        let session = Session {
            user_id: member_id,
            username: member.username,
            is_agent: false,
            managed_fence: None,
            subscribed_servers: HashSet::from([server_id]),
        };
        let authorizer = PostgresSessionFenceAuthorizer { pool: pool.clone() };
        let lease = authorizer
            .acquire_delivery_lease(&session, Some(server_id))
            .await
            .expect("current human membership must acquire a delivery lease");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let removal_pool = pool.clone();
        let mut removal = tokio::spawn(async move {
            let _ = started_tx.send(());
            crate::db::remove_member(&removal_pool, server_id, member_id).await
        });
        started_rx
            .await
            .expect("membership removal task must start");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut removal)
                .await
                .is_err(),
            "membership removal must wait while the delivery lease is held"
        );

        assert!(lease.release().await, "delivery lease must release cleanly");
        tokio::time::timeout(Duration::from_secs(2), removal)
            .await
            .expect("membership removal must resume after delivery")
            .expect("membership removal task must not panic")
            .expect("membership removal must succeed");
        assert!(
            !crate::db::is_member(&pool, server_id, session.user_id)
                .await
                .expect("membership post-state must be readable"),
            "membership must be absent after the serialized removal"
        );
    }

    /// A managed Ready send retains the shared room fence until socket flush completes.
    #[tokio::test]
    async fn live_managed_control_send_serializes_fence_advancement() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();
        let owner = crate::db::create_user(
            &pool,
            &format!("wsfo_{}", &suffix[..12]),
            &format!("wsfo-{suffix}@example.invalid"),
            "unusable-test-hash",
            None,
        )
        .await
        .expect("fence-lock owner must be created");
        let agent = crate::db::create_agent_user(
            &pool,
            &format!("wsfa_{}", &suffix[..12]),
            &format!("wsfa-{suffix}@agent.henosis.local"),
            "unusable-test-hash",
            None,
        )
        .await
        .expect("fence-lock agent must be created");
        let server = crate::db::create_server(
            &pool,
            &format!("WebSocket fence {}", &suffix[..12]),
            None,
            owner.id,
        )
        .await
        .expect("fence-lock server must be created");
        crate::db::add_member(&pool, server.id, agent.id)
            .await
            .expect("fence-lock membership must be created");
        sqlx::query(
            r#"INSERT INTO bridge_server_state (server_id, fencing_required)
               VALUES ($1, TRUE)"#,
        )
        .bind(server.id)
        .execute(&pool)
        .await
        .expect("fence-lock room state must be created");
        let fence = crate::db::agent_control::acquire_room_fence(&pool, server.id)
            .await
            .expect("initial managed fence must be issued");
        let session = Session {
            user_id: agent.id,
            username: agent.username,
            is_agent: true,
            managed_fence: Some(fence),
            subscribed_servers: HashSet::from([server.id]),
        };
        let authorizer = PostgresSessionFenceAuthorizer { pool: pool.clone() };
        let (send_started_tx, send_started_rx) = tokio::sync::oneshot::channel();
        let released = Arc::new(AtomicBool::new(false));
        let flush_waker = Arc::new(Mutex::new(None));
        let sink = BlockingSink {
            started: Some(send_started_tx),
            released: released.clone(),
            flush_waker: flush_waker.clone(),
        };
        let user_id = session.user_id;
        let username = session.username.clone();
        let send = tokio::spawn(async move {
            let mut sink = sink;
            send_session_event_if_current(
                &authorizer,
                &session,
                &GatewayEvent::Ready { user_id, username },
                &mut sink,
            )
            .await
        });
        send_started_rx
            .await
            .expect("managed frame must reach the sink while its lease is held");

        let advancement_pool = pool.clone();
        let server_id = server.id;
        let (advance_started_tx, advance_started_rx) = tokio::sync::oneshot::channel();
        let mut advancement = tokio::spawn(async move {
            let _ = advance_started_tx.send(());
            crate::db::agent_control::acquire_room_fence(&advancement_pool, server_id).await
        });
        advance_started_rx
            .await
            .expect("fence advancement task must start");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut advancement)
                .await
                .is_err(),
            "fence advancement must wait while the Ready frame is backpressured"
        );

        released.store(true, Ordering::Release);
        if let Some(waker) = flush_waker.lock().unwrap().take() {
            waker.wake();
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(2), send)
                .await
                .expect("managed Ready send must finish after flush release")
                .expect("managed Ready send task must not panic")
                .expect("blocking sink is infallible"),
            "managed Ready frame must be delivered under the original fence"
        );
        let successor = tokio::time::timeout(Duration::from_secs(2), advancement)
            .await
            .expect("fence advancement must resume after socket flush")
            .expect("fence advancement task must not panic")
            .expect("successor fence must be issued");
        assert_eq!(successor.epoch, fence.epoch + 1);
        assert_ne!(successor.lease_id, fence.lease_id);
    }

    /// A stale managed session cannot receive a Ready or Subscribe control frame.
    #[tokio::test]
    async fn stale_managed_session_control_frame_is_rechecked_before_send() {
        let server_id = Uuid::new_v4();
        let session = fenced_test_session(server_id);
        let mut sink = RecordingSink::default();
        let delivered = send_session_event_if_current(
            &RejectingFenceAuthorizer,
            &session,
            &GatewayEvent::Ready {
                user_id: session.user_id,
                username: session.username.clone(),
            },
            &mut sink,
        )
        .await
        .expect("recording sink is infallible");

        assert!(!delivered);
        assert!(sink.frames.is_empty());
    }

    /// Broadcast entry points reject embedded identifiers that disagree with their route.
    #[test]
    fn broadcast_routing_keys_reject_cross_wired_events() {
        let gateway = Gateway::new();
        let routed_server = Uuid::new_v4();
        let other_server = Uuid::new_v4();
        let routed_channel = Uuid::new_v4();
        let other_channel = Uuid::new_v4();

        assert!(!gateway.broadcast_to_channel(
            routed_channel,
            GatewayEvent::MessageDelete {
                event_id: Uuid::new_v4(),
                id: Uuid::new_v4(),
                channel_id: other_channel,
            },
        ));
        assert!(!gateway.broadcast_to_server(
            routed_server,
            GatewayEvent::ChannelCreate {
                id: Uuid::new_v4(),
                server_id: other_server,
                name: "cross-wired".to_string(),
                channel_type: "text".to_string(),
            },
        ));
        assert!(gateway.broadcast_to_channel(
            routed_channel,
            GatewayEvent::MessageDelete {
                event_id: Uuid::new_v4(),
                id: Uuid::new_v4(),
                channel_id: routed_channel,
            },
        ));
    }

    /// ChannelCreate cannot install a receiver until canonical ownership matches its server route.
    #[tokio::test]
    async fn channel_create_rejects_cross_server_channel_ownership() {
        let server_id = Uuid::new_v4();
        let cross_wired_channel = Uuid::new_v4();
        let gateway = Gateway::new();
        let mut session = Session {
            user_id: Uuid::new_v4(),
            username: "member".to_string(),
            is_agent: false,
            managed_fence: None,
            subscribed_servers: HashSet::new(),
        };
        let directory = FakeSubscriptionDirectory {
            members: HashSet::from([server_id]),
            channels: HashMap::from([(server_id, Vec::new())]),
            membership_calls: Mutex::new(Vec::new()),
            channel_calls: Mutex::new(Vec::new()),
        };
        let (internal_tx, mut internal_rx) = mpsc::channel(2);
        let mut sink = RecordingSink::default();
        install_subscriptions(
            &gateway,
            &mut session,
            &[server_id],
            &directory,
            Arc::new(CrossWiredChannelAuthorizer),
            &internal_tx,
            &mut sink,
        )
        .await
        .expect("recording sink is infallible");

        assert!(gateway.broadcast_to_server(
            server_id,
            GatewayEvent::ChannelCreate {
                id: cross_wired_channel,
                server_id,
                name: "foreign-channel".to_string(),
                channel_type: "text".to_string(),
            },
        ));
        tokio::task::yield_now().await;

        assert!(!gateway.channel_senders.contains_key(&cross_wired_channel));
        assert!(internal_rx.try_recv().is_err());
    }

    /// Membership revocation signals preserve both the affected user and room scope.
    #[tokio::test]
    async fn membership_revocation_is_published_to_active_sockets() {
        let gateway = Gateway::new();
        let server_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let mut receiver = gateway.membership_revocations.subscribe();

        gateway.revoke_membership(server_id, user_id);

        assert_eq!(
            receiver
                .recv()
                .await
                .expect("revocation sender remains live"),
            super::MembershipRevocation { server_id, user_id }
        );
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

    /// Message mutation events expose the durable outbox identity on the wire.
    #[test]
    fn message_delete_serializes_stable_event_id() {
        let event_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let message_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let channel_id = Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();

        assert_eq!(
            serde_json::to_value(GatewayEvent::MessageDelete {
                event_id,
                id: message_id,
                channel_id,
            })
            .unwrap(),
            serde_json::json!({
                "type": "MessageDelete",
                "data": {
                    "event_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    "channel_id": "cccccccc-cccc-cccc-cccc-cccccccccccc"
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
            event_id: Uuid::new_v4(),
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
            event_id: Uuid::new_v4(),
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
            event_id: Uuid::new_v4(),
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
            is_agent: false,
            managed_fence: None,
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
            Arc::new(AllowingFenceAuthorizer),
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
            serde_json::to_value(&queued_event.event).unwrap(),
            serde_json::to_value(&injected_event).unwrap()
        );
        sink.send(WsMessage::Text(
            serde_json::to_string(&queued_event.event).unwrap().into(),
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
            event_id: Uuid::new_v4(),
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
            is_agent: false,
            managed_fence: None,
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
            Arc::new(AllowingFenceAuthorizer),
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
