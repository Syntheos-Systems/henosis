//! The dispatcher itself: run the ordered gate chain, execute if allowed, and emit a lifecycle
//! event at every branch.

use std::sync::Arc;

use syntheos_axon::AxonBus;
use syntheos_contracts::{
    ActionCompleted, ActionDenied, ActionFailed, ActionInvoked, ApprovalRequired, FilterDecision,
    Gate, GateDecision, GateRequest, OutputFilter, PrincipalId, TenantId, TypedEvent,
};

use crate::error::DispatchError;
use crate::executor::Executor;
use crate::outcome::DispatchOutcome;

/// The canonical authority order every gate chain must be, exactly, in dispatch order:
/// `pistis -> plutus -> eidolon -> human -> phylax`.
///
/// [`Dispatcher::new`] validates against this by [`Gate::name`]: a runnable chain is *exactly*
/// these five authorities, each once, in this order -- no missing authority, no duplicate, no
/// reordering, and no extra non-canonical gate. The audit trail then attests precisely this set
/// ran; defense-in-depth checks (rate limiting, quotas) belong at the transport/server layer,
/// not interleaved into the authority chain.
pub const CANONICAL_GATE_ORDER: [&str; 5] = ["pistis", "plutus", "eidolon", "human", "phylax"];

/// The single chokepoint every action passes through. Holds the ordered gate chain, the
/// executor that runs an authorized action, an optional output filter, and the bus it narrates
/// lifecycle events onto.
///
/// Fail-closed BY CONSTRUCTION: [`Dispatcher::new`] is the only way to build one, and it rejects
/// an empty or non-canonical gate chain, so a runnable dispatcher always carries the full
/// canonical authority chain.
///
/// The output filter is the OUTPUT-direction counterpart to the input gate chain. It is optional
/// (default `None` = pass results through unchanged) and set additively via
/// [`Dispatcher::with_output_filter`], so the real Phase 2 policy filter wires in without any
/// change to the construction API.
///
/// Share it as `Arc<Dispatcher>`; all methods take `&self`.
pub struct Dispatcher {
    /// Gates run in order; the first non-`Allow` short-circuits.
    gates: Vec<Box<dyn Gate>>,
    /// Runs the action once every gate has allowed it.
    executor: Box<dyn Executor>,
    /// Optional redaction/transform applied to a successful result before it is returned. `None`
    /// passes the result through unchanged.
    output_filter: Option<Box<dyn OutputFilter>>,
    /// In-process bus the lifecycle events are published to.
    bus: Arc<AxonBus>,
}

/// Construction, validation, dispatch, and lifecycle emission for the canonical chain.
impl Dispatcher {
    /// Assemble a dispatcher from an ordered gate chain, an executor, and the shared bus,
    /// validating the chain before anything can dispatch through it.
    ///
    /// Rejects an empty chain ([`DispatchError::EmptyGateChain`]) and any chain that is not
    /// *exactly* [`CANONICAL_GATE_ORDER`] by [`Gate::name`] -- each authority present once, in
    /// order, with no extra non-canonical gate ([`DispatchError::NonCanonicalChain`]).
    pub fn new(
        gates: Vec<Box<dyn Gate>>,
        executor: Box<dyn Executor>,
        bus: Arc<AxonBus>,
    ) -> Result<Self, DispatchError> {
        Self::validate_chain(&gates)?;
        Ok(Self {
            gates,
            executor,
            output_filter: None,
            bus,
        })
    }

    /// Attach an output filter, applied to a successful result before it is returned.
    ///
    /// Additive and consuming: the construction/validation done by [`Dispatcher::new`] is
    /// unchanged, so wiring the real Phase 2 policy filter is not a breaking change. Calling this
    /// more than once keeps the last filter.
    pub fn with_output_filter(mut self, filter: Box<dyn OutputFilter>) -> Self {
        self.output_filter = Some(filter);
        self
    }

    /// Reject a gate chain that is empty or is not *exactly* the canonical authority set, in
    /// order, with nothing extra.
    fn validate_chain(gates: &[Box<dyn Gate>]) -> Result<(), DispatchError> {
        if gates.is_empty() {
            return Err(DispatchError::EmptyGateChain);
        }
        // Strict identity: the chain's gate names, in order, must equal CANONICAL_GATE_ORDER
        // verbatim. This rejects a missing authority, a duplicate, a reordering, AND any extra
        // non-canonical gate interleaved into the chain -- an unrecognized gate in the authority
        // set is a configuration error, not defense-in-depth, and is fail-closed rejected so the
        // audit trail attests exactly the canonical authorities ran.
        let names: Vec<&str> = gates.iter().map(|g| g.name()).collect();
        if names != CANONICAL_GATE_ORDER {
            return Err(DispatchError::NonCanonicalChain {
                expected: CANONICAL_GATE_ORDER.iter().map(|s| s.to_string()).collect(),
                got: names.iter().map(|s| s.to_string()).collect(),
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
    /// executor is never called. An unrecognized (`#[non_exhaustive]`) gate decision, and a gate
    /// that returns `Err` (could not decide), are both treated as a denial -- fail-closed. On
    /// full approval the executor runs; its result is passed through the output filter (if one is
    /// wired) and rides [`DispatchOutcome::Executed`], and an execution failure becomes
    /// [`DispatchError`]. A lifecycle event is emitted at every branch (best-effort; a publish
    /// failure is logged, never fatal).
    pub async fn dispatch(&self, request: GateRequest) -> Result<DispatchOutcome, DispatchError> {
        let tenant = request.context.tenant;
        let principal = request.context.principal;
        let tool = request.invocation.tool.clone();
        let action = request.invocation.action.clone();
        let task_id = request.context.task.as_ref().map(|task| task.id);

        self.emit(
            &ActionInvoked {
                tool: tool.clone(),
                action: action.clone(),
                task_id,
            },
            tenant,
            principal,
        );

        for gate in &self.gates {
            match gate.check(&request).await {
                Ok(GateDecision::Allow) => continue,
                Ok(GateDecision::Deny { reason }) => {
                    let gate = gate.name().to_string();
                    self.emit(
                        &ActionDenied {
                            tool: tool.clone(),
                            action: action.clone(),
                            gate: gate.clone(),
                            reason: reason.clone(),
                            task_id,
                        },
                        tenant,
                        principal,
                    );
                    return Ok(DispatchOutcome::Denied { gate, reason });
                }
                Ok(GateDecision::RequireApproval { prompt }) => {
                    let gate = gate.name().to_string();
                    self.emit(
                        &ApprovalRequired {
                            tool: tool.clone(),
                            action: action.clone(),
                            gate: gate.clone(),
                            prompt: prompt.clone(),
                            task_id,
                        },
                        tenant,
                        principal,
                    );
                    return Ok(DispatchOutcome::RequiresApproval { gate, prompt });
                }
                // `GateDecision` is `#[non_exhaustive]`: an unrecognized decision is fail-closed.
                Ok(_) => {
                    let gate = gate.name().to_string();
                    let reason = "unrecognized gate decision (fail-closed)".to_string();
                    self.emit(
                        &ActionDenied {
                            tool: tool.clone(),
                            action: action.clone(),
                            gate: gate.clone(),
                            reason: reason.clone(),
                            task_id,
                        },
                        tenant,
                        principal,
                    );
                    return Ok(DispatchOutcome::Denied { gate, reason });
                }
                // The gate could not reach a decision (authority unavailable, dependency failed,
                // malformed request). Fail-closed: deny, attributed to this gate.
                Err(err) => {
                    let gate = gate.name().to_string();
                    let reason = format!("gate error (fail-closed): {err}");
                    self.emit(
                        &ActionDenied {
                            tool: tool.clone(),
                            action: action.clone(),
                            gate: gate.clone(),
                            reason: reason.clone(),
                            task_id,
                        },
                        tenant,
                        principal,
                    );
                    return Ok(DispatchOutcome::Denied { gate, reason });
                }
            }
        }

        match self
            .executor
            .execute(&request.context, &request.invocation)
            .await
        {
            Ok(mut result) => {
                // Output-filter seam: redact/transform the result after execution. No filter wired
                // = pass-through (the Phase 0 default). The real policy filter (the EidolonGate
                // output side) lands in Phase 2 as an OutputFilter impl set via with_output_filter.
                if let Some(filter) = &self.output_filter {
                    match filter.filter(&mut result, &request.context).await {
                        FilterDecision::Pass => {}
                        FilterDecision::Replace(value) => result = value,
                        FilterDecision::Redact { reason } => {
                            result = serde_json::json!({ "redacted": true, "reason": reason });
                        }
                        // `FilterDecision` is `#[non_exhaustive]`: an unrecognized decision
                        // withholds the result -- fail-closed, so output is never leaked unfiltered.
                        _ => {
                            result = serde_json::json!({
                                "redacted": true,
                                "reason": "unrecognized filter decision (fail-closed)",
                            });
                        }
                    }
                }
                self.emit(
                    &ActionCompleted {
                        tool,
                        action,
                        task_id,
                    },
                    tenant,
                    principal,
                );
                Ok(DispatchOutcome::Executed { result })
            }
            Err(err) => {
                self.emit(
                    &ActionFailed {
                        tool,
                        action,
                        error: err.to_string(),
                        task_id,
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
/// Tests for canonical ordering, fail-closed behavior, output filtering, and events.
mod tests {
    use super::*;
    use crate::deny::deny_gate_chain;
    use crate::executor::ExecutorError;
    use crate::stubs::{stub_gate_chain, EchoExecutor, StubGate};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use syntheos_contracts::{AxonEnvelope, GateError, RequestContext, ToolInvocation};

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
    /// Gate implementation that returns a fixed denial.
    impl Gate for DenyGate {
        /// Return this test gate's canonical authority name.
        fn name(&self) -> &str {
            self.name
        }
        /// Deny every request with the configured reason.
        async fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::Deny {
                reason: self.reason.to_string(),
            })
        }
    }

    /// A gate that always escalates for approval.
    struct ApprovalGate {
        name: &'static str,
    }
    #[async_trait]
    /// Gate implementation that always requires human approval.
    impl Gate for ApprovalGate {
        /// Return this test gate's canonical authority name.
        fn name(&self) -> &str {
            self.name
        }
        /// Escalate every request with a fixed prompt.
        async fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::RequireApproval {
                prompt: "approve?".to_string(),
            })
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
    /// Gate implementation that records execution before returning its decision.
    impl Gate for RecordingGate {
        /// Return this test gate's canonical authority name.
        fn name(&self) -> &str {
            self.name
        }
        /// Record the gate visit and return the configured decision.
        async fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            self.log.lock().unwrap().push(self.name.to_string());
            Ok(self.decision.clone())
        }
    }

    /// An executor that counts how many times it ran (to prove it is skipped on deny).
    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    /// Executor implementation that counts authorized executions.
    impl Executor for CountingExecutor {
        /// Count the execution and return a deterministic result.
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
    /// Executor implementation that returns a fixed failure.
    impl Executor for FailingExecutor {
        /// Fail every attempted execution.
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
    /// A fully allowing canonical chain reaches the executor and emits completion.
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
    /// A denial short-circuits later gates and the executor.
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
    /// An approval requirement short-circuits execution and emits escalation.
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
    /// Executor failures propagate and emit the failed lifecycle event.
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
    /// Gates run in canonical order and stop at the first denial.
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
        let dispatcher =
            Dispatcher::new(gates, Box::new(EchoExecutor), bus.clone()).expect("canonical chain");

        let outcome = dispatcher
            .dispatch(request("kleos", "x"))
            .await
            .expect("dispatch");
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
    /// Typed action subscribers receive dispatcher lifecycle events.
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
                task_id: None,
            }
        );
    }

    #[test]
    /// Gate-name introspection preserves the canonical authority order.
    fn gate_names_reports_canonical_order() {
        let bus = Arc::new(AxonBus::new());
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus)
            .expect("canonical chain");
        assert_eq!(
            dispatcher.gate_names(),
            ["pistis", "plutus", "eidolon", "human", "phylax"]
        );
    }

    /// An output filter that always withholds the result with a fixed reason.
    struct RedactFilter {
        reason: &'static str,
    }
    #[async_trait]
    /// Output filter implementation that always withholds the result.
    impl OutputFilter for RedactFilter {
        /// Return the filter's stable name.
        fn name(&self) -> &str {
            "redact"
        }
        /// Replace the output with a redaction decision.
        async fn filter(
            &self,
            _result: &mut serde_json::Value,
            _ctx: &RequestContext,
        ) -> FilterDecision {
            FilterDecision::Redact {
                reason: self.reason.to_string(),
            }
        }
    }

    /// An output filter that replaces the result wholesale with a fixed value.
    struct ReplaceFilter {
        value: serde_json::Value,
    }
    #[async_trait]
    /// Output filter implementation that replaces the whole result.
    impl OutputFilter for ReplaceFilter {
        /// Return the filter's stable name.
        fn name(&self) -> &str {
            "replace"
        }
        /// Return the configured replacement value.
        async fn filter(
            &self,
            _result: &mut serde_json::Value,
            _ctx: &RequestContext,
        ) -> FilterDecision {
            FilterDecision::Replace(self.value.clone())
        }
    }

    /// An output filter that scrubs a top-level field in place, then passes the rest through.
    struct ScrubFilter {
        field: &'static str,
    }
    #[async_trait]
    /// Output filter implementation that removes one field in place.
    impl OutputFilter for ScrubFilter {
        /// Return the filter's stable name.
        fn name(&self) -> &str {
            "scrub"
        }
        /// Remove the configured field and pass the remaining output.
        async fn filter(
            &self,
            result: &mut serde_json::Value,
            _ctx: &RequestContext,
        ) -> FilterDecision {
            if let Some(obj) = result.as_object_mut() {
                obj.remove(self.field);
            }
            FilterDecision::Pass
        }
    }

    #[tokio::test]
    /// Redaction filters withhold the original executor result.
    async fn output_filter_redacts_result() {
        let bus = Arc::new(AxonBus::new());
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone())
            .expect("canonical chain")
            .with_output_filter(Box::new(RedactFilter { reason: "pii" }));

        let outcome = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        match outcome {
            DispatchOutcome::Executed { result } => {
                assert_eq!(
                    result,
                    serde_json::json!({ "redacted": true, "reason": "pii" })
                );
            }
            other => panic!("expected Executed, got {other:?}"),
        }
    }

    #[tokio::test]
    /// Replacement filters substitute their configured output.
    async fn output_filter_replaces_result() {
        let bus = Arc::new(AxonBus::new());
        let replacement = serde_json::json!({ "minimised": true });
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone())
            .expect("canonical chain")
            .with_output_filter(Box::new(ReplaceFilter {
                value: replacement.clone(),
            }));

        let outcome = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        match outcome {
            DispatchOutcome::Executed { result } => assert_eq!(result, replacement),
            other => panic!("expected Executed, got {other:?}"),
        }
    }

    #[tokio::test]
    /// In-place scrub filters preserve the non-sensitive fields.
    async fn output_filter_passes_after_scrubbing_in_place() {
        let bus = Arc::new(AxonBus::new());
        // EchoExecutor returns { tool, action, echoed }; scrub the `echoed` field, pass the rest.
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone())
            .expect("canonical chain")
            .with_output_filter(Box::new(ScrubFilter { field: "echoed" }));

        let outcome = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        match outcome {
            DispatchOutcome::Executed { result } => {
                assert_eq!(result["tool"], serde_json::json!("kleos"));
                assert!(result.get("echoed").is_none(), "echoed must be scrubbed");
            }
            other => panic!("expected Executed, got {other:?}"),
        }
    }

    #[tokio::test]
    /// A dispatcher without an output filter returns the executor result.
    async fn no_output_filter_passes_result_through() {
        let bus = Arc::new(AxonBus::new());
        // Default dispatcher (no filter) returns the executor result verbatim.
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(EchoExecutor), bus.clone())
            .expect("canonical chain");

        let outcome = dispatcher
            .dispatch(request("kleos", "memory_store"))
            .await
            .expect("dispatch");
        match outcome {
            DispatchOutcome::Executed { result } => {
                assert_eq!(result["echoed"], serde_json::json!(true));
            }
            other => panic!("expected Executed, got {other:?}"),
        }
    }

    #[test]
    /// An empty authority chain is rejected at construction.
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
    /// A chain missing one canonical authority is rejected.
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
    /// A chain with canonical authorities in the wrong order is rejected.
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
    /// A chain containing a duplicate authority is rejected.
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
    /// A chain containing an extra non-authority gate is rejected.
    fn extra_non_canonical_gate_rejected() {
        let bus = Arc::new(AxonBus::new());
        // An extra gate interleaved into the canonical five is rejected: the authority chain must
        // be EXACTLY canonical, so the audit trail attests precisely which authorities ran.
        // Defense-in-depth (e.g. rate limiting) belongs at the transport layer, not here.
        let gates: Vec<Box<dyn Gate>> = [
            "pistis",
            "plutus",
            "ratelimit",
            "eidolon",
            "human",
            "phylax",
        ]
        .into_iter()
        .map(|name| Box::new(StubGate::new(name)) as Box<dyn Gate>)
        .collect();
        let err = Dispatcher::new(gates, Box::new(EchoExecutor), bus)
            .err()
            .expect("an extra non-canonical gate must be rejected");
        match err {
            DispatchError::NonCanonicalChain { expected, got } => {
                assert_eq!(expected, CANONICAL_GATE_ORDER);
                assert_eq!(
                    got,
                    [
                        "pistis",
                        "plutus",
                        "ratelimit",
                        "eidolon",
                        "human",
                        "phylax"
                    ]
                );
            }
            other => panic!("expected NonCanonicalChain, got {other:?}"),
        }
    }

    /// A gate that cannot reach a decision -- it always errors.
    struct ErroringGate {
        name: &'static str,
    }
    #[async_trait]
    /// Gate implementation that simulates an unreachable authority.
    impl Gate for ErroringGate {
        /// Return this test gate's canonical authority name.
        fn name(&self) -> &str {
            self.name
        }
        /// Fail every policy check.
        async fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Err(GateError::new("authority unreachable"))
        }
    }

    #[tokio::test]
    /// Gate errors become fail-closed denials and never execute.
    async fn gate_error_denies_fail_closed() {
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe("action");
        let calls = Arc::new(AtomicUsize::new(0));
        // pistis allows, then plutus errors: a gate that cannot decide must deny (fail-closed),
        // the executor must never run, and the rest of the chain must not run either.
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(StubGate::new("pistis")),
            Box::new(ErroringGate { name: "plutus" }),
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
        match outcome {
            DispatchOutcome::Denied { gate, reason } => {
                assert_eq!(gate, "plutus", "denial is attributed to the erroring gate");
                assert!(reason.contains("fail-closed"), "reason: {reason}");
                assert!(reason.contains("authority unreachable"), "reason: {reason}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "executor must not run when a gate errors"
        );
        assert_eq!(drain_kinds(&mut rx), ["action.invoked", "action.denied"]);
    }

    #[tokio::test]
    /// The explicit deny chain rejects before reaching its executor.
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
