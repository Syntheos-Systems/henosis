//! The dispatcher itself: run the ordered gate chain, execute if allowed, and emit a lifecycle
//! event at every branch.

use std::sync::Arc;

use syntheos_axon::AxonBus;
use syntheos_contracts::{
    ActionCompleted, ActionDenied, ActionFailed, ActionInvoked, ApprovalRequired, Gate,
    GateDecision, GateRequest, PrincipalId, TenantId, TypedEvent,
};

use crate::error::DispatchError;
use crate::executor::Executor;
use crate::outcome::DispatchOutcome;

/// The single chokepoint every action passes through. Holds the ordered gate chain, the
/// executor that runs an authorized action, and the bus it narrates lifecycle events onto.
///
/// Share it as `Arc<Dispatcher>`; all methods take `&self`.
pub struct Dispatcher {
    /// Gates run in order; the first non-`Allow` short-circuits.
    gates: Vec<Box<dyn Gate>>,
    /// Runs the action once every gate has allowed it.
    executor: Box<dyn Executor>,
    /// In-process bus the lifecycle events are published to.
    bus: Arc<AxonBus>,
}

impl Dispatcher {
    /// Assemble a dispatcher from an ordered gate chain, an executor, and the shared bus.
    pub fn new(gates: Vec<Box<dyn Gate>>, executor: Box<dyn Executor>, bus: Arc<AxonBus>) -> Self {
        Self {
            gates,
            executor,
            bus,
        }
    }

    /// The names of the gates in dispatch order (for logging and introspection).
    pub fn gate_names(&self) -> Vec<&str> {
        self.gates.iter().map(|g| g.name()).collect()
    }

    /// Authorize and (if allowed) execute a single request.
    ///
    /// Runs the gate chain in order; the first `Deny`/`RequireApproval` short-circuits and the
    /// executor is never called. An unrecognized (`#[non_exhaustive]`) gate decision is treated
    /// as a denial -- fail-closed. On full approval the executor runs; its result rides
    /// [`DispatchOutcome::Executed`], and an execution failure becomes [`DispatchError`]. A
    /// lifecycle event is emitted at every branch (best-effort; a publish failure is logged,
    /// never fatal).
    pub async fn dispatch(&self, request: GateRequest) -> Result<DispatchOutcome, DispatchError> {
        let tenant = request.context.tenant;
        let principal = request.context.principal;
        let tool = request.invocation.tool.clone();
        let action = request.invocation.action.clone();

        self.emit(
            &ActionInvoked {
                tool: tool.clone(),
                action: action.clone(),
            },
            tenant,
            principal,
        );

        for gate in &self.gates {
            match gate.check(&request).await {
                GateDecision::Allow => continue,
                GateDecision::Deny { reason } => {
                    let gate = gate.name().to_string();
                    self.emit(
                        &ActionDenied {
                            tool: tool.clone(),
                            action: action.clone(),
                            gate: gate.clone(),
                            reason: reason.clone(),
                        },
                        tenant,
                        principal,
                    );
                    return Ok(DispatchOutcome::Denied { gate, reason });
                }
                GateDecision::RequireApproval { prompt } => {
                    let gate = gate.name().to_string();
                    self.emit(
                        &ApprovalRequired {
                            tool: tool.clone(),
                            action: action.clone(),
                            gate: gate.clone(),
                            prompt: prompt.clone(),
                        },
                        tenant,
                        principal,
                    );
                    return Ok(DispatchOutcome::RequiresApproval { gate, prompt });
                }
                // `GateDecision` is `#[non_exhaustive]`: an unrecognized decision is fail-closed.
                _ => {
                    let gate = gate.name().to_string();
                    let reason = "unrecognized gate decision (fail-closed)".to_string();
                    self.emit(
                        &ActionDenied {
                            tool: tool.clone(),
                            action: action.clone(),
                            gate: gate.clone(),
                            reason: reason.clone(),
                        },
                        tenant,
                        principal,
                    );
                    return Ok(DispatchOutcome::Denied { gate, reason });
                }
            }
        }

        match self.executor.execute(&request.context, &request.invocation).await {
            Ok(result) => {
                self.emit(&ActionCompleted { tool, action }, tenant, principal);
                Ok(DispatchOutcome::Executed { result })
            }
            Err(err) => {
                self.emit(
                    &ActionFailed {
                        tool,
                        action,
                        error: err.to_string(),
                    },
                    tenant,
                    principal,
                );
                Err(DispatchError::Execution(err))
            }
        }
    }

    /// Publish a lifecycle event, best-effort. A publish failure is logged and swallowed --
    /// telemetry must never change an action's outcome.
    fn emit<E: TypedEvent>(&self, event: &E, tenant: TenantId, principal: PrincipalId) {
        if let Err(err) = self.bus.publish_event(event, tenant, principal) {
            tracing::warn!(error = %err, kind = E::KIND, "failed to publish dispatch lifecycle event");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ExecutorError;
    use crate::stubs::{stub_gate_chain, EchoExecutor, StubGate};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use syntheos_contracts::{AxonEnvelope, RequestContext, ToolInvocation};

    /// Build a minimal request for `tool`/`action`.
    fn request(tool: &str, action: &str) -> GateRequest {
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
                tool: tool.to_string(),
                action: action.to_string(),
                args: serde_json::json!({}),
            },
        }
    }

    /// A gate that always denies with a fixed reason.
    struct DenyGate {
        name: &'static str,
        reason: &'static str,
    }
    #[async_trait]
    impl Gate for DenyGate {
        fn name(&self) -> &str {
            self.name
        }
        async fn check(&self, _req: &GateRequest) -> GateDecision {
            GateDecision::Deny {
                reason: self.reason.to_string(),
            }
        }
    }

    /// A gate that always escalates for approval.
    struct ApprovalGate {
        name: &'static str,
    }
    #[async_trait]
    impl Gate for ApprovalGate {
        fn name(&self) -> &str {
            self.name
        }
        async fn check(&self, _req: &GateRequest) -> GateDecision {
            GateDecision::RequireApproval {
                prompt: "approve?".to_string(),
            }
        }
    }

    /// A gate that records that it ran (to prove order + short-circuit), then returns a fixed
    /// decision.
    struct RecordingGate {
        name: &'static str,
        decision: GateDecision,
        log: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl Gate for RecordingGate {
        fn name(&self) -> &str {
            self.name
        }
        async fn check(&self, _req: &GateRequest) -> GateDecision {
            self.log.lock().unwrap().push(self.name.to_string());
            self.decision.clone()
        }
    }

    /// An executor that counts how many times it ran (to prove it is skipped on deny).
    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Executor for CountingExecutor {
        async fn execute(
            &self,
            _ctx: &RequestContext,
            _inv: &ToolInvocation,
        ) -> Result<serde_json::Value, ExecutorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "ran": true }))
        }
    }

    /// An executor that always fails.
    struct FailingExecutor;
    #[async_trait]
    impl Executor for FailingExecutor {
        async fn execute(
            &self,
            _ctx: &RequestContext,
            _inv: &ToolInvocation,
        ) -> Result<serde_json::Value, ExecutorError> {
            Err(ExecutorError::new("boom"))
        }
    }

    /// Drain all currently-buffered envelopes from a raw subscriber into their kind strings.
    fn drain_kinds(rx: &mut tokio::sync::broadcast::Receiver<AxonEnvelope>) -> Vec<String> {
        let mut kinds = Vec::new();
        while let Ok(env) = rx.try_recv() {
            kinds.push(env.kind);
        }
        kinds
    }

    #[tokio::test]
    async fn allow_chain_executes() {
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe("action");
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone());

        let outcome = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        match outcome {
            DispatchOutcome::Executed { result } => {
                assert_eq!(result["echoed"], serde_json::json!(true));
                assert_eq!(result["tool"], serde_json::json!("kleos"));
            }
            other => panic!("expected Executed, got {other:?}"),
        }
        assert_eq!(drain_kinds(&mut rx), ["action.invoked", "action.completed"]);
    }

    #[tokio::test]
    async fn deny_short_circuits() {
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe("action");
        let calls = Arc::new(AtomicUsize::new(0));
        // pistis stub allows, then a deny gate, then phylax stub (which must never run).
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(StubGate::new("pistis")),
            Box::new(DenyGate {
                name: "plutus",
                reason: "over quota",
            }),
            Box::new(StubGate::new("phylax")),
        ];
        let dispatcher = Dispatcher::new(
            gates,
            Box::new(CountingExecutor {
                calls: calls.clone(),
            }),
            bus.clone(),
        );

        let outcome = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        assert_eq!(
            outcome,
            DispatchOutcome::Denied {
                gate: "plutus".into(),
                reason: "over quota".into(),
            }
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "executor must not run on deny"
        );
        assert_eq!(drain_kinds(&mut rx), ["action.invoked", "action.denied"]);
    }

    #[tokio::test]
    async fn approval_short_circuits() {
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe("action");
        let calls = Arc::new(AtomicUsize::new(0));
        let gates: Vec<Box<dyn Gate>> = vec![Box::new(ApprovalGate { name: "human" })];
        let dispatcher = Dispatcher::new(
            gates,
            Box::new(CountingExecutor {
                calls: calls.clone(),
            }),
            bus.clone(),
        );

        let outcome = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        assert_eq!(
            outcome,
            DispatchOutcome::RequiresApproval {
                gate: "human".into(),
                prompt: "approve?".into(),
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            drain_kinds(&mut rx),
            ["action.invoked", "action.approval_required"]
        );
    }

    #[tokio::test]
    async fn execution_failure_propagates() {
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe("action");
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(FailingExecutor), bus.clone());

        let err = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect_err("should fail");
        match err {
            DispatchError::Execution(e) => assert_eq!(e.message, "boom"),
        }
        assert_eq!(drain_kinds(&mut rx), ["action.invoked", "action.failed"]);
    }

    #[tokio::test]
    async fn gate_order_and_short_circuit() {
        let bus = Arc::new(AxonBus::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(RecordingGate {
                name: "first",
                decision: GateDecision::Allow,
                log: log.clone(),
            }),
            Box::new(RecordingGate {
                name: "second",
                decision: GateDecision::Deny {
                    reason: "no".into(),
                },
                log: log.clone(),
            }),
            Box::new(RecordingGate {
                name: "third",
                decision: GateDecision::Allow,
                log: log.clone(),
            }),
        ];
        let dispatcher = Dispatcher::new(gates, Box::new(EchoExecutor), bus.clone());

        let outcome = dispatcher.dispatch(request("kleos", "x")).await.expect("dispatch");
        assert_eq!(
            outcome,
            DispatchOutcome::Denied {
                gate: "second".into(),
                reason: "no".into(),
            }
        );
        assert_eq!(
            *log.lock().unwrap(),
            ["first", "second"],
            "third gate must not run after a short-circuit"
        );
    }

    #[tokio::test]
    async fn typed_event_emission() {
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe_typed::<ActionInvoked>();
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone());

        dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        let got = rx.recv().await.expect("receives ActionInvoked");
        assert_eq!(
            got,
            ActionInvoked {
                tool: "kleos".into(),
                action: "memory_store".into(),
            }
        );
    }

    #[test]
    fn gate_names_reports_canonical_order() {
        let bus = Arc::new(AxonBus::new());
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus);
        assert_eq!(
            dispatcher.gate_names(),
            ["pistis", "plutus", "eidolon", "human", "phylax"]
        );
    }
}
