//! The `human` gate: human-in-the-loop approval, fail-closed.
//!
//! HumanGate is the fourth gate in the canonical dispatcher chain (`pistis ->
//! plutus -> eidolon -> human -> phylax`). It acts only on an invocation that
//! *declares it requires approval*; everything else passes through (mirroring
//! how PistisGate only acts on capability-bearing invocations).
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
//! Convention: a trusted invocation builder marks an action with a boolean
//! `requires_approval` arg (and an optional `approval_prompt` string). The
//! principal cannot set these; richer policy (e.g. require approval for every
//! `deploy`/`delete` action kind) is future work layered on top of this seam.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use syntheos_axon::AxonBus;
use syntheos_contracts::{Gate, GateDecision, GateError, GateRequest, ToolInvocation, TypedEvent};

/// The Axon channel human-approval notifications travel on.
pub const HUMAN_CHANNEL: &str = "human";

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

impl HumanGate {
    /// Build the gate over an approval channel and the Axon bus.
    pub fn new(approver: Arc<dyn Approver>, bus: Arc<AxonBus>) -> Self {
        Self { approver, bus }
    }

    /// The approval prompt for this invocation, or `None` when it declares no
    /// approval requirement (then the gate allows it -- not its concern).
    ///
    /// Requires `args.requires_approval == true`. The prompt is
    /// `args.approval_prompt` when present, else a default naming the action.
    fn approval_prompt(invocation: &ToolInvocation) -> Option<String> {
        let requires = invocation
            .args
            .get("requires_approval")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !requires {
            return None;
        }
        let prompt = invocation
            .args
            .get("approval_prompt")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "Approve action '{}' on tool '{}'?",
                    invocation.action, invocation.tool
                )
            });
        Some(prompt)
    }
}

#[async_trait]
impl Gate for HumanGate {
    /// The canonical authority name for this slot.
    fn name(&self) -> &str {
        "human"
    }

    /// Allow an invocation that declares no approval requirement; otherwise
    /// notify and block on the approver, mapping the human's decision to a gate
    /// decision (approved -> Allow, denied/timed-out -> Deny). Fail-closed.
    async fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        let Some(prompt) = Self::approval_prompt(&req.invocation) else {
            return Ok(GateDecision::Allow);
        };

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

#[cfg(test)]
mod tests {
    use super::*;
    use syntheos_contracts::{PrincipalId, RequestContext, TenantId};

    /// An approver that always returns a fixed decision.
    struct FixedApprover(ApprovalDecision);
    #[async_trait]
    impl Approver for FixedApprover {
        async fn await_decision(&self, _request: &ApprovalRequest) -> ApprovalDecision {
            self.0.clone()
        }
    }

    /// Build a request, optionally declaring an approval requirement.
    fn request(args: serde_json::Value) -> GateRequest {
        GateRequest {
            context: RequestContext {
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
                persona: None,
                session: None,
                room: Some("!room".to_owned()),
                task: None,
                workflow: None,
            },
            invocation: ToolInvocation {
                tool: "synapse".to_owned(),
                action: "deploy".to_owned(),
                args,
            },
        }
    }

    /// Build a gate over a fixed approver and a fresh bus.
    fn gate(decision: ApprovalDecision) -> HumanGate {
        HumanGate::new(Arc::new(FixedApprover(decision)), Arc::new(AxonBus::new()))
    }

    /// An invocation with no approval requirement is allowed without blocking.
    #[tokio::test]
    async fn no_requirement_allowed() {
        let g = gate(ApprovalDecision::TimedOut); // never consulted
        let d = g.check(&request(serde_json::json!({}))).await.unwrap();
        assert_eq!(d, GateDecision::Allow);
    }

    /// `requires_approval: false` is not an approval requirement.
    #[tokio::test]
    async fn explicit_false_allowed() {
        let g = gate(ApprovalDecision::Denied("x".into()));
        let d = g
            .check(&request(serde_json::json!({ "requires_approval": false })))
            .await
            .unwrap();
        assert_eq!(d, GateDecision::Allow);
    }

    /// An approved requirement yields Allow.
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

    /// The outbound notification reaches a subscriber on the human channel.
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
        assert_eq!(event.prompt, "Ship it?");
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
            Box::new(HumanGate::new(Arc::new(FixedApprover(decision)), bus.clone())),
            Box::new(StubGate::new("phylax")),
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

    /// An action with no approval requirement traverses the human slot to the executor.
    #[tokio::test]
    async fn dispatcher_passes_through_when_no_approval_required() {
        let bus = Arc::new(AxonBus::new());
        let dispatcher = dispatcher_with_human(ApprovalDecision::TimedOut, bus);
        let outcome = dispatcher
            .dispatch(request(serde_json::json!({})))
            .await
            .expect("dispatch");
        assert!(
            matches!(outcome, DispatchOutcome::Executed { .. }),
            "no-approval action must pass through, got {outcome:?}"
        );
    }
}
