//! `PlutusGate`: the real gate authority for the plutus dispatcher slot.
//!
//! Implements `syntheos_contracts::Gate` with a four-step fail-closed pipeline:
//!  1. Org must exist and be active.
//!  2. Principal must be a member whose role permits the invocation's action class.
//!  3. Hard daily quota for the action's dimension must not be exceeded.
//!  4. Per-org token-bucket rate limit must not be exhausted.
//!
//! FAIL-CLOSED INVARIANT: no code path in `check` returns `GateDecision::Allow` when
//! any authority call errored or its result is ambiguous. Every `?`-propagated error is
//! converted to `Err(GateError)` by the `.map_err(|e| GateError::new(e.to_string()))` chain;
//! every `None` (missing org / not a member) returns `GateDecision::Deny`. There is no
//! fallback `_ => Allow` arm anywhere in this file.

use std::sync::Arc;

use async_trait::async_trait;
use syntheos_contracts::{Gate, GateDecision, GateError, GateRequest};

use crate::action_map::map_invocation;
use crate::backend::{OrgStatus, PolicyBackend};
use crate::rbac::can;

/// An injectable clock abstraction so the rate-limit check is deterministic in tests.
///
/// Production uses [`WallClock`]; tests use [`FrozenClock`].
pub trait Clock: Send + Sync {
    /// Return the current UTC instant.
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc>;

    /// Return today's date as a `YYYY-MM-DD` string in UTC.
    ///
    /// Used as the `day` key in the `usage_counter` table.
    fn today_utc(&self) -> String;
}

/// The real wall clock: delegates to `chrono::Utc::now()`.
pub struct WallClock;

/// `Clock` implementation using the system wall clock.
impl Clock for WallClock {
    /// Return the current UTC instant from the system clock.
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    /// Return today's UTC date as `YYYY-MM-DD`.
    fn today_utc(&self) -> String {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    }
}

/// A frozen clock for deterministic tests: always returns the same instant.
///
/// Available under `#[cfg(any(test, feature = "test-helpers"))]` so external
/// test modules (e.g. syntheos-server) can inject a predictable time without
/// depending on the system clock.
#[cfg(any(test, feature = "test-helpers"))]
pub struct FrozenClock {
    /// The fixed UTC instant this clock always returns.
    pub time: chrono::DateTime<chrono::Utc>,
}

/// `Clock` implementation for the frozen test clock.
#[cfg(any(test, feature = "test-helpers"))]
impl Clock for FrozenClock {
    /// Return the fixed instant.
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
        self.time
    }

    /// Return the fixed date as `YYYY-MM-DD`.
    fn today_utc(&self) -> String {
        self.time.format("%Y-%m-%d").to_string()
    }
}

/// The real Plutus gate: enforces org status, RBAC, quota, and rate-limit in sequence.
///
/// Generic over `PolicyBackend` through a trait object (`Arc<dyn PolicyBackend>`) so the
/// production gate uses `PlutusStore` (Postgres) and test gates use `MockPolicyBackend`
/// (no live DB required -- D1 rule 6).
pub struct PlutusGate {
    /// The policy authority providing the four reads the gate needs.
    backend: Arc<dyn PolicyBackend>,
    /// The clock providing `now` and `today` to the rate-limit and quota steps.
    clock: Arc<dyn Clock>,
}

/// `PlutusGate` constructors.
impl PlutusGate {
    /// Build a gate backed by `backend` using the real system wall clock.
    ///
    /// Production call: `PlutusGate::new(Arc::new(plutus_store))`.
    pub fn new(backend: Arc<dyn PolicyBackend>) -> Self {
        Self {
            backend,
            clock: Arc::new(WallClock),
        }
    }

    /// Build a gate backed by `backend` with a custom `clock` (for deterministic tests).
    pub fn new_with_clock(backend: Arc<dyn PolicyBackend>, clock: Arc<dyn Clock>) -> Self {
        Self { backend, clock }
    }
}

/// The `Gate` implementation: runs the four-step fail-closed pipeline.
#[async_trait]
impl Gate for PlutusGate {
    /// Return the stable slot name for this gate.
    fn name(&self) -> &str {
        "plutus"
    }

    /// Authorize a request against org status, RBAC, quota, and rate limit.
    ///
    /// Pipeline (fail-closed at every step):
    ///  1. `org_status(tenant)` -- must return `Some(Active)`; `None` or non-Active => Deny.
    ///  2. `member_role(tenant, principal)` -- must return `Some(role)` with `can(role, perm)`.
    ///  3. `check_and_increment(tenant, dim, 1, today)` -- `allowed` must be true.
    ///  4. `rate_limit_ok(tenant, now)` -- must return `true`.
    ///
    /// Any `Err` from the backend propagates as `Err(GateError)`. The dispatcher treats
    /// every `GateError` as a denial, so a backend that cannot decide cannot accidentally allow.
    async fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        let tenant = req.context.tenant;
        let principal = req.context.principal;

        // Step 1: org must exist and be active.
        match self
            .backend
            .org_status(tenant)
            .await
            .map_err(|e| GateError::new(e.to_string()))?
        {
            None => {
                return Ok(GateDecision::Deny {
                    reason: "plutus: no org for tenant".into(),
                });
            }
            Some(OrgStatus::Active) => {}
            Some(other) => {
                return Ok(GateDecision::Deny {
                    reason: format!("plutus: org {other}"),
                });
            }
        }

        // Step 2: principal must be a member with a role that permits the action class.
        let class = map_invocation(&req.invocation);
        let role_opt = self
            .backend
            .member_role(tenant, principal)
            .await
            .map_err(|e| GateError::new(e.to_string()))?;
        let role = match role_opt {
            None => {
                return Ok(GateDecision::Deny {
                    reason: "plutus: principal is not a member of the org".into(),
                });
            }
            Some(r) => r,
        };
        if !can(role, class.permission) {
            return Ok(GateDecision::Deny {
                reason: format!("plutus: role {role} lacks {:?}", class.permission),
            });
        }

        // Step 3: hard daily quota for the action's dimension (skipped when dimension is None).
        if let Some(dim) = class.quota_dimension {
            let today = self.clock.today_utc();
            let outcome = self
                .backend
                .check_and_increment(tenant, dim, 1, &today)
                .await
                .map_err(|e| GateError::new(e.to_string()))?;
            if !outcome.allowed {
                return Ok(GateDecision::Deny {
                    reason: format!(
                        "plutus: quota_exceeded {dim} used={} limit={}",
                        outcome.used, outcome.limit
                    ),
                });
            }
        }

        // Step 4: per-org token-bucket rate limit.
        let now = self.clock.now_utc();
        let rate_ok = self
            .backend
            .rate_limit_ok(tenant, now)
            .await
            .map_err(|e| GateError::new(e.to_string()))?;
        if !rate_ok {
            return Ok(GateDecision::Deny {
                reason: "plutus: rate_limited".into(),
            });
        }

        Ok(GateDecision::Allow)
    }
}

/// Unit tests for Plutus gate policy and denial behavior.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockPolicyBackend;
    use crate::rbac::Role;
    use serde_json::json;
    use syntheos_contracts::{PrincipalId, RequestContext, TenantId, ToolInvocation};

    /// Build a minimal `GateRequest` for tests.
    fn make_req(tool: &str, action: &str) -> GateRequest {
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
                tool: tool.to_owned(),
                action: action.to_owned(),
                args: json!({}),
            },
        }
    }

    /// A frozen clock fixed at 2026-06-29T00:00:00Z.
    fn frozen() -> Arc<FrozenClock> {
        Arc::new(FrozenClock {
            time: chrono::DateTime::parse_from_rfc3339("2026-06-29T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        })
    }

    /// Build a gate backed by `backend` and the frozen clock.
    fn gate(backend: MockPolicyBackend) -> PlutusGate {
        PlutusGate::new_with_clock(Arc::new(backend), frozen())
    }

    // ---- Allow path ----

    /// Active org + Member role + memory_search (no quota dim) + within rate limit => Allow.
    #[tokio::test]
    async fn allow_active_org_member_role_memory_search() {
        let g = gate(MockPolicyBackend::allow_all());
        let req = make_req("kleos", "memory_search");
        assert_eq!(
            g.check(&req).await.expect("gate decides"),
            GateDecision::Allow
        );
    }

    /// Active org + Member role + memory_store (has quota dim, quota OK) => Allow.
    #[tokio::test]
    async fn allow_active_org_member_role_memory_store_with_quota() {
        let g = gate(MockPolicyBackend::allow_all());
        let req = make_req("kleos", "memory_store");
        assert_eq!(
            g.check(&req).await.expect("gate decides"),
            GateDecision::Allow
        );
    }

    // ---- Deny: step 1 (org check) ----

    /// Unknown tenant (org_status returns None) => Deny.
    #[tokio::test]
    async fn deny_unknown_tenant_no_org() {
        let g = gate(MockPolicyBackend::deny_no_org());
        let req = make_req("kleos", "memory_search");
        let decision = g.check(&req).await.expect("gate decides");
        match decision {
            GateDecision::Deny { reason } => {
                assert!(reason.contains("no org"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// Suspended org => Deny.
    #[tokio::test]
    async fn deny_suspended_org() {
        let g = gate(MockPolicyBackend::deny_suspended_org());
        let req = make_req("kleos", "memory_search");
        let decision = g.check(&req).await.expect("gate decides");
        match decision {
            GateDecision::Deny { reason } => {
                assert!(reason.contains("org"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ---- Deny: step 2 (membership + RBAC) ----

    /// Principal not a member of the org => Deny.
    #[tokio::test]
    async fn deny_principal_not_a_member() {
        let g = gate(MockPolicyBackend::deny_no_member());
        let req = make_req("kleos", "memory_search");
        let decision = g.check(&req).await.expect("gate decides");
        match decision {
            GateDecision::Deny { reason } => {
                assert!(reason.contains("not a member"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// Viewer role attempting an agent execute action => Deny (RBAC).
    ///
    /// An unknown tool/action maps to AgentExecute; Viewer does not hold that permission.
    #[tokio::test]
    async fn deny_viewer_role_agent_execute() {
        // Use Viewer role, invoke a mystery tool (maps to AgentExecute fail-closed).
        let g = gate(MockPolicyBackend::with_role(Role::Viewer));
        let req = make_req("mystery", "thing");
        let decision = g.check(&req).await.expect("gate decides");
        match decision {
            GateDecision::Deny { reason } => {
                assert!(reason.contains("lacks"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// Billing role attempting memory_store => Deny (RBAC -- Billing cannot MemoryStore).
    #[tokio::test]
    async fn deny_billing_role_memory_store() {
        let g = gate(MockPolicyBackend::with_role(Role::Billing));
        let req = make_req("kleos", "memory_store");
        let decision = g.check(&req).await.expect("gate decides");
        match decision {
            GateDecision::Deny { reason } => {
                assert!(reason.contains("lacks"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ---- Deny: step 3 (quota) ----

    /// Quota exhausted for the action's dimension => Deny.
    #[tokio::test]
    async fn deny_quota_exhausted() {
        let g = gate(MockPolicyBackend::deny_quota_exhausted());
        // memory_store has a quota_dimension; the mock reports quota_ok=false.
        let req = make_req("kleos", "memory_store");
        let decision = g.check(&req).await.expect("gate decides");
        match decision {
            GateDecision::Deny { reason } => {
                assert!(reason.contains("quota_exceeded"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    /// memory_search has no quota_dimension, so quota exhaustion is irrelevant -- still Allow.
    #[tokio::test]
    async fn allow_quota_exhausted_no_dimension_skips_check() {
        // quota_ok=false on the mock, but memory_search has no quota_dimension.
        let g = gate(MockPolicyBackend::deny_quota_exhausted());
        // Override: memory_search maps to MemorySearch with no quota dim -- Viewer can't do it
        // but Member can. Since deny_quota_exhausted has role=Member, the RBAC passes.
        // BUT: deny_quota_exhausted also has rate_ok=true, and memory_search has no quota dim,
        // so quota step is skipped entirely. Result: Allow.
        let req = make_req("kleos", "memory_search");
        assert_eq!(
            g.check(&req).await.expect("gate decides"),
            GateDecision::Allow,
            "no quota_dimension means quota step is skipped"
        );
    }

    // ---- Deny: step 4 (rate limit) ----

    /// Rate limit exhausted => Deny.
    #[tokio::test]
    async fn deny_rate_limited() {
        let g = gate(MockPolicyBackend::deny_rate_limited());
        // Use memory_search (no quota_dim) so only the rate-limit step can deny.
        let req = make_req("kleos", "memory_search");
        let decision = g.check(&req).await.expect("gate decides");
        match decision {
            GateDecision::Deny { reason } => {
                assert!(reason.contains("rate_limited"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ---- Error path (fail-closed: Err -> never Allow) ----

    /// When the backend returns an error the gate must return Err(GateError) -- NEVER Allow.
    ///
    /// This is the critical fail-closed proof: an unreachable authority cannot silently permit.
    #[tokio::test]
    async fn error_backend_returns_err_not_allow() {
        let g = gate(MockPolicyBackend::always_error());
        let req = make_req("kleos", "memory_search");
        let result = g.check(&req).await;
        match result {
            Err(gate_err) => {
                // Correct: error propagated, not converted to Allow.
                assert!(
                    gate_err.to_string().contains("mock error"),
                    "gate_err: {gate_err}"
                );
            }
            Ok(GateDecision::Allow) => {
                panic!("FAIL-CLOSED VIOLATION: gate returned Allow when backend errored");
            }
            Ok(GateDecision::Deny { reason }) => {
                // Also acceptable: the gate may choose to Deny on error (belt-and-suspenders).
                // But the contract says Err(GateError) is the signal; we accept Deny too since
                // the dispatcher denies on Err and the policy effect is the same.
                assert!(!reason.is_empty(), "deny reason must be non-empty");
            }
            Ok(GateDecision::RequireApproval { .. }) => {
                panic!("gate returned RequireApproval when backend errored -- this is wrong");
            }
            // Non-exhaustive: any future GateDecision variant on an error is also wrong.
            Ok(other) => {
                panic!("gate returned non-error decision {other:?} when backend errored");
            }
        }
    }

    /// Gate name is the canonical slot identifier.
    #[test]
    fn gate_name_is_plutus() {
        // Use the wall-clock constructor; name() is synchronous.
        let g = PlutusGate::new(Arc::new(MockPolicyBackend::allow_all()));
        assert_eq!(g.name(), "plutus");
    }
}
