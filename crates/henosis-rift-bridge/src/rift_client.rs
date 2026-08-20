//! HTTP + WS client for Rift server API.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use uuid::Uuid;

use crate::auth::AgentAuthManager;
use crate::config::{validate_bridge_api_url, validate_distinct_rift_origins};
use crate::error::BridgeError;
use crate::types::RoomMessage;

/// Header carrying the positive managed-room generation number.
const FENCE_EPOCH_HEADER: &str = "x-henosis-fence-epoch";

/// Header carrying the opaque managed-room generation lease.
const FENCE_LEASE_HEADER: &str = "x-henosis-fence-lease";

/// Header carrying the managed room authorized by the generation capability.
const FENCE_SERVER_HEADER: &str = "x-henosis-fence-server";

/// Maximum wall-clock duration for any Rift REST authority operation.
const RIFT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Build a proxy-free, redirect-free client with a bounded total request lifetime.
fn build_http_client(timeout: Duration) -> Result<Client, reqwest::Error> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
}

/// HTTP client for the Rift server REST API.
pub struct RiftRestClient {
    /// Proxy-free, no-redirect client for agent-JWT operations.
    public_client: Client,
    /// Proxy-free, no-redirect client for bridge-secret operations.
    bridge_client: Client,
    /// Public Rift base URL used only with agent JWTs.
    public_api_url: String,
    /// Private Rift base URL used only with the bridge secret.
    bridge_api_url: String,
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

/// Bridge status response from the daemon-only pause endpoint.
#[derive(Debug, Deserialize)]
pub struct BridgeStatus {
    /// Whether the bridge is paused.
    pub paused: bool,
}

/// Implements REST operations used by the bridge daemon.
impl RiftRestClient {
    /// Create a REST client with distinct public and bridge-only origins.
    pub fn new(
        public_api_url: String,
        bridge_api_url: String,
        auth: AgentAuthManager,
    ) -> Result<Self, BridgeError> {
        validate_bridge_api_url(&bridge_api_url)?;
        validate_distinct_rift_origins(&public_api_url, &bridge_api_url)?;
        let public_client = build_http_client(RIFT_REQUEST_TIMEOUT)?;
        let bridge_client = build_http_client(RIFT_REQUEST_TIMEOUT)?;
        Ok(Self {
            public_client,
            bridge_client,
            public_api_url: normalized_api_url(public_api_url),
            bridge_api_url: normalized_api_url(bridge_api_url),
            auth,
        })
    }

    /// Bind one private request to the current managed-room generation when present.
    fn fenced_bridge_request(
        &self,
        request: RequestBuilder,
        server_id: Uuid,
    ) -> Result<RequestBuilder, BridgeError> {
        let Some(fence) = self.auth.managed_fence() else {
            return Ok(request);
        };
        if fence.server_id != server_id {
            return Err(BridgeError::Auth(
                "managed room fence does not authorize the requested server".to_string(),
            ));
        }
        Ok(request
            .header(FENCE_SERVER_HEADER, fence.server_id.to_string())
            .header(FENCE_EPOCH_HEADER, fence.epoch.to_string())
            .header(FENCE_LEASE_HEADER, fence.lease_id.to_string()))
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
        let url = format!("{}/api/bridge/provision", self.bridge_api_url);
        let payload: Vec<serde_json::Value> = agents
            .iter()
            .map(|(username, display_name)| {
                serde_json::json!({
                    "username": username,
                    "display_name": display_name,
                })
            })
            .collect();

        let request = self
            .bridge_client
            .post(&url)
            .bearer_auth(self.auth.bridge_secret())
            .json(&serde_json::json!({
                "server_id": server_id,
                "agents": payload,
            }));
        let resp = self
            .fenced_bridge_request(request, server_id)?
            .send()
            .await?;

        if resp.status().is_success() {
            let body: ProvisionResponse = resp.json().await?;
            Ok(body.agents)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(classify_rift_api_error(status, &body, "provision"))
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
        let url = format!(
            "{}/api/channels/{}/messages",
            self.public_api_url, channel_id
        );

        let resp = self
            .public_client
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
            Err(classify_rift_api_error(status, &body, "send_message"))
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
            self.public_api_url, channel_id, limit
        );

        let resp = self
            .public_client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(resp.json().await?)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(classify_rift_api_error(status, &body, "list_messages"))
        }
    }

    /// Check whether the bridge for one server is paused.
    ///
    /// Presents the bridge secret: the status route is control-plane state, not
    /// public. Failures propagate rather than reporting "not paused", so the
    /// poller keeps the last known state instead of silently resuming a room the
    /// operator paused.
    pub async fn is_paused(&self, server_id: Uuid) -> Result<bool, BridgeError> {
        let url = bridge_status_url(&self.bridge_api_url, server_id);
        let request = self
            .bridge_client
            .get(&url)
            .bearer_auth(self.auth.bridge_secret());
        let resp = self
            .fenced_bridge_request(request, server_id)?
            .send()
            .await?;

        if resp.status().is_success() {
            let status: BridgeStatus = resp.json().await?;
            Ok(status.paused)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(classify_rift_api_error(
                status,
                &body,
                "bridge status check",
            ))
        }
    }
}

/// Remove trailing separators before endpoint paths are appended.
fn normalized_api_url(url: String) -> String {
    url.trim_end_matches('/').to_string()
}

/// Convert stable leadership-fence API codes into a fatal managed-runtime error.
fn classify_rift_api_error(
    status: reqwest::StatusCode,
    body: &str,
    operation: &str,
) -> BridgeError {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("code")?.as_str().map(str::to_owned));
    if matches!(
        code.as_deref(),
        Some("stale_leadership_fence" | "leadership_fence_unavailable")
    ) {
        BridgeError::StaleLeadership
    } else {
        BridgeError::RiftApi(format!("{operation} failed ({status}): {body}"))
    }
}

/// Build the server-scoped bridge status endpoint.
fn bridge_status_url(base_url: &str, server_id: Uuid) -> String {
    format!("{base_url}/api/bridge/servers/{server_id}/status")
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
    MessageCreate(RiftMessageEvent),
    /// WebSocket connection lost.
    Disconnected,
}

/// One durable message event with its stable outbox identity intact.
#[derive(Debug, Clone, Deserialize)]
pub struct RiftMessageEvent {
    /// Stable identity reused by every delivery attempt for this event.
    pub event_id: Uuid,
    /// Message payload consumed by the room state machine.
    #[serde(flatten)]
    pub message: RoomMessage,
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
    // Subscribe to server channels.
    let subscribe = serde_json::json!({
        "type": "Subscribe",
        "data": { "server_ids": server_ids }
    });
    ws.send(WsMessage::Text(subscribe.to_string().into()))
        .await
        .map_err(|e| BridgeError::WebSocket(format!("subscribe failed: {e}")))?;

    // A managed supervisor may replace the old bridge as soon as it observes
    // this event, so readiness must follow the successful subscription send.
    let _ = event_tx.send(RiftWsEvent::Ready).await;

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
                    match serde_json::from_value::<RiftMessageEvent>(data.clone()) {
                        Ok(message_event) => {
                            let _ = event_tx
                                .send(RiftWsEvent::MessageCreate(message_event))
                                .await;
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
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{Request, StatusCode};
    use axum::response::Redirect;
    use axum::routing::any;
    use axum::{Json, Router};
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;

    /// The stable outbox event identity must survive bridge-side payload parsing.
    #[test]
    fn message_create_payload_preserves_stable_event_id() {
        let event_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let author_id = Uuid::new_v4();
        let payload = serde_json::json!({
            "event_id": event_id,
            "id": message_id,
            "channel_id": channel_id,
            "author_id": author_id,
            "author_username": "human",
            "content": "run this once",
            "message_type": "user",
            "created_at": "2026-08-17T12:00:00Z"
        });

        let parsed = serde_json::from_value::<super::RiftMessageEvent>(payload)
            .expect("parse MessageCreate payload");
        assert_eq!(parsed.event_id, event_id);
        assert_eq!(parsed.message.id, message_id);
    }

    use super::{
        bridge_status_url, build_http_client, classify_rift_api_error, message_payload,
        RiftRestClient,
    };
    use crate::auth::AgentAuthManager;
    use crate::error::BridgeError;
    use henosis_rift_server::models::leadership::RoomFence;
    use uuid::Uuid;

    /// One request observed at a recorder trust boundary.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        /// HTTP method sent by the Rift client.
        method: String,
        /// Absolute request path without the recorder origin.
        path: String,
        /// Authorization header presented to the endpoint.
        authorization: String,
        /// Optional managed-room server header presented to the endpoint.
        fence_server: Option<String>,
        /// Optional managed-room epoch header presented to the endpoint.
        fence_epoch: Option<String>,
        /// Optional managed-room opaque lease header presented to the endpoint.
        fence_lease: Option<String>,
    }

    /// Concurrent request log shared between one recorder and its test.
    type RecordedRequests = Arc<Mutex<Vec<RecordedRequest>>>;

    /// Record any request and return the minimal response for the exercised client operation.
    async fn record_request(
        State(requests): State<RecordedRequests>,
        request: Request<Body>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let method = request.method().to_string();
        let path = request.uri().path().to_string();
        let authorization = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let fence_server = request
            .headers()
            .get("x-henosis-fence-server")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let fence_epoch = request
            .headers()
            .get("x-henosis-fence-epoch")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let fence_lease = request
            .headers()
            .get("x-henosis-fence-lease")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        requests.lock().await.push(RecordedRequest {
            method: method.clone(),
            path: path.clone(),
            authorization,
            fence_server,
            fence_epoch,
            fence_lease,
        });

        let body = match (method.as_str(), path.as_str()) {
            ("POST", "/api/bridge/provision") => serde_json::json!({
                "agents": [{
                    "id": Uuid::new_v4(),
                    "username": "agent-bridge",
                    "is_agent": true,
                }],
            }),
            ("GET", path)
                if path.starts_with("/api/bridge/servers/") && path.ends_with("/status") =>
            {
                serde_json::json!({ "paused": false })
            }
            ("POST", path) if path.starts_with("/api/channels/") => serde_json::json!({
                "id": Uuid::new_v4(),
                "channel_id": Uuid::new_v4(),
                "author_id": Uuid::new_v4(),
                "content": "hello",
                "message_type": "agent",
            }),
            ("GET", path) if path.starts_with("/api/channels/") => serde_json::json!([]),
            _ => return (StatusCode::NOT_FOUND, Json(serde_json::json!({}))),
        };
        (StatusCode::OK, Json(body))
    }

    /// Bind one ephemeral recorder and return its URL, log, and server task.
    async fn spawn_recorder() -> (String, RecordedRequests, JoinHandle<()>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .fallback(any(record_request))
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind recorder");
        let address = listener.local_addr().expect("recorder address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve recorder");
        });
        (format!("http://{address}"), requests, task)
    }

    /// Redirect every request to a caller-selected origin.
    async fn redirect_request(State(target): State<String>) -> Redirect {
        Redirect::temporary(&target)
    }

    /// Bind one ephemeral redirector that attempts to move a private request elsewhere.
    async fn spawn_redirector(target: String) -> (String, JoinHandle<()>) {
        let app = Router::new()
            .fallback(any(redirect_request))
            .with_state(target);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind redirector");
        let address = listener.local_addr().expect("redirector address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve redirector");
        });
        (format!("http://{address}"), task)
    }

    /// A peer that accepts but never answers cannot hold an authority request forever.
    #[tokio::test]
    async fn http_client_times_out_a_stalled_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silent peer");
        let address = listener.local_addr().expect("silent peer address");
        let peer = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.expect("accept stalled request");
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let client = build_http_client(Duration::from_millis(50)).expect("bounded client");

        let error = client
            .get(format!("http://{address}/stalled"))
            .send()
            .await
            .expect_err("stalled peer must time out");

        assert!(error.is_timeout());
        peer.abort();
    }

    /// Bridge credentials stay on the private origin while agent JWTs stay on public Rift.
    #[tokio::test]
    async fn rest_client_routes_credentials_to_separate_origins() {
        let (public_url, public_requests, public_task) = spawn_recorder().await;
        let (bridge_url, bridge_requests, bridge_task) = spawn_recorder().await;
        let bridge_secret = "b".repeat(32);
        let fence = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 9,
            lease_id: Uuid::new_v4(),
        };
        let auth = AgentAuthManager::new("j".repeat(32), bridge_secret.clone())
            .with_managed_fence(Some(fence));
        let client = RiftRestClient::new(public_url, bridge_url, auth).expect("Rift client");
        let server_id = fence.server_id;
        let channel_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();

        client
            .provision_agents(
                server_id,
                &[("agent-bridge".to_string(), "Bridge Agent".to_string())],
            )
            .await
            .expect("private provisioning request");
        assert!(!client
            .is_paused(server_id)
            .await
            .expect("private status request"));
        client
            .send_message(agent_id, "agent-bridge", channel_id, "hello", None)
            .await
            .expect("public message request");
        client
            .list_messages(agent_id, "agent-bridge", channel_id, 10)
            .await
            .expect("public list request");

        let public = public_requests.lock().await.clone();
        let private = bridge_requests.lock().await.clone();
        public_task.abort();
        bridge_task.abort();

        assert_eq!(public.len(), 2);
        assert!(public
            .iter()
            .all(|request| request.path.starts_with("/api/channels/")));
        assert!(public.iter().all(|request| {
            request.authorization.starts_with("Bearer ")
                && request.authorization != format!("Bearer {bridge_secret}")
        }));
        assert_eq!(private.len(), 2);
        assert!(private
            .iter()
            .all(|request| request.path.starts_with("/api/bridge/")));
        assert!(private
            .iter()
            .all(|request| request.authorization == format!("Bearer {bridge_secret}")));
        assert!(private.iter().all(|request| {
            request.fence_server.as_deref() == Some(fence.server_id.to_string().as_str())
        }));
        assert!(private
            .iter()
            .all(|request| request.fence_epoch.as_deref() == Some("9")));
        assert!(private.iter().all(|request| {
            request.fence_lease.as_deref() == Some(fence.lease_id.to_string().as_str())
        }));
        assert!(public.iter().all(|request| request.fence_server.is_none()
            && request.fence_epoch.is_none()
            && request.fence_lease.is_none()));
    }

    /// Stable leadership-fence responses terminate a stale managed bridge.
    #[test]
    fn stale_fence_api_errors_are_classified_as_fatal() {
        let error = classify_rift_api_error(
            reqwest::StatusCode::CONFLICT,
            r#"{"code":"stale_leadership_fence","error":"stale"}"#,
            "message send",
        );
        assert!(matches!(error, BridgeError::StaleLeadership));

        let unavailable = classify_rift_api_error(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            r#"{"code":"leadership_fence_unavailable","error":"unavailable"}"#,
            "status check",
        );
        assert!(matches!(unavailable, BridgeError::StaleLeadership));
    }

    /// Private bridge requests never follow a redirect onto the public origin.
    #[tokio::test]
    async fn bridge_client_refuses_cross_origin_redirects() {
        let (public_url, public_requests, public_task) = spawn_recorder().await;
        let redirect_target = format!("{public_url}/api/bridge/redirected");
        let (bridge_url, bridge_task) = spawn_redirector(redirect_target).await;
        let auth = AgentAuthManager::new("j".repeat(32), "b".repeat(32));
        let client = RiftRestClient::new(public_url, bridge_url, auth).expect("Rift client");

        assert!(client.is_paused(Uuid::new_v4()).await.is_err());
        assert!(public_requests.lock().await.is_empty());

        public_task.abort();
        bridge_task.abort();
    }

    /// Direct client construction cannot bypass the private loopback URL gate.
    #[test]
    fn rest_client_rejects_unsafe_or_reused_bridge_origins() {
        let remote = RiftRestClient::new(
            "http://127.0.0.1:3200".to_string(),
            "http://192.0.2.10:3201".to_string(),
            AgentAuthManager::new("j".repeat(32), "b".repeat(32)),
        );
        assert!(remote.is_err());

        let reused = RiftRestClient::new(
            "http://127.0.0.1:3200/".to_string(),
            "http://127.0.0.1:3200".to_string(),
            AgentAuthManager::new("j".repeat(32), "b".repeat(32)),
        );
        assert!(reused.is_err());
    }

    /// Public agent requests never follow a redirect onto the bridge origin.
    #[tokio::test]
    async fn public_client_refuses_cross_origin_redirects() {
        let (bridge_url, bridge_requests, bridge_task) = spawn_recorder().await;
        let redirect_target = format!("{bridge_url}/api/channels/redirected/messages");
        let (public_url, public_task) = spawn_redirector(redirect_target).await;
        let auth = AgentAuthManager::new("j".repeat(32), "b".repeat(32));
        let client = RiftRestClient::new(public_url, bridge_url, auth).expect("Rift client");

        assert!(client
            .send_message(
                Uuid::new_v4(),
                "agent-bridge",
                Uuid::new_v4(),
                "hello",
                None,
            )
            .await
            .is_err());
        assert!(bridge_requests.lock().await.is_empty());

        public_task.abort();
        bridge_task.abort();
    }

    /// Process proxy settings cannot receive an agent JWT from the public client.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn public_client_ignores_proxy_environment() {
        if std::env::var_os("SYNTHEOS_RIFT_PROXY_CHILD").is_some() {
            let bridge_url =
                std::env::var("SYNTHEOS_RIFT_PROXY_URL").expect("child process receives proxy URL");
            let client = RiftRestClient::new(
                "http://rift-public.invalid:3200".to_string(),
                bridge_url,
                AgentAuthManager::new("j".repeat(32), "b".repeat(32)),
            )
            .expect("Rift client");
            let result = client
                .send_message(
                    Uuid::new_v4(),
                    "agent-bridge",
                    Uuid::new_v4(),
                    "hello",
                    None,
                )
                .await;
            assert!(result.is_err(), "public request used the process proxy");
            return;
        }

        let (proxy_url, proxy_requests, proxy_task) = spawn_recorder().await;
        let executable = std::env::current_exe().expect("current test executable");
        let child_proxy_url = proxy_url.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(executable)
                .env_clear()
                .env("SYNTHEOS_RIFT_PROXY_CHILD", "1")
                .env("SYNTHEOS_RIFT_PROXY_URL", &child_proxy_url)
                .env("HTTP_PROXY", &child_proxy_url)
                .env("HTTPS_PROXY", &child_proxy_url)
                .env("ALL_PROXY", &child_proxy_url)
                .env("http_proxy", &child_proxy_url)
                .env("https_proxy", &child_proxy_url)
                .env("all_proxy", &child_proxy_url)
                .arg("--exact")
                .arg("rift_client::tests::public_client_ignores_proxy_environment")
                .arg("--nocapture")
                .output()
                .expect("run isolated proxy child")
        })
        .await
        .expect("join isolated proxy child");
        let proxy_was_unused = proxy_requests.lock().await.is_empty();
        proxy_task.abort();

        assert!(
            output.status.success(),
            "isolated proxy child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(proxy_was_unused, "proxy observed an agent JWT request");
    }

    /// The pause poll is bound to the configured server rather than global state.
    #[test]
    fn test_bridge_status_url_is_server_scoped() {
        let server_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        assert_eq!(
            bridge_status_url("https://rift.example", server_id),
            "https://rift.example/api/bridge/servers/11111111-1111-1111-1111-111111111111/status"
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
