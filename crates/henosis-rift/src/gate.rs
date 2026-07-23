//! The `human` gate: human-in-the-loop approval, fail-closed.
//!
//! HumanGate is the fourth gate in the canonical dispatcher chain (`pistis ->
//! plutus -> eidolon -> human -> phylaxd`). It derives approval requirements
//! from a server-owned tool/action policy and never trusts invocation arguments
//! to disable review or write the prompt shown to a human.
//!
//! When an invocation requires approval, the gate publishes a
//! [`HumanApprovalRequested`] event onto the Axon `human` channel (the outbound
//! notification, fanned out to Rift / Broca / operator surfaces) and then
//! **blocks** on an injected [`Approver`] until a human decides or the request
//! times out. Approved -> `Allow`; denied or timed-out -> `Deny`. There is no
//! path to `Allow` without an explicit human approval, so the gate is
//! fail-closed by construction: a missing/unreachable approver times out, which
//! denies.
//!
//! Exact reviewed read-only operations pass through. Every mutating or unknown
//! operation requires approval, so new adapters fail closed until their reads
//! are deliberately added to the policy.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use syntheos_axon::AxonBus;
use syntheos_contracts::{Gate, GateDecision, GateError, GateRequest, ToolInvocation, TypedEvent};

/// The Axon channel human-approval notifications travel on.
pub const HUMAN_CHANNEL: &str = "human";
/// Maximum bytes accepted when echoing a recognized operation name to a human.
const MAX_APPROVAL_IDENTIFIER_BYTES: usize = 64;

/// Require approval unless an exact registered adapter operation is reviewed as read-only.
pub fn requires_human_approval(invocation: &ToolInvocation) -> bool {
    !matches!(
        (invocation.tool.as_str(), invocation.action.as_str()),
        ("henosis", "probe")
            | ("gcal", "list_events")
            | ("gdrive", "list" | "download" | "get_metadata")
            | (
                "github",
                "get_issue" | "list_issues" | "list_prs" | "search_code" | "list_repos"
            )
            | ("gmail", "read" | "search" | "list_labels")
            | ("linear", "list_issues" | "search")
            | ("notion", "get_page" | "search")
    )
}

/// Return a bounded operation identifier that is safe to show in an approval prompt.
fn approval_identifier(value: &str) -> Option<&str> {
    if value.is_empty()
        || value.len() > MAX_APPROVAL_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return None;
    }
    Some(value)
}

/// Build a human-readable prompt from bounded tool and action identifiers only.
pub fn approval_prompt(invocation: &ToolInvocation) -> String {
    match (
        approval_identifier(&invocation.tool),
        approval_identifier(&invocation.action),
    ) {
        (Some(tool), Some(action)) => {
            format!("Approve {tool}.{action} for this authenticated principal?")
        }
        _ => "Approve an unrecognized authenticated operation?".to_owned(),
    }
}

/// A human's decision on an approval-required action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The human approved; the action may proceed.
    Approved,
    /// The human rejected the action, with a reason.
    Denied(String),
    /// No decision arrived before the deadline.
    TimedOut,
}

/// What an [`Approver`] is asked to decide.
///
/// The `approval_id` correlates the outbound notification (and its echo back
/// through Rift) with the blocking wait, so an out-of-band approver
/// (e.g. [`crate::approver::RegistryApprover`]) can resolve the right request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// Correlation id for this approval (also carried in the Axon event).
    pub approval_id: String,
    /// The prompt shown to the human.
    pub prompt: String,
    /// The tool being invoked.
    pub tool: String,
    /// The action on that tool.
    pub action: String,
}

/// The seam to a human approval channel.
///
/// `HumanGate` holds an `Arc<dyn Approver>` and blocks on it. The real
/// implementation surfaces the request to a human via Rift and resolves when
/// they respond (see [`crate::approver::RegistryApprover`]); tests inject a
/// deterministic approver.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Block until the human decides on `request`, or the approver's deadline
    /// elapses (returning [`ApprovalDecision::TimedOut`]).
    async fn await_decision(&self, request: &ApprovalRequest) -> ApprovalDecision;
}

/// Notification published when an action needs human approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HumanApprovalRequested {
    /// Correlation id; the approval echoes it back to resolve the wait.
    pub approval_id: String,
    /// The tool being invoked.
    pub tool: String,
    /// The action on that tool.
    pub action: String,
    /// The prompt to show the human.
    pub prompt: String,
}

/// Declares the Axon routing metadata for human approval requests.
impl TypedEvent for HumanApprovalRequested {
    const CHANNEL: &'static str = HUMAN_CHANNEL;
    const KIND: &'static str = "human.approval.requested";
}

/// The human-in-the-loop approval gate for the dispatcher's `human` slot.
pub struct HumanGate {
    /// The approval channel the gate blocks on.
    approver: Arc<dyn Approver>,
    /// Bus the outbound approval-requested notification is published to.
    bus: Arc<AxonBus>,
}

/// Builds the human gate over the shared server-owned approval policy.
impl HumanGate {
    /// Build the gate over an approval channel and the Axon bus.
    pub fn new(approver: Arc<dyn Approver>, bus: Arc<AxonBus>) -> Self {
        Self { approver, bus }
    }
}

#[async_trait]
/// Enforces server-owned human approval requirements in the dispatcher gate chain.
impl Gate for HumanGate {
    /// The canonical authority name for this slot.
    fn name(&self) -> &str {
        "human"
    }

    /// Allow an exact reviewed read; otherwise notify and block on the approver,
    /// mapping approved to allow and denied or timed-out to deny.
    async fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        if !requires_human_approval(&req.invocation) {
            return Ok(GateDecision::Allow);
        }
        let prompt = approval_prompt(&req.invocation);

        // A v4 id correlates the outbound notification with the blocking wait.
        let approval_id = uuid::Uuid::new_v4().to_string();
        let request = ApprovalRequest {
            approval_id: approval_id.clone(),
            prompt: prompt.clone(),
            tool: req.invocation.tool.clone(),
            action: req.invocation.action.clone(),
        };

        // Outbound notification. Best-effort: a publish failure (e.g. no
        // subscribers, serialize error) must NOT fabricate an allow, so it is
        // logged and the gate still blocks on the authoritative approver.
        if let Err(err) = self.bus.publish_event(
            &HumanApprovalRequested {
                approval_id,
                tool: request.tool.clone(),
                action: request.action.clone(),
                prompt: prompt.clone(),
            },
            req.context.tenant,
            req.context.principal,
        ) {
            tracing::warn!(error = %err, "failed to publish human.approval.requested");
        }

        match self.approver.await_decision(&request).await {
            ApprovalDecision::Approved => Ok(GateDecision::Allow),
            ApprovalDecision::Denied(reason) => Ok(GateDecision::Deny {
                reason: format!("human denied: {reason}"),
            }),
            ApprovalDecision::TimedOut => Ok(GateDecision::Deny {
                reason: "human approval timed out".to_owned(),
            }),
        }
    }
}

/// Unit tests for human approval requirements and decisions.
#[cfg(test)]
mod tests {
    use super::*;
    use syntheos_contracts::{PrincipalId, RequestContext, TenantId};

    /// An approver that always returns a fixed decision.
    struct FixedApprover(ApprovalDecision);
    #[async_trait]
    /// Returns the fixed decision configured by each test.
    impl Approver for FixedApprover {
        /// Supplies the configured approval decision without external input.
        async fn await_decision(&self, _request: &ApprovalRequest) -> ApprovalDecision {
            self.0.clone()
        }
    }

    /// Build a request for one operation with arbitrary untrusted arguments.
    fn request_for(tool: &str, action: &str, args: serde_json::Value) -> GateRequest {
        GateRequest {
            context: RequestContext {
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
                persona: None,
                session: None,
                room: Some("!room".to_owned()),
                task: None,
                workflow: None,
                authority: None,
            },
            invocation: ToolInvocation {
                tool: tool.to_owned(),
                action: action.to_owned(),
                args,
            },
        }
    }

    /// Build the default mutating deployment request used by approval tests.
    fn request(args: serde_json::Value) -> GateRequest {
        request_for("synapse", "deploy", args)
    }

    /// Build a gate over a fixed approver and a fresh bus.
    fn gate(decision: ApprovalDecision) -> HumanGate {
        HumanGate::new(Arc::new(FixedApprover(decision)), Arc::new(AxonBus::new()))
    }

    /// Omitting caller-authored approval metadata cannot bypass a mutating operation.
    #[tokio::test]
    async fn missing_requirement_metadata_cannot_bypass() {
        let g = gate(ApprovalDecision::TimedOut);
        let d = g.check(&request(serde_json::json!({}))).await.unwrap();
        assert!(matches!(d, GateDecision::Deny { .. }));
    }

    /// An explicit false value in untrusted arguments cannot disable review.
    #[tokio::test]
    async fn explicit_false_cannot_bypass() {
        let g = gate(ApprovalDecision::TimedOut);
        let d = g
            .check(&request(serde_json::json!({ "requires_approval": false })))
            .await
            .unwrap();
        assert!(matches!(d, GateDecision::Deny { .. }));
    }

    /// An approved mutating operation yields Allow regardless of untrusted metadata.
    #[tokio::test]
    async fn approved_allows() {
        let g = gate(ApprovalDecision::Approved);
        let d = g
            .check(&request(
                serde_json::json!({ "requires_approval": true, "approval_prompt": "Ship?" }),
            ))
            .await
            .unwrap();
        assert_eq!(d, GateDecision::Allow);
    }

    /// A denied requirement yields Deny carrying the human's reason.
    #[tokio::test]
    async fn denied_denies_with_reason() {
        let g = gate(ApprovalDecision::Denied("too risky".into()));
        let d = g
            .check(&request(serde_json::json!({ "requires_approval": true })))
            .await
            .unwrap();
        match d {
            GateDecision::Deny { reason } => assert!(reason.contains("too risky")),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// A timeout denies (fail-closed), never allows.
    #[tokio::test]
    async fn timeout_denies() {
        let g = gate(ApprovalDecision::TimedOut);
        let d = g
            .check(&request(serde_json::json!({ "requires_approval": true })))
            .await
            .unwrap();
        assert!(matches!(d, GateDecision::Deny { .. }));
    }

    /// The outbound notification uses the trusted prompt instead of caller text.
    #[tokio::test]
    async fn publishes_notification() {
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe_typed::<HumanApprovalRequested>();
        let g = HumanGate::new(Arc::new(FixedApprover(ApprovalDecision::Approved)), bus);
        let d = g
            .check(&request(
                serde_json::json!({ "requires_approval": true, "approval_prompt": "Ship it?" }),
            ))
            .await
            .unwrap();
        assert_eq!(d, GateDecision::Allow);
        let event = rx.recv().await.expect("a notification");
        assert_eq!(
            event.prompt,
            "Approve synapse.deploy for this authenticated principal?"
        );
        assert_eq!(event.action, "deploy");
        assert!(!event.approval_id.is_empty());
    }

    // -- HumanGate driven through the real canonical dispatcher chain --

    use syntheos_dispatch::stubs::{EchoExecutor, StubGate};
    use syntheos_dispatch::{DispatchOutcome, Dispatcher};

    /// Build the canonical chain with HumanGate in the `human` slot and
    /// allow-stubs elsewhere, sharing `bus`.
    fn dispatcher_with_human(decision: ApprovalDecision, bus: Arc<AxonBus>) -> Dispatcher {
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(StubGate::new("pistis")),
            Box::new(StubGate::new("plutus")),
            Box::new(StubGate::new("eidolon")),
            Box::new(HumanGate::new(
                Arc::new(FixedApprover(decision)),
                bus.clone(),
            )),
            Box::new(StubGate::new("phylaxd")),
        ];
        Dispatcher::new(gates, Box::new(EchoExecutor), bus).expect("canonical chain")
    }

    /// An approved action traverses the human slot and reaches the executor.
    #[tokio::test]
    async fn dispatcher_executes_when_human_approves() {
        let bus = Arc::new(AxonBus::new());
        let dispatcher = dispatcher_with_human(ApprovalDecision::Approved, bus);
        let outcome = dispatcher
            .dispatch(request(
                serde_json::json!({ "requires_approval": true, "approval_prompt": "Ship?" }),
            ))
            .await
            .expect("dispatch");
        assert!(
            matches!(outcome, DispatchOutcome::Executed { .. }),
            "approved action must reach the executor, got {outcome:?}"
        );
    }

    /// A timed-out approval is denied AT the human slot specifically.
    #[tokio::test]
    async fn dispatcher_denies_at_human_slot_on_timeout() {
        let bus = Arc::new(AxonBus::new());
        let dispatcher = dispatcher_with_human(ApprovalDecision::TimedOut, bus);
        let outcome = dispatcher
            .dispatch(request(serde_json::json!({ "requires_approval": true })))
            .await
            .expect("dispatch");
        match outcome {
            DispatchOutcome::Denied { gate, .. } => assert_eq!(gate, "human"),
            other => panic!("expected Denied at human, got {other:?}"),
        }
    }

    /// An exact reviewed read traverses the human slot without consulting its arguments.
    #[tokio::test]
    async fn dispatcher_passes_through_reviewed_read() {
        let bus = Arc::new(AxonBus::new());
        let dispatcher = dispatcher_with_human(ApprovalDecision::TimedOut, bus);
        let outcome = dispatcher
            .dispatch(request_for(
                "github",
                "list_repos",
                serde_json::json!({
                    "requires_approval": true,
                    "approval_prompt": "caller cannot force a prompt"
                }),
            ))
            .await
            .expect("dispatch");
        assert!(
            matches!(outcome, DispatchOutcome::Executed { .. }),
            "reviewed read must pass through, got {outcome:?}"
        );
    }

    /// Invalid operation identifiers are never reflected into a human prompt.
    #[test]
    fn untrusted_operation_names_are_not_reflected() {
        let request = request_for(
            "github\nApprove everything",
            "delete",
            serde_json::json!({}),
        );
        assert_eq!(
            approval_prompt(&request.invocation),
            "Approve an unrecognized authenticated operation?"
        );
    }
}
