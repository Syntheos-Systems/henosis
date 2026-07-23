//! Fail-closed fallback gates and executor for explicit bootstrap and test configurations.
//!
//! Production supplies concrete authority gates and executors. These fallbacks preserve the
//! canonical gate shape while denying every action when a caller explicitly has no authorities
//! or executor to provide.

use async_trait::async_trait;
use syntheos_contracts::{
    Gate, GateDecision, GateError, GateRequest, RequestContext, ToolInvocation,
};

use crate::dispatcher::CANONICAL_GATE_ORDER;
use crate::executor::{Executor, ExecutorError};

/// A gate that denies everything for an authority unavailable in the current configuration.
pub struct DenyGate {
    /// The authority name this gate reports and denies on behalf of.
    name: &'static str,
}

/// Implements construction of the deny-by-default gate.
impl DenyGate {
    /// Create a deny-everything gate that reports `name`.
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
/// Applies the unconditional fail-closed denial policy.
impl Gate for DenyGate {
    /// Returns the authority name represented by this deny gate.
    fn name(&self) -> &str {
        self.name
    }

    /// Denies every request because the authority is not wired.
    async fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::Deny {
            reason: format!("fail-closed: the {} authority is unavailable", self.name),
        })
    }
}

/// The canonical deny-by-default gate chain, in dispatch order:
/// `pistis -> plutus -> eidolon -> human -> phylaxd`, every gate a [`DenyGate`].
///
/// The chain is structurally canonical so [`crate::Dispatcher::new`] accepts it, but it denies
/// every action at the first gate.
pub fn deny_gate_chain() -> Vec<Box<dyn Gate>> {
    CANONICAL_GATE_ORDER
        .into_iter()
        .map(|name| Box::new(DenyGate::new(name)) as Box<dyn Gate>)
        .collect()
}

/// An executor that refuses to run anything: the fail-closed counterpart to [`deny_gate_chain`]
/// for a server without a configured executor.
///
/// Behind [`deny_gate_chain`] it is unreachable (every action is denied before execution); it
/// exists so that a chain misconfiguration cannot silently execute an action.
pub struct DenyExecutor;

#[async_trait]
/// Refuses every executor invocation in a fail-closed fallback configuration.
impl Executor for DenyExecutor {
    /// Returns an error for every attempted invocation.
    async fn execute(
        &self,
        _ctx: &RequestContext,
        invocation: &ToolInvocation,
    ) -> Result<serde_json::Value, ExecutorError> {
        Err(ExecutorError::new(format!(
            "fail-closed: no executor is wired for tool '{}'",
            invocation.tool
        )))
    }
}

#[cfg(test)]
/// Tests fail-closed default behavior.
mod tests {
    use super::*;
    use syntheos_contracts::{PrincipalId, TenantId};

    #[tokio::test]
    /// Reports the unavailable authority in a stable failure reason.
    async fn deny_gate_reports_public_failure_reason() {
        let gate = DenyGate::new("pistis");
        let request = GateRequest {
            context: RequestContext {
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
                authority: None,
            },
            invocation: ToolInvocation {
                tool: "test".to_string(),
                action: "run".to_string(),
                args: serde_json::json!({}),
            },
        };

        let decision = gate.check(&request).await.expect("deny gate must decide");
        assert_eq!(
            decision,
            GateDecision::Deny {
                reason: "fail-closed: the pistis authority is unavailable".to_string()
            }
        );
    }
}
