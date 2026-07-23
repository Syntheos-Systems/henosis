//! HTTP + WS client for Rift server API.

use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use uuid::Uuid;

use crate::auth::AgentAuthManager;
use crate::error::BridgeError;
use crate::types::RoomMessage;

/// HTTP client for the Rift server REST API.
pub struct RiftRestClient {
    /// Underlying HTTP client.
    client: Client,
    /// Base URL of the Rift server (e.g., http://localhost:3200).
    base_url: String,
    /// Auth manager for issuing agent JWTs.
    auth: AgentAuthManager,
}

/// Response from user registration.
#[derive(Debug, Deserialize)]
pub struct UserResponse {
    /// Rift user ID.
    pub id: Uuid,
    /// Username.
    pub username: String,
    /// Whether the user is an agent.
    pub is_agent: bool,
}

/// Response from the bridge provisioning endpoint.
#[derive(Debug, Deserialize)]
struct ProvisionResponse {
    /// Provisioned agents, in the order they were requested.
    agents: Vec<UserResponse>,
}

/// Response from sending a message.
#[derive(Debug, Deserialize)]
pub struct MessageResponse {
    /// Message ID.
    pub id: Uuid,
    /// Channel the message was posted in.
    pub channel_id: Uuid,
    /// Author's user ID.
    pub author_id: Uuid,
    /// Message text content.
    pub content: String,
    /// Message type discriminator.
    pub message_type: Option<String>,
}

/// Single message from the list_messages response.
#[derive(Debug, Deserialize)]
pub struct ListMessageResponse {
    /// Message ID.
    pub id: Uuid,
    /// Channel ID.
    pub channel_id: Uuid,
    /// Author's user ID.
    pub author_id: Uuid,
    /// Author's username (may be absent in older data).
    #[serde(default)]
    pub author_username: Option<String>,
    /// Message text content.
    pub content: String,
    /// Message type discriminator.
    pub message_type: Option<String>,
    /// ISO timestamp of message creation.
    pub created_at: String,
}

/// Bridge status response from the pause endpoint.
#[derive(Debug, Deserialize)]
pub struct BridgeStatus {
    /// Whether the bridge is paused.
    pub paused: bool,
}

/// Implements REST operations used by the bridge daemon.
impl RiftRestClient {
    /// Create a new REST client for the given base URL.
    pub fn new(base_url: String, auth: AgentAuthManager) -> Self {
        Self {
            client: Client::new(),
            base_url,
            auth,
        }
    }

    /// Provision the whole agent roster and join every agent to the server.
    ///
    /// Replaces per-agent `/api/auth/register` calls, which created the users
    /// but left them out of the server's member list. The gateway refuses a
    /// non-member's channel subscription, so agents provisioned that way could
    /// post but never hear anything -- a silent, total failure of the room.
    /// Idempotent, so it runs on every boot.
    pub async fn provision_agents(
        &self,
        server_id: Uuid,
        agents: &[(String, String)],
    ) -> Result<Vec<UserResponse>, BridgeError> {
        let url = format!("{}/api/bridge/provision", self.base_url);
        let payload: Vec<serde_json::Value> = agents
            .iter()
            .map(|(username, display_name)| {
                serde_json::json!({
                    "username": username,
                    "display_name": display_name,
                })
            })
            .collect();

        let resp = self
            .client
            .post(&url)
            .bearer_auth(self.auth.bridge_secret())
            .json(&serde_json::json!({
                "server_id": server_id,
                "agents": payload,
            }))
            .send()
            .await?;

        if resp.status().is_success() {
            let body: ProvisionResponse = resp.json().await?;
            Ok(body.agents)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(BridgeError::RiftApi(format!(
                "provision failed ({status}): {body}"
            )))
        }
    }

    /// Send a message to a channel as an agent.
    ///
    /// `message_type` of `Some("stimulus")` or `Some("system")` asks the
    /// server to stamp that structural type; `None` lets the server infer
    /// from the author (agents land as 'agent'). Older servers without the
    /// field simply ignore it.
    pub async fn send_message(
        &self,
        agent_user_id: Uuid,
        agent_username: &str,
        channel_id: Uuid,
        content: &str,
        message_type: Option<&str>,
    ) -> Result<MessageResponse, BridgeError> {
        let token = self.auth.issue_token(agent_user_id, agent_username)?;
        let url = format!("{}/api/channels/{}/messages", self.base_url, channel_id);

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .json(&message_payload(content, message_type))
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(resp.json().await?)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(BridgeError::RiftApi(format!(
                "send_message failed ({status}): {body}"
            )))
        }
    }

    /// Fetch recent messages from a channel.
    pub async fn list_messages(
        &self,
        agent_user_id: Uuid,
        agent_username: &str,
        channel_id: Uuid,
        limit: u32,
    ) -> Result<Vec<ListMessageResponse>, BridgeError> {
        let token = self.auth.issue_token(agent_user_id, agent_username)?;
        let url = format!(
            "{}/api/channels/{}/messages?limit={}",
            self.base_url, channel_id, limit
        );

        let resp = self.client.get(&url).bearer_auth(&token).send().await?;

        if resp.status().is_success() {
            Ok(resp.json().await?)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(BridgeError::RiftApi(format!(
                "list_messages failed ({status}): {body}"
            )))
        }
    }

    /// Check whether the bridge for one server is paused.
    pub async fn is_paused(&self, server_id: Uuid) -> Result<bool, BridgeError> {
        let url = bridge_status_url(&self.base_url, server_id);
        let resp = self.client.get(&url).send().await?;

        if resp.status().is_success() {
            let status: BridgeStatus = resp.json().await?;
            Ok(status.paused)
        } else {
            Ok(false)
        }
    }
}

/// Build the server-scoped bridge status endpoint.
fn bridge_status_url(base_url: &str, server_id: Uuid) -> String {
    format!("{base_url}/api/servers/{server_id}/bridge/status")
}

/// Build the JSON body for a message post.
///
/// The structural type is included only when explicitly requested: an absent
/// field keeps the wire format identical to pre-message_type bridges, so a
/// server of either vintage sees exactly what it expects.
fn message_payload(content: &str, message_type: Option<&str>) -> serde_json::Value {
    match message_type {
        Some(t) => serde_json::json!({ "content": content, "message_type": t }),
        None => serde_json::json!({ "content": content }),
    }
}

/// Events received from the Rift WebSocket gateway.
#[derive(Debug, Clone)]
pub enum RiftWsEvent {
    /// Gateway authenticated and ready.
    Ready,
    /// New message posted in a subscribed channel.
    MessageCreate(RoomMessage),
    /// WebSocket connection lost.
    Disconnected,
}

/// Connect to Rift's WebSocket gateway and forward events to the channel.
/// Reconnects automatically on disconnect.
pub async fn ws_listen(
    ws_url: String,
    token: String,
    server_ids: Vec<Uuid>,
    event_tx: mpsc::Sender<RiftWsEvent>,
) {
    loop {
        tracing::info!("connecting to Rift WebSocket at {}", ws_url);
        match connect_and_listen(&ws_url, &token, &server_ids, &event_tx).await {
            Ok(()) => tracing::info!("WebSocket connection closed cleanly"),
            Err(e) => tracing::error!("WebSocket error: {e}"),
        }
        let _ = event_tx.send(RiftWsEvent::Disconnected).await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Single WebSocket connection lifecycle: identify, subscribe, then forward events.
async fn connect_and_listen(
    ws_url: &str,
    token: &str,
    server_ids: &[Uuid],
    event_tx: &mpsc::Sender<RiftWsEvent>,
) -> Result<(), BridgeError> {
    let (mut ws, _) = connect_async(ws_url)
        .await
        .map_err(|e| BridgeError::WebSocket(format!("connect failed: {e}")))?;

    // Send Identify command with auth token.
    let identify = serde_json::json!({
        "type": "Identify",
        "data": { "token": token }
    });
    ws.send(WsMessage::Text(identify.to_string().into()))
        .await
        .map_err(|e| BridgeError::WebSocket(format!("identify failed: {e}")))?;

    // Wait for Ready event.
    let ready_msg = ws
        .next()
        .await
        .ok_or_else(|| BridgeError::WebSocket("connection closed before Ready".into()))?
        .map_err(|e| BridgeError::WebSocket(format!("read error: {e}")))?;

    if let WsMessage::Text(ref text) = ready_msg {
        let val: serde_json::Value = serde_json::from_str(text.as_str())?;
        if val["type"].as_str() != Some("Ready") {
            return Err(BridgeError::WebSocket(format!(
                "expected Ready, got: {}",
                text.as_str()
            )));
        }
    }
    let _ = event_tx.send(RiftWsEvent::Ready).await;

    // Subscribe to server channels.
    let subscribe = serde_json::json!({
        "type": "Subscribe",
        "data": { "server_ids": server_ids }
    });
    ws.send(WsMessage::Text(subscribe.to_string().into()))
        .await
        .map_err(|e| BridgeError::WebSocket(format!("subscribe failed: {e}")))?;

    tracing::info!("WebSocket connected and subscribed");

    // Event forwarding loop.
    while let Some(msg) = ws.next().await {
        let msg = msg.map_err(|e| BridgeError::WebSocket(format!("read error: {e}")))?;

        if let WsMessage::Text(ref text) = msg {
            let val: serde_json::Value = match serde_json::from_str(text.as_str()) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if val["type"].as_str() == Some("MessageCreate") {
                if let Some(data) = val.get("data") {
                    // A parse failure here means the room goes deaf to that
                    // message; it must never be silent (live smoke test
                    // finding: a missing field cost hours of "why is the
                    // room ignoring everyone").
                    match serde_json::from_value::<RoomMessage>(data.clone()) {
                        Ok(room_msg) => {
                            let _ = event_tx.send(RiftWsEvent::MessageCreate(room_msg)).await;
                        }
                        Err(e) => {
                            tracing::warn!("dropping unparseable MessageCreate event: {e}");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Covers the message post payload shape.
#[cfg(test)]
mod tests {
    use super::{bridge_status_url, message_payload};
    use uuid::Uuid;

    /// The pause poll is bound to the configured server rather than global state.
    #[test]
    fn test_bridge_status_url_is_server_scoped() {
        let server_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        assert_eq!(
            bridge_status_url("https://rift.example", server_id),
            "https://rift.example/api/servers/11111111-1111-1111-1111-111111111111/bridge/status"
        );
    }

    /// A typed post carries the discriminator for the server to stamp.
    #[test]
    fn test_payload_includes_requested_type() {
        let body = message_payload("hello", Some("stimulus"));
        assert_eq!(body["content"], "hello");
        assert_eq!(body["message_type"], "stimulus");
    }

    /// An untyped post omits the field entirely -- the server infers the
    /// type, and older servers see the pre-message_type wire format.
    #[test]
    fn test_payload_omits_absent_type() {
        let body = message_payload("hello", None);
        assert_eq!(body["content"], "hello");
        assert!(body.get("message_type").is_none());
    }
}
