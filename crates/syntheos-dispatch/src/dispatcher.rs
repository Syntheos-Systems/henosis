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

/// The canonical authority order every gate chain must contain, in dispatch order:
/// `pistis -> plutus -> eidolon -> human -> phylax`.
///
/// [`Dispatcher::new`] validates against this by [`Gate::name`]; chains missing an authority,
/// duplicating one, or reordering them are rejected at construction.
pub const CANONICAL_GATE_ORDER: [&str; 5] = ["pistis", "plutus", "eidolon", "human", "phylax"];

/// The single chokepoint every action passes through. Holds the ordered gate chain, the
/// executor that runs an authorized action, and the bus it narrates lifecycle events onto.
///
/// Fail-closed BY CONSTRUCTION: [`Dispatcher::new`] is the only way to build one, and it rejects
/// an empty or non-canonical gate chain, so a runnable dispatcher always carries the full
/// canonical authority chain.
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
    /// Assemble a dispatcher from an ordered gate chain, an executor, and the shared bus,
    /// validating the chain before anything can dispatch through it.
    ///
    /// Rejects an empty chain ([`DispatchError::EmptyGateChain`]) and any chain whose canonical
    /// authorities (matched on [`Gate::name`]) are not exactly [`CANONICAL_GATE_ORDER`] -- each
    /// present once, in canonical relative order ([`DispatchError::NonCanonicalChain`]).
    /// Additional non-canonical gates may be interleaved anywhere in the chain.
    pub fn new(
        gates: Vec<Box<dyn Gate>>,
        executor: Box<dyn Executor>,
        bus: Arc<AxonBus>,
    ) -> Result<Self, DispatchError> {
        Self::validate_chain(&gates)?;
        Ok(Self {
            gates,
            executor,
            bus,
        })
    }

    /// Reject a gate chain that is empty or whose canonical authorities are missing,
    /// duplicated, or out of canonical order.
    fn validate_chain(gates: &[Box<dyn Gate>]) -> Result<(), DispatchError> {
        if gates.is_empty() {
            return Err(DispatchError::EmptyGateChain);
        }
        // The canonical authorities, in the order this chain presents them. Each must appear
        // exactly once and in canonical relative order; anything else is fail-closed rejected.
        let canonical_in_chain: Vec<&str> = gates
            .iter()
            .map(|g| g.name())
            .filter(|name| CANONICAL_GATE_ORDER.contains(name))
            .collect();
        if canonical_in_chain != CANONICAL_GATE_ORDER {
            return Err(DispatchError::NonCanonicalChain {
                expected: CANONICAL_GATE_ORDER.iter().map(|s| s.to_string()).collect(),
                got: gates.iter().map(|g| g.name().to_string()).collect(),
            });
        }
        Ok(())
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
    use crate::deny::deny_gate_chain;
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
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone())
            .expect("canonical chain");

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
        // pistis stub allows, then plutus denies; the rest of the canonical chain must never run.
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(StubGate::new("pistis")),
            Box::new(DenyGate {
                name: "plutus",
                reason: "over quota",
            }),
            Box::new(StubGate::new("eidolon")),
            Box::new(StubGate::new("human")),
            Box::new(StubGate::new("phylax")),
        ];
        let dispatcher = Dispatcher::new(
            gates,
            Box::new(CountingExecutor {
                calls: calls.clone(),
            }),
            bus.clone(),
        )
        .expect("canonical chain");

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
        // Canonical chain where the human authority escalates instead of allowing.
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(StubGate::new("pistis")),
            Box::new(StubGate::new("plutus")),
            Box::new(StubGate::new("eidolon")),
            Box::new(ApprovalGate { name: "human" }),
            Box::new(StubGate::new("phylax")),
        ];
        let dispatcher = Dispatcher::new(
            gates,
            Box::new(CountingExecutor {
                calls: calls.clone(),
            }),
            bus.clone(),
        )
        .expect("canonical chain");

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
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(FailingExecutor), bus.clone())
            .expect("canonical chain");

        let err = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect_err("should fail");
        match err {
            DispatchError::Execution(e) => assert_eq!(e.message, "boom"),
            other => panic!("expected Execution error, got {other:?}"),
        }
        assert_eq!(drain_kinds(&mut rx), ["action.invoked", "action.failed"]);
    }

    #[tokio::test]
    async fn gate_order_and_short_circuit() {
        let bus = Arc::new(AxonBus::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        // Full canonical chain of recording gates; plutus denies, so eidolon/human/phylax must
        // never run.
        let gates: Vec<Box<dyn Gate>> = CANONICAL_GATE_ORDER
            .into_iter()
            .map(|name| {
                let decision = if name == "plutus" {
                    GateDecision::Deny {
                        reason: "no".into(),
                    }
                } else {
                    GateDecision::Allow
                };
                Box::new(RecordingGate {
                    name,
                    decision,
                    log: log.clone(),
                }) as Box<dyn Gate>
            })
            .collect();
        let dispatcher = Dispatcher::new(gates, Box::new(EchoExecutor), bus.clone())
            .expect("canonical chain");

        let outcome = dispatcher.dispatch(request("kleos", "x")).await.expect("dispatch");
        assert_eq!(
            outcome,
            DispatchOutcome::Denied {
                gate: "plutus".into(),
                reason: "no".into(),
            }
        );
        assert_eq!(
            *log.lock().unwrap(),
            ["pistis", "plutus"],
            "gates after a short-circuit must not run"
        );
    }

    #[tokio::test]
    async fn typed_event_emission() {
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe_typed::<ActionInvoked>();
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone())
            .expect("canonical chain");

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
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus)
            .expect("canonical chain");
        assert_eq!(
            dispatcher.gate_names(),
            ["pistis", "plutus", "eidolon", "human", "phylax"]
        );
    }

    #[test]
    fn empty_chain_rejected() {
        let bus = Arc::new(AxonBus::new());
        let err = Dispatcher::new(Vec::new(), Box::new(EchoExecutor), bus)
            .err()
            .expect("empty chain must be rejected");
        assert!(
            matches!(err, DispatchError::EmptyGateChain),
            "expected EmptyGateChain, got {err:?}"
        );
    }

    #[test]
    fn incomplete_chain_rejected() {
        let bus = Arc::new(AxonBus::new());
        // Canonical chain minus the human authority: incomplete, must be rejected.
        let gates: Vec<Box<dyn Gate>> = ["pistis", "plutus", "eidolon", "phylax"]
            .into_iter()
            .map(|name| Box::new(StubGate::new(name)) as Box<dyn Gate>)
            .collect();
        let err = Dispatcher::new(gates, Box::new(EchoExecutor), bus)
            .err()
            .expect("incomplete chain must be rejected");
        match err {
            DispatchError::NonCanonicalChain { expected, got } => {
                assert_eq!(expected, CANONICAL_GATE_ORDER);
                assert_eq!(got, ["pistis", "plutus", "eidolon", "phylax"]);
            }
            other => panic!("expected NonCanonicalChain, got {other:?}"),
        }
    }

    #[test]
    fn misordered_chain_rejected() {
        let bus = Arc::new(AxonBus::new());
        // All five authorities present, but phylax before human: order violation, rejected.
        let gates: Vec<Box<dyn Gate>> = ["pistis", "plutus", "eidolon", "phylax", "human"]
            .into_iter()
            .map(|name| Box::new(StubGate::new(name)) as Box<dyn Gate>)
            .collect();
        let err = Dispatcher::new(gates, Box::new(EchoExecutor), bus)
            .err()
            .expect("misordered chain must be rejected");
        assert!(
            matches!(err, DispatchError::NonCanonicalChain { .. }),
            "expected NonCanonicalChain, got {err:?}"
        );
    }

    #[test]
    fn duplicate_authority_rejected() {
        let bus = Arc::new(AxonBus::new());
        // A duplicated canonical authority is rejected even with all five present in order.
        let gates: Vec<Box<dyn Gate>> =
            ["pistis", "pistis", "plutus", "eidolon", "human", "phylax"]
                .into_iter()
                .map(|name| Box::new(StubGate::new(name)) as Box<dyn Gate>)
                .collect();
        let err = Dispatcher::new(gates, Box::new(EchoExecutor), bus)
            .err()
            .expect("duplicate authority must be rejected");
        assert!(
            matches!(err, DispatchError::NonCanonicalChain { .. }),
            "expected NonCanonicalChain, got {err:?}"
        );
    }

    #[test]
    fn extra_non_canonical_gates_allowed() {
        let bus = Arc::new(AxonBus::new());
        // Defense-in-depth gates may interleave as long as the canonical five stay in order.
        let gates: Vec<Box<dyn Gate>> =
            ["pistis", "plutus", "ratelimit", "eidolon", "human", "phylax"]
                .into_iter()
                .map(|name| Box::new(StubGate::new(name)) as Box<dyn Gate>)
                .collect();
        let dispatcher = Dispatcher::new(gates, Box::new(EchoExecutor), bus)
            .expect("interleaved extras are valid");
        assert_eq!(
            dispatcher.gate_names(),
            ["pistis", "plutus", "ratelimit", "eidolon", "human", "phylax"]
        );
    }

    #[tokio::test]
    async fn deny_chain_denies_and_never_executes() {
        let bus = Arc::new(AxonBus::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatcher = Dispatcher::new(
            deny_gate_chain(),
            Box::new(CountingExecutor {
                calls: calls.clone(),
            }),
            bus.clone(),
        )
        .expect("deny chain is canonical");

        let outcome = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        match outcome {
            DispatchOutcome::Denied { gate, reason } => {
                assert_eq!(gate, "pistis", "first gate in the chain must deny");
                assert!(reason.contains("fail-closed"), "reason: {reason}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "executor must never run behind the deny chain"
        );
    }
}
