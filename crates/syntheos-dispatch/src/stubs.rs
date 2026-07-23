//! Allow-all placeholder gates and executor, for TESTS ONLY.
//!
//! This module is feature-gated behind the non-default `stubs` cargo feature (and this crate's
//! own `cfg(test)`), so the allow-all chain can never compile into a default or release build.
//! The live binary's fail-closed defaults live in [`crate::deny`]. Real authorities (Pistis,
//! Plutus, Eidolon, Human, phylaxd) and real executors (Hermes, Synapse) replace both by trait
//! object as they land.

use async_trait::async_trait;
use syntheos_contracts::{
    Gate, GateDecision, GateError, GateRequest, RequestContext, ToolInvocation,
};

use crate::dispatcher::CANONICAL_GATE_ORDER;
use crate::executor::{Executor, ExecutorError};

/// A gate that allows everything, reporting a fixed name. Stands in for an authority gate not
/// yet wired in.
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
/// `pistis -> plutus -> eidolon -> human -> phylaxd`. TESTS ONLY -- the live default is
/// [`crate::deny::deny_gate_chain`].
pub fn stub_gate_chain() -> Vec<Box<dyn Gate>> {
    CANONICAL_GATE_ORDER
        .into_iter()
        .map(|name| Box::new(StubGate::new(name)) as Box<dyn Gate>)
        .collect()
}

/// An executor that echoes the invocation back as its result. TESTS ONLY -- placeholder until
/// real executors are wired in.
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
