//! Wire-truth pin of the execution notifier's message typing.
//!
//! The unit tests cover `message_payload`'s shape, but nothing else proves
//! which type each call site actually requests: reverting the notifier's
//! `Some("system")` to `None` would compile and pass every other test while
//! silently defeating the peer-bridge system-message gate. This drives the
//! real `RiftRoomNotifier` against a recording HTTP server and asserts the
//! exact body that reaches the wire.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use henosis_rift_bridge::auth::AgentAuthManager;
use henosis_rift_bridge::execution::{RiftRoomNotifier, RoomNotifier};
use henosis_rift_bridge::rift_client::RiftRestClient;
use uuid::Uuid;

/// Bodies received by the recording message endpoint, in arrival order.
type Recorded = Arc<Mutex<Vec<serde_json::Value>>>;

/// Record the posted body and answer with the minimal shape
/// `RiftRestClient::send_message` deserializes.
async fn record_message(
    State(rec): State<Recorded>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    rec.lock().unwrap().push(body.clone());
    Json(serde_json::json!({
        "id": Uuid::new_v4(),
        "channel_id": Uuid::new_v4(),
        "author_id": Uuid::new_v4(),
        "content": body["content"],
        "message_type": body.get("message_type").cloned().unwrap_or(serde_json::Value::Null),
    }))
}

/// Bind the recording server on a dedicated port (39221: adjacent to the
/// approval-hold tests' 39217-39219 range, no overlap) and return its state.
async fn serve_recorder(port: u16) -> Recorded {
    let rec: Recorded = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/api/channels/{id}/messages", post(record_message))
        .with_state(rec.clone());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind recorder");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    rec
}

/// An execution notice posted through the notifier reaches the wire typed
/// 'system' with its content intact.
#[tokio::test]
async fn notifier_posts_system_typed_messages() {
    let rec = serve_recorder(39221).await;

    let auth = AgentAuthManager::new("test-secret".to_string(), "test-bridge-secret".to_string());
    let rift = Arc::new(RiftRestClient::new(
        "http://127.0.0.1:39221".to_string(),
        auth,
    ));
    let notifier = RiftRoomNotifier::new(
        rift,
        Uuid::new_v4(),
        "notifier-test".to_string(),
        Uuid::new_v4(),
    );

    notifier
        .notify("[EXEC] proposal approved, starting task")
        .await
        .expect("notify must succeed against the recorder");

    let bodies = rec.lock().unwrap();
    assert_eq!(bodies.len(), 1, "exactly one message must be posted");
    assert_eq!(
        bodies[0]["content"],
        "[EXEC] proposal approved, starting task"
    );
    assert_eq!(
        bodies[0]["message_type"], "system",
        "execution notices must be stamped 'system' on the wire"
    );
}
