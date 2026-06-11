//! Fail-closed defaults for the live binary: a deny-everything gate chain and an executor that
//! refuses to run anything.
//!
//! Until the real authorities (Pistis, Plutus, Eidolon, Human, Phylax) land, the live server
//! wires [`deny_gate_chain`] so every dispatched action is denied at the first gate. This is the
//! correct Phase 0 posture for a running system: deny by default, swap in real gates by trait
//! object as each authority ships. Allow-all placeholders live in [`crate::stubs`] behind the
//! non-default `stubs` feature and never reach a release build.

use async_trait::async_trait;
use syntheos_contracts::{
    Gate, GateDecision, GateError, GateRequest, RequestContext, ToolInvocation,
};

use crate::dispatcher::CANONICAL_GATE_ORDER;
use crate::executor::{Executor, ExecutorError};

/// A gate that denies everything, reporting a fixed authority name. Stands in, fail-closed, for
/// an authority gate not yet implemented.
pub struct DenyGate {
    /// The authority name this gate reports and denies on behalf of.
    name: &'static str,
}

impl DenyGate {
    /// Create a deny-everything gate that reports `name`.
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Gate for DenyGate {
    fn name(&self) -> &str {
        self.name
    }

    async fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::Deny {
            reason: format!(
                "fail-closed: the {} authority is not yet implemented",
                self.name
            ),
        })
    }
}

/// The canonical deny-by-default gate chain, in dispatch order:
/// `pistis -> plutus -> eidolon -> human -> phylax`, every gate a [`DenyGate`].
///
/// This is what the live binary wires until real authorities land: structurally canonical (so
/// [`crate::Dispatcher::new`] accepts it) but denying every action at the first gate.
pub fn deny_gate_chain() -> Vec<Box<dyn Gate>> {
    CANONICAL_GATE_ORDER
        .into_iter()
        .map(|name| Box::new(DenyGate::new(name)) as Box<dyn Gate>)
        .collect()
}

/// An executor that refuses to run anything: the fail-closed counterpart to [`deny_gate_chain`]
/// for the live binary until real executors (Hermes, Synapse) land.
///
/// Behind [`deny_gate_chain`] it is unreachable (every action is denied before execution); it
/// exists so that even a future mis-wiring of the chain cannot silently execute an action.
pub struct DenyExecutor;

#[async_trait]
impl Executor for DenyExecutor {
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
