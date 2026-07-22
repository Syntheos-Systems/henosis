//! Authenticated WebSocket sessions and scoped Rift event fan-out.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

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
    Ready {
        user_id: Uuid,
        username: String,
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
                && sender.receiver_count() == 0 {
                    drop(sender);
                    self.user_senders.remove(&user_id);
                }
        }
    }

    /// Handle a new WebSocket connection
    pub async fn handle_connection(
        &self,
        socket: WebSocket,
        jwt_secret: String,
        pool: PgPool,
    ) {
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
            .send(WsMessage::Text(serde_json::to_string(&ready).unwrap().into()))
            .await;

        // Subscribe to user-specific events
        let mut user_rx = gateway.subscribe_user(session.user_id);

        let user_id = session.user_id;
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::channel::<GatewayEvent>(256);

        // Task: forward user events to internal channel
        let internal_tx_clone = internal_tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = user_rx.recv().await {
                if internal_tx_clone.send(event).await.is_err() {
                    break;
                }
            }
        });

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
                                        if !subscriptions_fit(&session.subscribed_servers, &server_ids) {
                                            tracing::warn!(
                                                user_id = %session.user_id,
                                                requested = server_ids.len(),
                                                maximum = MAX_SUBSCRIPTION_BATCH,
                                                "refusing oversized server subscription batch"
                                            );
                                            continue;
                                        }
                                        // Subscribe to server-wide events (member join/leave, channel create/delete)
                                        for server_id in server_ids {
                                            if session.subscribed_servers.contains(&server_id) {
                                                continue;
                                            }
                                            // A refused subscription must never be silent. When
                                            // this dropped through quietly, a non-member bridge
                                            // agent looked completely healthy while the room was
                                            // deaf to every message.
                                            match crate::db::is_member(&pool, server_id, session.user_id).await {
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
                                                Err(e) => {
                                                    tracing::warn!(
                                                        server_id = %server_id,
                                                        user_id = %session.user_id,
                                                        error = %e,
                                                        "refusing server subscription: membership check failed"
                                                    );
                                                    continue;
                                                }
                                            }

                                            let mut rx = gateway.subscribe_server(server_id);
                                            let tx = internal_tx.clone();
                                            let gw = gateway.clone();
                                            let tx2 = internal_tx.clone();
                                            tokio::spawn(async move {
                                                while let Ok(event) = rx.recv().await {
                                                    // Auto-subscribe to newly created channels
                                                    if let GatewayEvent::ChannelCreate { id: channel_id, .. } = &event {
                                                        let mut chan_rx = gw.subscribe_channel(*channel_id);
                                                        let chan_tx = tx2.clone();
                                                        tokio::spawn(async move {
                                                            while let Ok(ev) = chan_rx.recv().await {
                                                                if chan_tx.send(ev).await.is_err() {
                                                                    break;
                                                                }
                                                            }
                                                        });
                                                    }
                                                    if tx.send(event).await.is_err() {
                                                        break;
                                                    }
                                                }
                                            });

                                            // Subscribe to all existing channels in this server
                                            if let Ok(channels) = crate::db::get_server_channels(&pool, server_id).await {
                                                for channel in channels {
                                                    let mut chan_rx = gateway.subscribe_channel(channel.id);
                                                    let chan_tx = internal_tx.clone();
                                                    tokio::spawn(async move {
                                                        while let Ok(event) = chan_rx.recv().await {
                                                            if chan_tx.send(event).await.is_err() {
                                                                break;
                                                            }
                                                        }
                                                    });
                                                }
                                            }

                                            session.subscribed_servers.insert(server_id);
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

        // Cleanup: mark offline
        gateway.mark_connection_closed(user_id);
    }

    /// Subscribe a connection to receive events for a specific channel
    pub fn subscribe_connection_to_channel(&self, channel_id: Uuid, tx: tokio::sync::mpsc::Sender<GatewayEvent>) {
        let mut rx = self.subscribe_channel(channel_id);
        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
    }
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

#[cfg(test)]
/// Exercises WebSocket command bounds that do not require a live database.
mod tests {
    use super::{MAX_SUBSCRIPTION_BATCH, parse_identify, subscriptions_fit};
    use crate::auth::jwt;
    use std::collections::HashSet;
    use uuid::Uuid;

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
}
