//! The `phylax` gate: authorize credential-touching invocations, fail-closed.
//!
//! The gate is the policy DECISION point for the dispatcher's phylax slot; the actual
//! cryptographic resolution happens later, at execution, when the executor calls the
//! [`PhylaxStore`] resolve methods in-process. The gate inspects whether an invocation is a
//! Phylax credential operation and, if so, asks the store whether the requesting principal's
//! policy permits that mode on that secret.
//!
//! Convention: a credential operation is an invocation whose `tool` is `"phylax"`. Its `action`
//! is the resolve mode (`sign`/`verify`/`derive`/`exec`) and its `args` carry string `category`
//! and `name` fields. Any other tool is not a credential operation and the gate allows it (the
//! other gates in the chain decide it). A malformed phylax invocation -- unknown mode, missing
//! category/name -- is DENIED, never waved through.
//!
//! Fail-closed by construction: the only paths to `Allow` are a non-phylax invocation or a
//! policy that explicitly permits the mode. A policy-store error becomes a [`GateError`], which
//! the dispatcher denies on. There is NO advisory mode and NO self-approval path (the roadmap
//! security-debt rule): the gate never converts a backing-store failure or a missing policy into
//! an allow.

use std::sync::Arc;

use async_trait::async_trait;
use syntheos_contracts::{Gate, GateDecision, GateError, GateRequest, ToolInvocation};

use crate::error::PhylaxError;
use crate::model::ResolveMode;
use crate::store::PhylaxStore;

/// The tool name that marks an invocation as a Phylax credential operation.
const PHYLAX_TOOL: &str = "phylax";

/// The fail-closed authorization gate for the dispatcher's phylax slot.
pub struct PhylaxGate {
    /// The store whose capability policies the gate consults.
    store: Arc<PhylaxStore>,
}

impl PhylaxGate {
    /// Build the gate over a credential store.
    pub fn new(store: Arc<PhylaxStore>) -> Self {
        Self { store }
    }

    /// Map an action token to a resolve mode, or `None` for an unknown action.
    fn parse_mode(action: &str) -> Option<ResolveMode> {
        match action {
            "sign" => Some(ResolveMode::Sign),
            "verify" => Some(ResolveMode::Verify),
            "derive" => Some(ResolveMode::Derive),
            "exec" => Some(ResolveMode::Exec),
            _ => None,
        }
    }

    /// Pull a required string field from the invocation args.
    fn arg_str<'a>(invocation: &'a ToolInvocation, key: &str) -> Option<&'a str> {
        invocation.args.get(key).and_then(|v| v.as_str())
    }
}

#[async_trait]
impl Gate for PhylaxGate {
    /// The canonical authority name for this slot.
    fn name(&self) -> &str {
        "phylax"
    }

    /// Authorize a credential operation, allow a non-credential one, deny a malformed one, and
    /// surface a policy-store failure as a fail-closed [`GateError`].
    async fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        // Not a Phylax credential operation -- nothing for this gate to authorize.
        if req.invocation.tool != PHYLAX_TOOL {
            return Ok(GateDecision::Allow);
        }

        // A phylax invocation must name a known mode and a (category, name). A malformed one is
        // denied, not waved through.
        let Some(mode) = Self::parse_mode(&req.invocation.action) else {
            return Ok(GateDecision::Deny {
                reason: format!("unknown phylax resolve mode {:?}", req.invocation.action),
            });
        };
        let (Some(category), Some(name)) = (
            Self::arg_str(&req.invocation, "category"),
            Self::arg_str(&req.invocation, "name"),
        ) else {
            return Ok(GateDecision::Deny {
                reason: "phylax invocation missing string 'category'/'name' args".into(),
            });
        };

        // The decision is the store's policy verdict. PermissionDenied is a definitive Deny; any
        // other error means the authority could not decide -> GateError -> dispatcher denies.
        match self.store.authorize_mode(
            &req.context.tenant,
            &req.context.principal,
            category,
            name,
            mode,
        ) {
            Ok(()) => Ok(GateDecision::Allow),
            Err(PhylaxError::PermissionDenied(reason)) => Ok(GateDecision::Deny { reason }),
            Err(e) => Err(GateError::new(format!("phylax authority unavailable: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use crate::model::SecretData;
    use syntheos_axon::AxonBus;
    use syntheos_contracts::{PrincipalId, RequestContext, TenantId};

    /// A store with one Note secret and a sign-allowing policy for `principal`.
    fn store_with_policy() -> (Arc<PhylaxStore>, TenantId, PrincipalId) {
        let store =
            PhylaxStore::open_in_memory(Arc::new(AxonBus::new()), *crypto::generate_key()).unwrap();
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        store
            .store_secret(
                &tenant,
                &principal,
                "prod",
                "db",
                &SecretData::Note {
                    content: "super-secret".into(),
                },
            )
            .unwrap();
        store
            .create_policy(
                &tenant,
                Some(&principal),
                Some("prod"),
                None,
                &[ResolveMode::Sign],
                None,
            )
            .unwrap();
        (Arc::new(store), tenant, principal)
    }

    /// Build a gate request for a phylax invocation.
    fn phylax_req(
        tenant: TenantId,
        principal: PrincipalId,
        action: &str,
        args: serde_json::Value,
    ) -> GateRequest {
        GateRequest {
            context: RequestContext {
                tenant,
                principal,
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
            },
            invocation: ToolInvocation {
                tool: "phylax".into(),
                action: action.into(),
                args,
            },
        }
    }

    /// A non-phylax invocation is allowed (this gate has nothing to authorize).
    #[tokio::test]
    async fn non_phylax_invocation_allowed() {
        let (store, tenant, principal) = store_with_policy();
        let gate = PhylaxGate::new(store);
        let mut req = phylax_req(tenant, principal, "sign", serde_json::json!({}));
        req.invocation.tool = "kleos".into();
        req.invocation.action = "memory_store".into();
        assert_eq!(gate.check(&req).await.unwrap(), GateDecision::Allow);
    }

    /// A permitted mode on a covered secret is allowed.
    #[tokio::test]
    async fn permitted_mode_allowed() {
        let (store, tenant, principal) = store_with_policy();
        let gate = PhylaxGate::new(store);
        let req = phylax_req(
            tenant,
            principal,
            "sign",
            serde_json::json!({"category": "prod", "name": "db"}),
        );
        assert_eq!(gate.check(&req).await.unwrap(), GateDecision::Allow);
    }

    /// A mode the policy does not name is denied.
    #[tokio::test]
    async fn mode_not_in_policy_denied() {
        let (store, tenant, principal) = store_with_policy();
        let gate = PhylaxGate::new(store);
        let req = phylax_req(
            tenant,
            principal,
            "derive",
            serde_json::json!({"category": "prod", "name": "db"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// A principal with no policy is denied (deny-by-default).
    #[tokio::test]
    async fn principal_without_policy_denied() {
        let (store, tenant, _principal) = store_with_policy();
        let gate = PhylaxGate::new(store);
        let intruder = PrincipalId::new();
        let req = phylax_req(
            tenant,
            intruder,
            "sign",
            serde_json::json!({"category": "prod", "name": "db"}),
        );
        assert!(matches!(
            gate.check(&req).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// An unknown mode and a missing category/name are both denied, never allowed.
    #[tokio::test]
    async fn malformed_phylax_invocation_denied() {
        let (store, tenant, principal) = store_with_policy();
        let gate = PhylaxGate::new(store);

        let unknown = phylax_req(
            tenant,
            principal,
            "decrypt",
            serde_json::json!({"category": "prod", "name": "db"}),
        );
        assert!(matches!(
            gate.check(&unknown).await.unwrap(),
            GateDecision::Deny { .. }
        ));

        let missing = phylax_req(
            tenant,
            principal,
            "sign",
            serde_json::json!({"category": "prod"}),
        );
        assert!(matches!(
            gate.check(&missing).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// The real gate, in the phylax slot of the canonical dispatcher chain, denies a credential
    /// request whose policy forbids it -- at the phylax slot specifically -- and lets a permitted
    /// one through to the executor.
    #[tokio::test]
    async fn dispatcher_denies_at_phylax_slot() {
        use syntheos_dispatch::stubs::{EchoExecutor, StubGate};
        use syntheos_dispatch::{DispatchOutcome, Dispatcher};

        let (store, tenant, principal) = store_with_policy();
        let bus = Arc::new(AxonBus::new());
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(StubGate::new("pistis")),
            Box::new(StubGate::new("plutus")),
            Box::new(StubGate::new("eidolon")),
            Box::new(StubGate::new("human")),
            Box::new(PhylaxGate::new(store)),
        ];
        let dispatcher =
            Dispatcher::new(gates, Box::new(EchoExecutor), bus).expect("canonical chain");

        // derive is not in the policy (only sign is) -> denied at phylax.
        let denied = dispatcher
            .dispatch(phylax_req(
                tenant,
                principal,
                "derive",
                serde_json::json!({"category": "prod", "name": "db"}),
            ))
            .await
            .expect("dispatch");
        match denied {
            DispatchOutcome::Denied { gate, .. } => assert_eq!(gate, "phylax"),
            other => panic!("expected Denied at phylax, got {other:?}"),
        }

        // sign is permitted -> the request traverses every stub and reaches the executor.
        let allowed = dispatcher
            .dispatch(phylax_req(
                tenant,
                principal,
                "sign",
                serde_json::json!({"category": "prod", "name": "db"}),
            ))
            .await
            .expect("dispatch");
        assert!(
            matches!(allowed, DispatchOutcome::Executed { .. }),
            "permitted credential request must reach the executor, got {allowed:?}"
        );
    }

    /// The gate never returns Allow for a phylax invocation that was not explicitly permitted:
    /// sweep the mismatch combinations and assert none allow.
    #[tokio::test]
    async fn no_allow_without_explicit_policy() {
        let (store, tenant, principal) = store_with_policy();
        let gate = PhylaxGate::new(store);
        // Sign is the only permitted mode on prod/db for this principal. Every other mode, and
        // every other secret name, must NOT allow.
        for action in ["verify", "derive", "exec"] {
            let req = phylax_req(
                tenant,
                principal,
                action,
                serde_json::json!({"category": "prod", "name": "db"}),
            );
            assert_ne!(
                gate.check(&req).await.unwrap(),
                GateDecision::Allow,
                "mode {action} must not be allowed"
            );
        }
        let other_secret = phylax_req(
            tenant,
            principal,
            "sign",
            serde_json::json!({"category": "prod", "name": "other"}),
        );
        // Policy is category-scoped (name = NULL), so a different name under prod is still covered
        // for sign. Use a different category to prove non-coverage denies.
        let other_cat = phylax_req(
            tenant,
            principal,
            "sign",
            serde_json::json!({"category": "staging", "name": "db"}),
        );
        assert_eq!(
            gate.check(&other_secret).await.unwrap(),
            GateDecision::Allow,
            "category-scoped policy covers any name under prod"
        );
        assert!(matches!(
            gate.check(&other_cat).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }
}
