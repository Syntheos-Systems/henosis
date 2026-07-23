//! `OperatorAuth` axum extractor and RBAC permission check helper.
//!
//! [`OperatorAuth`] implements [`axum::extract::FromRequestParts<OperatorState>`].
//! It reads the `Authorization: Bearer` header, decodes and verifies the JWT via
//! [`super::auth::decode`], parses the embedded `sub` and `org` fields, and resolves the current
//! membership role from policy state. Any authentication failure produces a 401 response.
//!
//! [`OperatorAuth::require`] is the per-endpoint permission gate: it calls
//! [`henosis_plutus::can`] and converts a denial into a 403 [`OperatorError::Forbidden`].

use std::str::FromStr;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use henosis_plutus::{can, OrgStatus, Permission, PolicyBackend, Role};
use syntheos_contracts::{PrincipalId, TenantId};

use super::auth::{self, OperatorError};
use super::OperatorState;

/// The resolved, authenticated operator identity extracted from a Bearer JWT.
///
/// Produced by the [`FromRequestParts<OperatorState>`] implementation on every
/// request that carries a valid `Authorization: Bearer` token. Handlers declare
/// `OperatorAuth` as a parameter to require authentication, then call
/// [`require`](OperatorAuth::require) to enforce a specific permission.
#[derive(Debug, Clone)]
pub struct OperatorAuth {
    /// The authenticated principal's UUID.
    pub principal: PrincipalId,
    /// The tenant (org) UUID the session is scoped to.
    pub org: TenantId,
    /// The principal's RBAC role within the org.
    pub role: Role,
}

/// Enforces endpoint permissions for an authenticated operator.
impl OperatorAuth {
    /// Assert that the authenticated operator holds `perm`.
    ///
    /// Delegates to [`henosis_plutus::can`]. Returns `Ok(())` when
    /// `can(self.role, perm)` is `true`. Returns [`OperatorError::Forbidden`]
    /// (HTTP 403) otherwise. Handlers call this at the top of their body to
    /// enforce per-endpoint permission checks.
    pub fn require(&self, perm: Permission) -> Result<(), OperatorError> {
        if can(self.role, perm) {
            Ok(())
        } else {
            Err(OperatorError::Forbidden(format!(
                "role {:?} does not have permission {:?}",
                self.role, perm
            )))
        }
    }
}

/// Resolve the current active membership role for one signed operator identity.
pub(super) async fn resolve_live_role(
    policy: &dyn PolicyBackend,
    org: TenantId,
    principal: PrincipalId,
) -> Result<Role, OperatorError> {
    let status = policy
        .org_status(org)
        .await
        .map_err(|error| OperatorError::Backend(error.to_string()))?;
    if status != Some(OrgStatus::Active) {
        return Err(OperatorError::Auth("invalid session token".into()));
    }
    policy
        .member_role(org, principal)
        .await
        .map_err(|error| OperatorError::Backend(error.to_string()))?
        .ok_or_else(|| OperatorError::Auth("invalid session token".into()))
}

/// Extracts and validates operator identity and role claims from bearer tokens.
impl FromRequestParts<OperatorState> for OperatorAuth {
    /// Authentication or authorization failures become [`OperatorError`] responses
    /// (401 for auth failures, 403 for permission denials).
    type Rejection = OperatorError;

    /// Extract an [`OperatorAuth`] from the incoming request parts.
    ///
    /// Steps:
    /// 1. Read the `Authorization: Bearer <token>` header (401 on missing/malformed).
    /// 2. Decode and verify the JWT against `state.jwt_secret` (401 on any JWT failure).
    /// 3. Parse `claims.sub` -> [`PrincipalId`] via [`PrincipalId::from_str`] (401 on failure).
    /// 4. Parse `claims.org` -> [`TenantId`] via [`TenantId::from_str`] (401 on failure).
    /// 5. Resolve the current active membership and role from the policy backend.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &OperatorState,
    ) -> Result<Self, Self::Rejection> {
        // Step 1: extract the raw Bearer token string from the Authorization header.
        let token = auth::bearer_token(&parts.headers)?;

        // Step 2: decode and cryptographically verify the JWT.
        let claims = auth::decode(token, &state.jwt_secret)?;

        // Step 3: parse the principal UUID from the `sub` claim.
        let principal = PrincipalId::from_str(&claims.sub)
            .map_err(|e| OperatorError::Auth(format!("invalid principal in JWT: {e}")))?;

        // Step 4: parse the tenant UUID from the `org` claim.
        let org = TenantId::from_str(&claims.org)
            .map_err(|e| OperatorError::Auth(format!("invalid org in JWT: {e}")))?;

        // Step 5: signed claims identify the subject, but current policy grants authority.
        let role = resolve_live_role(&*state.plutus, org, principal).await?;

        Ok(OperatorAuth {
            principal,
            org,
            role,
        })
    }
}

#[cfg(test)]
/// Unit tests for [`OperatorAuth::require`].
mod tests {
    use super::*;

    /// A `Viewer` is allowed `OrgRead` but denied `OrgDelete`; `require` maps each correctly.
    ///
    /// Covers the `require` -> `can` delegation without going through the HTTP extractor
    /// path. End-to-end extractor behaviour is exercised in Task 5's dashboard test.
    #[test]
    fn rbac_require_allows_and_denies() {
        let auth = OperatorAuth {
            principal: PrincipalId::new(),
            org: TenantId::new(),
            role: Role::Viewer,
        };
        assert!(auth.require(Permission::OrgRead).is_ok());
        assert!(auth.require(Permission::OrgDelete).is_err());
    }

    /// Live role resolution rejects stale tenant membership instead of trusting JWT role claims.
    #[tokio::test]
    async fn live_role_resolution_rejects_stale_membership() {
        use henosis_plutus::{LocalPolicyBackend, QuotaTier};

        let active_org = TenantId::new();
        let principal = PrincipalId::new();
        let policy = LocalPolicyBackend::new(active_org, principal, Role::Member, QuotaTier::Free);
        assert_eq!(
            resolve_live_role(&policy, active_org, principal)
                .await
                .expect("active membership"),
            Role::Member
        );
        assert!(matches!(
            resolve_live_role(&policy, TenantId::new(), principal).await,
            Err(OperatorError::Auth(_))
        ));
        assert!(matches!(
            resolve_live_role(&policy, active_org, PrincipalId::new()).await,
            Err(OperatorError::Auth(_))
        ));
    }
}
