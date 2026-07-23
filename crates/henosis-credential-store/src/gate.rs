//! The embedded compatibility gate for credential broker operations.
//!
//! The gate is the policy decision point for the dispatcher's `phylaxd` slot. Cryptographic
//! resolution happens at execution time through [`CredentialStore`] methods. The gate asks the
//! store whether the requesting principal may perform the requested operation.
//!
//! A credential operation is an invocation whose `tool` is `"phylaxd"`. Its `action`
//! is the resolve mode (`sign`/`verify`/`derive`/`exec`) and its `args` carry string `category`
//! and `name` fields. Any other tool is not a credential operation and the gate allows it (the
//! other gates in the chain decide it). A malformed broker invocation -- unknown mode, missing
//! category/name -- is DENIED, never waved through.
//!
//! Fail-closed by construction: the only paths to `Allow` are a non-broker invocation or a
//! policy that explicitly permits the mode. A policy-store error becomes a [`GateError`], which
//! the dispatcher denies on. The gate never converts a backing-store failure or missing policy
//! into an allow.

use std::sync::Arc;

use async_trait::async_trait;
use syntheos_contracts::{Gate, GateDecision, GateError, GateRequest, ToolInvocation};

use crate::error::CredentialStoreError;
use crate::model::ResolveMode;
use crate::store::CredentialStore;

/// The tool name that marks an invocation as a credential broker operation.
const CREDENTIAL_BROKER_TOOL: &str = "phylaxd";

/// The fail-closed embedded compatibility gate for credential-store operations.
pub struct CredentialStoreGate {
    /// The store whose capability policies the gate consults.
    store: Arc<CredentialStore>,
}

/// Implements embedded credential-policy construction and action classification.
impl CredentialStoreGate {
    /// Build the gate over a credential store.
    pub fn new(store: Arc<CredentialStore>) -> Self {
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
/// Applies embedded credential-access policy in the compatibility gate chain.
impl Gate for CredentialStoreGate {
    /// The canonical authority name for this slot.
    fn name(&self) -> &str {
        "phylaxd"
    }

    /// Authorize a credential operation, allow a non-credential one, deny a malformed one, and
    /// surface a policy-store failure as a fail-closed [`GateError`].
    async fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        // Non-broker operations are decided by the other gates in the chain.
        if req.invocation.tool != CREDENTIAL_BROKER_TOOL {
            return Ok(GateDecision::Allow);
        }

        // A broker invocation must name a known mode and a (category, name). A malformed one is
        // denied, not waved through.
        let Some(mode) = Self::parse_mode(&req.invocation.action) else {
            return Ok(GateDecision::Deny {
                reason: format!(
                    "unknown credential broker resolve mode {:?}",
                    req.invocation.action
                ),
            });
        };
        let (Some(category), Some(name)) = (
            Self::arg_str(&req.invocation, "category"),
            Self::arg_str(&req.invocation, "name"),
        ) else {
            return Ok(GateDecision::Deny {
                reason: "credential broker invocation missing string 'category'/'name' args".into(),
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
            Err(CredentialStoreError::PermissionDenied(reason)) => {
                Ok(GateDecision::Deny { reason })
            }
            Err(e) => Err(GateError::new(format!("credential store unavailable: {e}"))),
        }
    }
}

/// Tests embedded credential-gate authorization decisions.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use crate::model::SecretData;
    use syntheos_axon::AxonBus;
    use syntheos_contracts::{PrincipalId, RequestContext, TenantId};

    /// A store with one Note secret and a sign-allowing policy for `principal`.
    fn store_with_policy() -> (Arc<CredentialStore>, TenantId, PrincipalId) {
        let store =
            CredentialStore::open_in_memory(Arc::new(AxonBus::new()), *crypto::generate_key())
                .unwrap();
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

    /// Build a gate request for a phylaxd invocation.
    fn credential_broker_req(
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
                authority: None,
            },
            invocation: ToolInvocation {
                tool: "phylaxd".into(),
                action: action.into(),
                args,
            },
        }
    }

    /// A non-phylaxd invocation is allowed (this gate has nothing to authorize).
    #[tokio::test]
    async fn non_broker_invocation_allowed() {
        let (store, tenant, principal) = store_with_policy();
        let gate = CredentialStoreGate::new(store);
        let mut req = credential_broker_req(tenant, principal, "sign", serde_json::json!({}));
        req.invocation.tool = "kleos".into();
        req.invocation.action = "memory_store".into();
        assert_eq!(gate.check(&req).await.unwrap(), GateDecision::Allow);
    }

    /// A permitted mode on a covered secret is allowed.
    #[tokio::test]
    async fn permitted_mode_allowed() {
        let (store, tenant, principal) = store_with_policy();
        let gate = CredentialStoreGate::new(store);
        let req = credential_broker_req(
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
        let gate = CredentialStoreGate::new(store);
        let req = credential_broker_req(
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
        let gate = CredentialStoreGate::new(store);
        let intruder = PrincipalId::new();
        let req = credential_broker_req(
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
    async fn malformed_broker_invocation_denied() {
        let (store, tenant, principal) = store_with_policy();
        let gate = CredentialStoreGate::new(store);

        let unknown = credential_broker_req(
            tenant,
            principal,
            "decrypt",
            serde_json::json!({"category": "prod", "name": "db"}),
        );
        assert!(matches!(
            gate.check(&unknown).await.unwrap(),
            GateDecision::Deny { .. }
        ));

        let missing = credential_broker_req(
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

    /// The real gate, in the phylaxd slot of the canonical dispatcher chain, denies a credential
    /// request whose policy forbids it -- at the phylaxd slot specifically -- and lets a permitted
    /// one through to the executor.
    #[tokio::test]
    async fn dispatcher_denies_at_broker_slot() {
        use syntheos_dispatch::stubs::{EchoExecutor, StubGate};
        use syntheos_dispatch::{DispatchOutcome, Dispatcher};

        let (store, tenant, principal) = store_with_policy();
        let bus = Arc::new(AxonBus::new());
        let gates: Vec<Box<dyn Gate>> = vec![
            Box::new(StubGate::new("pistis")),
            Box::new(StubGate::new("plutus")),
            Box::new(StubGate::new("eidolon")),
            Box::new(StubGate::new("human")),
            Box::new(CredentialStoreGate::new(store)),
        ];
        let dispatcher =
            Dispatcher::new(gates, Box::new(EchoExecutor), bus).expect("canonical chain");

        // derive is not in the policy (only sign is) -> denied at phylaxd.
        let denied = dispatcher
            .dispatch(credential_broker_req(
                tenant,
                principal,
                "derive",
                serde_json::json!({"category": "prod", "name": "db"}),
            ))
            .await
            .expect("dispatch");
        match denied {
            DispatchOutcome::Denied { gate, .. } => assert_eq!(gate, "phylaxd"),
            other => panic!("expected Denied at phylaxd, got {other:?}"),
        }

        // sign is permitted -> the request traverses every stub and reaches the executor.
        let allowed = dispatcher
            .dispatch(credential_broker_req(
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

    /// The gate never returns Allow for a phylaxd invocation that was not explicitly permitted:
    /// sweep the mismatch combinations and assert none allow.
    #[tokio::test]
    async fn no_allow_without_explicit_policy() {
        let (store, tenant, principal) = store_with_policy();
        let gate = CredentialStoreGate::new(store);
        // Sign is the only permitted mode on prod/db for this principal. Every other mode, and
        // every other secret name, must NOT allow.
        for action in ["verify", "derive", "exec"] {
            let req = credential_broker_req(
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
        let other_secret = credential_broker_req(
            tenant,
            principal,
            "sign",
            serde_json::json!({"category": "prod", "name": "other"}),
        );
        // Policy is category-scoped (name = NULL), so a different name under prod is still covered
        // for sign. Use a different category to prove non-coverage denies.
        let other_cat = credential_broker_req(
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
