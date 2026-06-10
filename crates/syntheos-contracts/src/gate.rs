//! The input-authorization gate interface. The dispatcher runs a chain of gates
//! (Pistis, Plutus, Eidolon-input, Human, Phylax) against each request.
//!
//! Output redaction/transform is handled by a separate `OutputFilter` interface
//! and is deliberately not modeled here.

use serde::{Deserialize, Serialize};

use crate::action::{RequestContext, ToolInvocation};

/// What the dispatcher hands to each gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRequest {
    /// The request context (tenant, principal, persona, etc.).
    pub context: RequestContext,
    /// The proposed action being authorized.
    pub invocation: ToolInvocation,
}

/// The outcome of one input-authorization gate.
///
/// Marked `#[non_exhaustive]`: gate outcomes may grow, and downstream matches
/// must keep a wildcard arm.
///
/// This is an in-process type. It derives `Serialize`/`Deserialize` for logging
/// and persistence only -- it carries no cross-version wire-compatibility
/// guarantee. A persisted decision must be read back by an equal-or-newer build,
/// since an older reader will reject a variant it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum GateDecision {
    /// The action is permitted by this gate.
    Allow,
    /// The action is rejected, with a human-readable reason.
    Deny {
        /// Why the action was denied.
        reason: String,
    },
    /// The action requires explicit approval before proceeding.
    RequireApproval {
        /// The prompt shown to the approver.
        prompt: String,
    },
}

/// One authorization gate in the dispatcher chain.
///
/// Object-safe via `async_trait` so the dispatcher can hold `Vec<Box<dyn Gate>>`.
#[async_trait::async_trait]
pub trait Gate: Send + Sync {
    /// A short, stable name for this gate (used in logs and audit trails).
    fn name(&self) -> &str;

    /// Authorize (or reject, or escalate) a single request.
    async fn check(&self, req: &GateRequest) -> GateDecision;
}

/// Tests for gate trait object-safety and decision wire contracts.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{PrincipalId, TenantId};

    /// A gate that permits everything; exists only to prove the trait is usable.
    struct AllowAllGate;

    /// Implement `Gate` for the test allow-all gate.
    #[async_trait::async_trait]
    impl Gate for AllowAllGate {
        /// Return the stable test gate name.
        fn name(&self) -> &str {
            "allow-all"
        }

        /// Permit every request in the object-safety test.
        async fn check(&self, _req: &GateRequest) -> GateDecision {
            GateDecision::Allow
        }
    }

    /// Build a minimal gate request for tests.
    fn sample_request() -> GateRequest {
        GateRequest {
            context: RequestContext {
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
            },
            invocation: ToolInvocation {
                tool: "kleos".to_string(),
                action: "memory_store".to_string(),
                args: serde_json::json!({}),
            },
        }
    }

    /// Boxed gates remain object-safe and callable.
    #[tokio::test]
    async fn boxed_gate_is_object_safe_and_allows() {
        let gate: Box<dyn Gate> = Box::new(AllowAllGate);
        assert_eq!(gate.name(), "allow-all");
        let decision = gate.check(&sample_request()).await;
        assert_eq!(decision, GateDecision::Allow);
    }

    /// Gate decision variants roundtrip without adding or removing variants.
    #[test]
    fn decision_variants_roundtrip() {
        for d in [
            GateDecision::Allow,
            GateDecision::Deny {
                reason: "no".to_string(),
            },
            GateDecision::RequireApproval {
                prompt: "ok?".to_string(),
            },
        ] {
            let json = serde_json::to_string(&d).expect("serialize");
            let back: GateDecision = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(d, back);
        }
    }

    /// Gate requests reject misspelled or extra fields.
    #[test]
    fn gate_request_rejects_unknown_fields() {
        let req = sample_request();
        let mut value = serde_json::to_value(req).expect("serialize");
        value["cotnext"] = serde_json::json!({});
        let err = serde_json::from_value::<GateRequest>(value).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }
}
