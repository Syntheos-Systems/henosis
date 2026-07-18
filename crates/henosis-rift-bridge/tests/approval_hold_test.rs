//! Operator-facing behaviour of approvals held across a bridge pause.
//!
//! Drives the real control server over HTTP. The bug these cover: approving a
//! proposal while the bridge was paused REMOVED it from the registry and
//! parked it in the drain task's private queue, so `GET /control/approvals`
//! showed nothing and the operator could neither see nor cancel the pending
//! execution until somebody unpaused the bridge.

use henosis_rift_bridge::config::ControlConfig;
use henosis_rift_bridge::control;
use henosis_rift_bridge::execution::approval::{
    decide_drain_action, ApprovalRegistry, DrainAction,
};
use henosis_rift_bridge::execution::{PendingProposal, ProposalId};
use tokio::sync::mpsc;

/// Bring up the control server on a dedicated port and return its base URL.
///
/// Each test uses a distinct port so they can run concurrently under the
/// default test harness without racing for a bind address.
async fn serve_control(port: u16, registry: ApprovalRegistry) -> (String, mpsc::Receiver<PendingProposal>) {
    let (tx, rx) = mpsc::channel::<PendingProposal>(8);
    let config = ControlConfig {
        bind_addr: format!("127.0.0.1:{port}"),
        auth_token: "smoke-token".to_string(),
    };
    tokio::spawn(async move {
        let _ = control::serve(config, registry, tx).await;
    });

    // Wait for the listener rather than sleeping a fixed amount.
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (base, rx)
}

/// Register a proposal the operator will act on.
fn seed(registry: &ApprovalRegistry) -> ProposalId {
    registry.register(
        "architect".to_string(),
        "task-42".to_string(),
        "refactor the widget".to_string(),
        vec![],
        "rift".to_string(),
    )
}

/// Fetch the approvals list as JSON.
async fn list(client: &reqwest::Client, base: &str) -> serde_json::Value {
    client
        .get(format!("{base}/control/approvals"))
        .bearer_auth("smoke-token")
        .send()
        .await
        .expect("list request")
        .json()
        .await
        .expect("list json")
}

/// Post an action for a proposal id and return the HTTP status.
async fn act(client: &reqwest::Client, base: &str, id: ProposalId, action: &str) -> u16 {
    client
        .post(format!("{base}/control/approvals/{}", id.0))
        .bearer_auth("smoke-token")
        .json(&serde_json::json!({ "action": action }))
        .send()
        .await
        .expect("action request")
        .status()
        .as_u16()
}

/// An approval held through a pause stays listed, tagged, and rejectable.
#[tokio::test]
async fn held_approval_is_visible_and_cancellable_while_paused() {
    let registry = ApprovalRegistry::new(1800);
    let id = seed(&registry);
    let (base, mut approved_rx) = serve_control(39217, registry.clone()).await;
    let client = reqwest::Client::new();

    // Operator approves while the bridge is paused.
    assert_eq!(act(&client, &base, id, "approve").await, 200);

    let proposal = approved_rx.recv().await.expect("proposal reaches the drain");
    assert_eq!(
        decide_drain_action(true, &registry, proposal.id),
        DrainAction::Hold,
        "a paused bridge must hold, not dispatch"
    );

    // THE REGRESSION: the held approval must still be listed, and labelled so
    // the operator can tell it is awaiting unpause rather than awaiting them.
    let body = list(&client, &base).await;
    let pending = body["pending"].as_array().expect("pending array");
    assert_eq!(pending.len(), 1, "held approval must remain visible");
    assert_eq!(pending[0]["id"].as_u64(), Some(id.0));
    assert_eq!(pending[0]["state"].as_str(), Some("approved_held"));
    assert_eq!(pending[0]["task_id"].as_str(), Some("task-42"));

    // And the operator can still cancel it.
    assert_eq!(act(&client, &base, id, "reject").await, 200);

    let body = list(&client, &base).await;
    assert!(
        body["pending"].as_array().expect("pending array").is_empty(),
        "rejected approval must be gone"
    );

    // Unpausing must not resurrect the cancelled execution.
    assert!(
        registry.take_approved().is_empty(),
        "a rejected hold must never dispatch on unpause"
    );
}

/// A pending proposal is reported as pending, and unpause flushes a hold.
#[tokio::test]
async fn unpause_dispatches_the_held_approval_exactly_once() {
    let registry = ApprovalRegistry::new(1800);
    let id = seed(&registry);
    let (base, mut approved_rx) = serve_control(39218, registry.clone()).await;
    let client = reqwest::Client::new();

    // Before any decision it reads as pending.
    let body = list(&client, &base).await;
    assert_eq!(body["pending"][0]["state"].as_str(), Some("pending"));

    assert_eq!(act(&client, &base, id, "approve").await, 200);
    let proposal = approved_rx.recv().await.expect("proposal reaches the drain");
    assert_eq!(
        decide_drain_action(true, &registry, proposal.id),
        DrainAction::Hold
    );

    // Unpause: the flush claims the held approval.
    let flushed = registry.take_approved();
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0].id, id);

    // A late redelivery of the same proposal on the channel must not run it
    // a second time now that the bridge is unpaused.
    assert_eq!(
        decide_drain_action(false, &registry, proposal.id),
        DrainAction::Skip,
        "exactly-once: the flush already dispatched this proposal"
    );

    let body = list(&client, &base).await;
    assert!(body["pending"].as_array().expect("pending array").is_empty());
}

/// Approving twice cannot queue two executions of the same task.
#[tokio::test]
async fn second_approve_is_rejected_by_the_control_api() {
    let registry = ApprovalRegistry::new(1800);
    let id = seed(&registry);
    let (base, _rx) = serve_control(39219, registry.clone()).await;
    let client = reqwest::Client::new();

    assert_eq!(act(&client, &base, id, "approve").await, 200);
    assert_eq!(
        act(&client, &base, id, "approve").await,
        404,
        "an already-approved proposal is not approvable again"
    );
}
