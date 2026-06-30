//! `OperatorAuth` axum extractor and RBAC permission check helper.
//!
//! [`OperatorAuth`] implements [`axum::extract::FromRequestParts<OperatorState>`].
//! It reads the `Authorization: Bearer` header, decodes and verifies the JWT via
//! [`super::auth::decode`], then parses the embedded `sub`, `org`, and `role`
//! fields into their strongly-typed forms. Any failure produces a 401 response.
//!
//! [`OperatorAuth::require`] is the per-endpoint permission gate: it calls
//! [`henosis_plutus::can`] and converts a denial into a 403 [`OperatorError::Forbidden`].

use std::str::FromStr;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use henosis_plutus::{can, Permission, Role};
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
    /// 5. Parse `claims.role` -> [`Role`] via [`Role::from_str`] (401 on unrecognized role).
    async fn from_request_parts(
        parts: &mut Parts,
        state: &OperatorState,
    ) -> Result<Self, Self::Rejection> {
        // Step 1: extract the raw Bearer token string from the Authorization header.
        let token = bearer_from_parts(parts)?;

        // Step 2: decode and cryptographically verify the JWT.
        let claims = auth::decode(token, &state.jwt_secret)?;

        // Step 3: parse the principal UUID from the `sub` claim.
        let principal = PrincipalId::from_str(&claims.sub)
            .map_err(|e| OperatorError::Auth(format!("invalid principal in JWT: {e}")))?;

        // Step 4: parse the tenant UUID from the `org` claim.
        let org = TenantId::from_str(&claims.org)
            .map_err(|e| OperatorError::Auth(format!("invalid org in JWT: {e}")))?;

        // Step 5: parse the role string from the `role` claim.
        let role = Role::from_str(&claims.role)
            .map_err(|e| OperatorError::Auth(format!("invalid role in JWT: {e}")))?;

        Ok(OperatorAuth { principal, org, role })
    }
}

/// Extract a Bearer token string from the request's `Authorization` header.
///
/// Returns the raw token string (without the `Bearer ` prefix) as a slice
/// borrowing from `parts.headers`. Returns [`OperatorError::Auth`] (401) when
/// the header is absent or its value does not start with `Bearer `.
fn bearer_from_parts(parts: &Parts) -> Result<&str, OperatorError> {
    let auth = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| OperatorError::Auth("missing Authorization header".into()))?;
    auth.strip_prefix("Bearer ")
        .ok_or_else(|| OperatorError::Auth("expected Bearer token in Authorization header".into()))
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
}
