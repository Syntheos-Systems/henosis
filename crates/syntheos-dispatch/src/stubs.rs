//! Allow-all placeholder gates and executor, for TESTS ONLY.
//!
//! This module is feature-gated behind the non-default `stubs` cargo feature (and this crate's
//! own `cfg(test)`), so the allow-all chain can never compile into a default or release build.
//! Production supplies concrete authority gates and executors; [`crate::deny`] provides
//! explicit fail-closed fallbacks.

use async_trait::async_trait;
use syntheos_contracts::{
    Gate, GateDecision, GateError, GateRequest, RequestContext, ToolInvocation,
};

use crate::dispatcher::CANONICAL_GATE_ORDER;
use crate::executor::{Executor, ExecutorError};

/// A gate that allows everything while reporting a fixed test authority name.
pub struct StubGate {
    /// The name this stub reports (matching the authority it stands in for).
    name: &'static str,
}

/// Implements construction of the test-only allow-all gate.
impl StubGate {
    /// Create a stub gate that reports `name` and always allows.
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
/// Applies the test-only unconditional allow policy.
impl Gate for StubGate {
    /// Returns the authority name represented by this stub gate.
    fn name(&self) -> &str {
        self.name
    }

    /// Allows every request in test-only configurations.
    async fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::Allow)
    }
}

/// The canonical gate chain, all allow-all stubs, in dispatch order:
/// `pistis -> plutus -> eidolon -> human -> phylaxd`. TESTS ONLY.
pub fn stub_gate_chain() -> Vec<Box<dyn Gate>> {
    CANONICAL_GATE_ORDER
        .into_iter()
        .map(|name| Box::new(StubGate::new(name)) as Box<dyn Gate>)
        .collect()
}

/// An executor that echoes the invocation back as its result. TESTS ONLY.
pub struct EchoExecutor;

#[async_trait]
/// Echoes invocations for test-only dispatch configurations.
impl Executor for EchoExecutor {
    /// Returns a JSON representation of the received invocation.
    async fn execute(
        &self,
        _ctx: &RequestContext,
        invocation: &ToolInvocation,
    ) -> Result<serde_json::Value, ExecutorError> {
        Ok(serde_json::json!({
            "tool": invocation.tool,
            "action": invocation.action,
            "echoed": true,
        }))
    }
}
