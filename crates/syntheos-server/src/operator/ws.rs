//! `GET /ws` -- WebSocket event hub over the in-process AxonBus.
//!
//! ## Channel wiring
//!
//! The hub subscribes to three channels at connection time (all channels the
//! kernel currently publishes on). Events on other channels are not forwarded;
//! as new emitters land, add their channels to `SUBSCRIBED_CHANNELS` here.
//!
//! | Channel      | Producer            | Event kinds emitted                               |
//! |-------------|---------------------|--------------------------------------------------|
//! | `narration` | `henosis-broca`     | `narration.action_logged`                         |
//! | `agent`     | `henosis-soma`      | `agent.registered`, `agent.deregistered`,         |
//! |             |                     | `agent.heartbeat`, `agent.status_changed`,        |
//! |             |                     | `agent.quality_updated`                           |
//! | `workflow`  | `henosis-loom`      | `workflow.run_created`, `workflow.run_completed`,  |
//! |             |                     | `workflow.run_failed`, `workflow.run_cancelled`,   |
//! |             |                     | `workflow.step_started`, `workflow.step_completed`,|
//! |             |                     | `workflow.step_failed`                            |
//!
//! ## Org isolation
//!
//! Every envelope carries a `tenant` field. [`envelope_to_event`] drops any
//! envelope whose `tenant != org` (the org from the connection's JWT). Events
//! from other tenants are never visible to connected operators.
//!
//! ## Auth
//!
//! The handshake must offer `henosis.v1` and `henosis.auth.<jwt>` through
//! `Sec-WebSocket-Protocol`. Only the fixed public protocol is echoed. A missing
//! or invalid credential returns HTTP 401, and the session closes at token expiry.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use henosis_plutus::{can, Permission, PolicyBackend};
use serde_json::json;
use syntheos_axon::AxonBus;
use syntheos_contracts::{AxonEnvelope, PrincipalId, TenantId};
use tokio::sync::broadcast::error::RecvError;
use zeroize::Zeroizing;

use super::auth;
use super::rbac::resolve_live_role;
use super::OperatorState;

/// Channels the hub subscribes to at connection time.
///
/// These are the channels the kernel currently publishes on. The list is
/// explicit (not a wildcard) so uncovered event types land only as their
/// emitters are added -- no fabricated events reach clients.
const SUBSCRIBED_CHANNELS: &[&str] = &[
    "narration", // henosis-broca: ActionLogged events
    "agent", // henosis-soma: AgentRegistered/Deregistered/Heartbeat/StatusChanged/QualityUpdated
    "workflow", // henosis-loom: RunCreated/Completed/Failed/Cancelled, StepStarted/Completed/Failed
];

/// Periodic heartbeat interval. The server sends a heartbeat frame every
/// `HEARTBEAT_INTERVAL_SECS` seconds so clients can detect stale connections.
const HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// Fixed public WebSocket subprotocol negotiated with authenticated clients.
const WS_PROTOCOL: &str = "henosis.v1";
/// Prefix separating the private bearer credential from the public subprotocol.
const WS_AUTH_PREFIX: &str = "henosis.auth.";

/// Convert an [`AxonEnvelope`] to a WS event JSON frame, enforcing org isolation.
///
/// Returns `None` when `env.tenant != org` -- events from other tenants are
/// silently dropped so connected operators never see cross-tenant data.
///
/// Returns `Some(json)` when the envelope belongs to `org`, with the shape:
/// ```json
/// {
///   "type": "<env.kind>",
///   "payload": {
///     "id": "<event-id>",
///     "channel": "<channel>",
///     "kind": "<env.kind>",
///     "tenant": "<tenant-uuid>",
///     "principal": "<principal-uuid>",
///     "occurred_at": "<rfc3339>",
///     "data": { <envelope-specific payload> }
///   }
/// }
/// ```
pub fn envelope_to_event(env: &AxonEnvelope, org: TenantId) -> Option<serde_json::Value> {
    if env.tenant != org {
        return None; // org-isolation filter: drop cross-tenant events
    }
    Some(json!({
        "type": env.kind,
        "payload": {
            "id": env.id,
            "channel": env.channel,
            "kind": env.kind,
            "tenant": env.tenant,
            "principal": env.principal,
            "occurred_at": env.occurred_at,
            "data": env.payload,
        }
    }))
}

/// Extract one bearer credential from the strict WebSocket subprotocol offer.
fn websocket_credential(headers: &HeaderMap) -> Result<Zeroizing<String>, StatusCode> {
    let mut values = headers.get_all(SEC_WEBSOCKET_PROTOCOL).iter();
    let value = values.next().ok_or(StatusCode::UNAUTHORIZED)?;
    if values.next().is_some() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let value = value.to_str().map_err(|_| StatusCode::UNAUTHORIZED)?;
    let protocols = value.split(',').map(str::trim).collect::<Vec<_>>();
    if protocols.len() != 2 || protocols[0] != WS_PROTOCOL {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let credential = protocols[1]
        .strip_prefix(WS_AUTH_PREFIX)
        .filter(|value| !value.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    Ok(Zeroizing::new(credential.to_string()))
}

/// `GET /ws` -- validate subprotocol authentication, upgrade, then stream events.
///
/// **Auth gate**: the bearer credential is decoded with [`auth::decode`] before
/// [`WebSocketUpgrade::on_upgrade`] is called. A missing or invalid credential
/// returns HTTP 401 -- the WS handshake is never completed for unauthenticated
/// callers (the close-code-1008 equivalent at the HTTP layer).
///
/// **After upgrade**: delegates to [`handle_socket`] which subscribes to
/// `SUBSCRIBED_CHANNELS` on the [`AxonBus`], then runs the event loop.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<OperatorState>,
) -> Response {
    // Decode the JWT BEFORE completing the upgrade. Bad token -> HTTP 401;
    // the handshake is never completed for unauthenticated callers.
    let credential = match websocket_credential(&headers) {
        Ok(credential) => credential,
        Err(status) => return status.into_response(),
    };
    let claims = match auth::decode(&credential, &state.jwt_secret) {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    // Parse the subject identifiers from the signed claims before consulting live policy.
    let org: TenantId = match claims.org.parse() {
        Ok(t) => t,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let principal = match claims.sub.parse() {
        Ok(principal) => principal,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let role = match resolve_live_role(&*state.plutus, org, principal).await {
        Ok(role) => role,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !can(role, Permission::OrgRead) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let now = chrono::Utc::now().timestamp();
    let Some(expires_in) = claims
        .exp
        .checked_sub(now)
        .filter(|remaining| *remaining > 0)
        .map(|remaining| Duration::from_secs(remaining as u64))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let axon = Arc::clone(&state.axon);
    let policy = Arc::clone(&state.plutus);
    ws.protocols([WS_PROTOCOL])
        .on_upgrade(move |socket| async move {
            handle_socket(socket, org, principal, axon, policy, expires_in).await;
        })
}

/// Internal event discriminant used to consolidate `tokio::select!` results.
///
/// One variant per source of activity in the session loop. Using an explicit
/// enum avoids multiple concurrent mutable borrows on the `socket` -- the
/// select! loop has exactly one `socket.recv()` arm; all sends happen after
/// the select! returns and the borrow is released.
enum SessionEvent {
    /// A message received from the connected client.
    Client(Option<Result<Message, axum::Error>>),
    /// An envelope forwarded from the merged AxonBus mpsc channel.
    Envelope(Option<AxonEnvelope>),
    /// A tick from the periodic heartbeat interval.
    Heartbeat,
    /// The authenticated access token reached its signed expiry.
    Expired,
}

/// Drive the authenticated WebSocket session: bus forwarding + heartbeat + ping/pong.
///
/// **Subscriptions**: subscribes to all channels in [`SUBSCRIBED_CHANNELS`],
/// each in a dedicated background task. Each forwarder task reads from its
/// `broadcast::Receiver` and pushes envelopes into a shared `mpsc::Sender`.
/// This merge pattern means the select! loop holds only one `mpsc::Receiver`,
/// avoiding multiple concurrent mutable borrows on the socket.
///
/// **Select! loop** (four arms):
/// 1. `socket.recv()` -- client frames. Responds to pings; breaks on close/error.
/// 2. `rx.recv()` -- merged bus envelopes. Calls [`envelope_to_event`] for org
///    filtering; forwards `Some` results as JSON text frames; skips `None`.
/// 3. `hb.tick()` -- heartbeat. Sends `{"type":"server.heartbeat","payload":{"ts":<u64>}}`.
/// 4. signed expiry -- closes the connection when its access token expires.
///
/// **Lag handling**: when a broadcast receiver falls behind (ring buffer
/// overrun), the forwarder logs a warning and continues -- the receiver
/// auto-advances to the oldest available message.
///
/// **Clean exit**: the session exits when the mpsc closes (`rx.recv()` returns
/// `None`), the client closes the connection, or any send fails.
async fn handle_socket(
    mut socket: axum::extract::ws::WebSocket,
    org: TenantId,
    principal: PrincipalId,
    axon: Arc<AxonBus>,
    policy: Arc<dyn PolicyBackend>,
    expires_in: Duration,
) {
    use tokio::sync::mpsc;

    // Merge all channel receivers into one mpsc so the select! loop has a
    // single recv() future -- avoids concurrent &mut borrows on the socket.
    let (tx, mut rx) = mpsc::channel::<AxonEnvelope>(256);
    let mut forwarders = Vec::with_capacity(SUBSCRIBED_CHANNELS.len());

    for &channel in SUBSCRIBED_CHANNELS {
        let mut receiver = axon.subscribe(channel);
        let fwd_tx = tx.clone();
        let forwarder = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(env) => {
                        // Send to the merged channel; break if the receiver dropped.
                        if fwd_tx.send(env).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // The broadcast ring buffer overran -- log and continue.
                        // broadcast::Receiver auto-advances; next recv() returns
                        // the oldest available message after the gap.
                        tracing::warn!(
                            channel,
                            skipped = n,
                            "WS event hub bus receiver lagged -- skipping overrun events"
                        );
                    }
                    Err(RecvError::Closed) => break, // AxonBus dropped for this channel
                }
            }
        });
        forwarders.push(forwarder);
    }
    // Drop the original sender so the mpsc closes when ALL forwarder tasks exit.
    drop(tx);

    // Consume the initial zero-delay tick so the first heartbeat fires after
    // HEARTBEAT_INTERVAL_SECS, not immediately.
    let mut hb = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    hb.tick().await;
    let expires = tokio::time::sleep(expires_in);
    tokio::pin!(expires);

    loop {
        // Gather the next event from one of the four sources.
        let event = tokio::select! {
            msg  = socket.recv()  => SessionEvent::Client(msg),
            env  = rx.recv()      => SessionEvent::Envelope(env),
            _    = hb.tick()      => SessionEvent::Heartbeat,
            _    = &mut expires   => SessionEvent::Expired,
        };

        match event {
            // -- Client frames --
            SessionEvent::Client(Some(Ok(Message::Ping(data)))) => {
                if socket.send(Message::Pong(data)).await.is_err() {
                    break;
                }
            }
            SessionEvent::Client(Some(Ok(Message::Close(_)))) | SessionEvent::Client(None) => {
                break; // client closed or stream ended cleanly
            }
            SessionEvent::Client(Some(Err(_))) => break, // transport error
            SessionEvent::Client(Some(Ok(_))) => {}      // ignore text/binary from client

            // -- AxonBus events --
            SessionEvent::Envelope(None) => break, // all forwarder tasks exited (bus dropped)
            SessionEvent::Envelope(Some(env)) => {
                if let Some(event_json) = envelope_to_event(&env, org) {
                    let text = serde_json::to_string(&event_json).unwrap_or_default();
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                // envelope_to_event returned None -> wrong tenant, silently skip
            }

            // -- Heartbeat --
            SessionEvent::Heartbeat => {
                let authorized = resolve_live_role(&*policy, org, principal)
                    .await
                    .is_ok_and(|role| can(role, Permission::OrgRead));
                if !authorized {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let hb_msg = json!({
                    "type": "server.heartbeat",
                    "payload": { "ts": ts }
                });
                let text = serde_json::to_string(&hb_msg).unwrap_or_default();
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }

            // -- Credential expiry --
            SessionEvent::Expired => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        }
    }
    for forwarder in forwarders {
        forwarder.abort();
    }
}

#[cfg(test)]
/// Tests for the WebSocket hub: org-filter purity + auth rejection.
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use henosis_broca::BrocaStore;
    use henosis_chiasm::ChiasmStore;
    use henosis_loom::LoomStore;
    use henosis_plutus::MockPolicyBackend;
    use henosis_soma::SomaStore;
    use henosis_thymus::ThymusStore;
    use syntheos_axon::AxonBus;
    use syntheos_contracts::{AxonEnvelope, EventId, PrincipalId, TenantId, Timestamp};
    use syntheos_identity::InMemoryDirectory;

    use super::super::auth::{sign, OperatorClaims};
    use super::super::OperatorState;
    use super::{envelope_to_event, websocket_credential, ws_handler, WS_AUTH_PREFIX, WS_PROTOCOL};

    /// Build an in-memory `OperatorState` for WS tests.
    ///
    /// Returns `(state, org, jwt_secret, axon_bus)`. The org is the
    /// `TenantId` that a valid JWT should carry and that matching envelopes
    /// must have as their `tenant`.
    fn make_state() -> (OperatorState, TenantId, Arc<Vec<u8>>, Arc<AxonBus>) {
        let bus = Arc::new(AxonBus::new());
        let dir = Arc::new(InMemoryDirectory::new());
        let soma = Arc::new(SomaStore::open_in_memory(bus.clone(), dir.clone()).expect("soma"));
        let chiasm = Arc::new(ChiasmStore::open_in_memory(bus.clone()).expect("chiasm"));
        let broca = Arc::new(BrocaStore::open_in_memory(bus.clone()).expect("broca"));
        let loom = Arc::new(LoomStore::open_in_memory(bus.clone()).expect("loom"));
        let thymus = Arc::new(ThymusStore::open_in_memory(bus.clone()).expect("thymus"));
        let accounts =
            Arc::new(syntheos_identity::SqliteDirectory::open_in_memory().expect("accounts"));
        let plutus: Arc<dyn henosis_plutus::PolicyBackend> =
            Arc::new(MockPolicyBackend::allow_all());
        let jwt_secret: Arc<Vec<u8>> = Arc::new(b"ws-test-secret-32bytes-padded!!!".to_vec());

        let org = TenantId::new();
        let state = OperatorState {
            accounts,
            plutus,
            jwt_secret: jwt_secret.clone(),
            soma,
            chiasm,
            broca,
            thymus,
            loom,
            axon: bus.clone(),
            // Not exercised by these handler-level tests (no CORS preflight involved).
            cors_origins: Arc::new(vec![]),
        };
        (state, org, jwt_secret, bus)
    }

    /// Mint a far-future operator JWT for the given org and secret.
    ///
    /// Uses `iat = 9_000_000_000` so `exp = iat + 3600` is always in the future.
    fn mint_token(org: TenantId, secret: &[u8]) -> String {
        let principal = PrincipalId::new();
        let claims = OperatorClaims::new(
            &principal.to_string(),
            &org.to_string(),
            "viewer",
            "ws-test-session",
            9_000_000_000,
            3600,
        );
        sign(&claims, secret).expect("sign test JWT")
    }

    /// Build a WebSocket request with the fixed protocol and private JWT offer.
    fn websocket_request(
        port: u16,
        credential: &str,
    ) -> tokio_tungstenite::tungstenite::http::Request<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let mut request = format!("ws://127.0.0.1:{port}/ws")
            .into_client_request()
            .expect("websocket request");
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            format!("{WS_PROTOCOL}, {WS_AUTH_PREFIX}{credential}")
                .parse()
                .expect("protocol header"),
        );
        request
    }

    /// Authentication parsing rejects missing, duplicate, and malformed protocol offers.
    #[test]
    fn websocket_protocol_authentication_is_strict() {
        let empty = HeaderMap::new();
        assert_eq!(websocket_credential(&empty), Err(StatusCode::UNAUTHORIZED));

        let mut valid = HeaderMap::new();
        valid.insert(
            SEC_WEBSOCKET_PROTOCOL,
            "henosis.v1, henosis.auth.token-value"
                .parse()
                .expect("header"),
        );
        assert_eq!(
            websocket_credential(&valid).expect("credential").as_str(),
            "token-value"
        );

        valid.append(
            SEC_WEBSOCKET_PROTOCOL,
            "henosis.v1, henosis.auth.other".parse().expect("header"),
        );
        assert_eq!(websocket_credential(&valid), Err(StatusCode::UNAUTHORIZED));
    }

    // ----------------------------------------------------------------
    // Unit test: envelope_to_event -- the org-filter assertion (REQUIRED)
    // ----------------------------------------------------------------

    /// `envelope_to_event` forwards events whose tenant matches the connection
    /// org and silently drops events from other tenants (org-isolation rule).
    ///
    /// This is the primary correctness gate for the WS hub: cross-tenant data
    /// must NEVER be forwarded, regardless of channel or event kind.
    #[test]
    fn envelope_to_event_org_filter() {
        let org = TenantId::new();
        let other_org = TenantId::new();

        let env = AxonEnvelope {
            id: EventId::new(),
            channel: "narration".to_string(),
            kind: "narration.action_logged".to_string(),
            tenant: org,
            principal: PrincipalId::new(),
            occurred_at: Timestamp::now(),
            payload: serde_json::json!({ "action": "test" }),
        };

        // Matching tenant: Some with correct type field.
        let result = envelope_to_event(&env, org);
        assert!(result.is_some(), "matching tenant must produce Some");
        let json = result.unwrap();
        assert_eq!(
            json["type"], "narration.action_logged",
            "type must equal env.kind"
        );
        assert_eq!(
            json["payload"]["channel"], "narration",
            "payload.channel must match"
        );

        // Different tenant: None (org-isolation filter drops it).
        let result_other = envelope_to_event(&env, other_org);
        assert!(
            result_other.is_none(),
            "cross-tenant envelope must produce None"
        );
    }

    // ----------------------------------------------------------------
    // Integration test: auth rejection + bus event forwarding over real TCP
    // ----------------------------------------------------------------

    /// Verifies both auth rejection and org-filtered event forwarding over a
    /// real TCP connection. Uses `tokio-tungstenite` because `WebSocketUpgrade`
    /// requires an actual `hyper::upgrade::OnUpgrade` extension that `oneshot`
    /// cannot provide.
    ///
    /// **Auth assertions**:
    /// - A `connect_async` with a malformed credential protocol fails (server returns non-101).
    /// - A `connect_async` with a valid credential protocol succeeds (101 Switching Protocols).
    ///
    /// **Org-filter assertions** (after a valid-token connection):
    /// - An envelope whose `tenant == org` arrives as a JSON text frame.
    /// - An envelope whose `tenant != org` does NOT arrive within a short timeout.
    #[tokio::test]
    async fn ws_rejects_bad_token_and_forwards_org_events() {
        use futures::StreamExt;

        let (state, org, jwt_secret, bus) = make_state();
        let valid_token = mint_token(org, &jwt_secret);

        let app: Router = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(state);

        // Bind on a random OS-assigned port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let port = listener.local_addr().expect("local addr").port();

        // Serve in background.
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        // -- Auth rejection: a bad token must be refused before the upgrade. --
        let bad_result =
            tokio_tungstenite::connect_async(websocket_request(port, "garbage_token")).await;
        assert!(
            bad_result.is_err(),
            "connect_async with a garbage token must fail (server returns non-101)"
        );

        // -- Valid token: connect succeeds. --
        let (mut ws_stream, response) =
            tokio_tungstenite::connect_async(websocket_request(port, &valid_token))
                .await
                .expect("valid token must connect successfully");
        assert_eq!(
            response
                .headers()
                .get(SEC_WEBSOCKET_PROTOCOL)
                .and_then(|value| value.to_str().ok()),
            Some(WS_PROTOCOL)
        );

        // Wait briefly for the server task to subscribe to the bus channels.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // -- Publish an org-matching envelope and verify it arrives. --
        let principal = PrincipalId::new();
        let matching_env = AxonEnvelope {
            id: EventId::new(),
            channel: "narration".to_string(),
            kind: "narration.action_logged".to_string(),
            tenant: org,
            principal,
            occurred_at: Timestamp::now(),
            payload: serde_json::json!({ "action": "ws_test" }),
        };
        bus.publish(&matching_env);

        let frame = tokio::time::timeout(Duration::from_secs(2), ws_stream.next())
            .await
            .expect("timed out waiting for the org-matching event")
            .expect("stream ended unexpectedly")
            .expect("WS protocol error");

        let text = frame.into_text().expect("expected text frame");
        let json: serde_json::Value =
            serde_json::from_str(&text).expect("frame must be valid JSON");
        assert_eq!(
            json["type"], "narration.action_logged",
            "type must match env.kind"
        );
        assert_eq!(
            json["payload"]["channel"], "narration",
            "payload.channel must be forwarded"
        );

        // -- Publish a cross-tenant envelope; it must NOT arrive (org-isolation). --
        let other_org = TenantId::new();
        let cross_env = AxonEnvelope {
            id: EventId::new(),
            channel: "narration".to_string(),
            kind: "narration.action_logged".to_string(),
            tenant: other_org, // wrong tenant -- dropped by the org filter
            principal,
            occurred_at: Timestamp::now(),
            payload: serde_json::json!({ "action": "cross_tenant_must_not_arrive" }),
        };
        bus.publish(&cross_env);

        // The org filter must silence the cross-tenant event within the timeout.
        let cross_frame = tokio::time::timeout(Duration::from_millis(100), ws_stream.next()).await;
        assert!(
            cross_frame.is_err(),
            "cross-tenant event must not be forwarded (org-isolation assertion REQUIRED)"
        );
    }
}
