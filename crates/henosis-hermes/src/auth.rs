//! Authenticated tenant identity for standalone Hermes transports.
//!
//! The standalone server binds one configured Bearer credential to one tenant.
//! HTTP and MCP handlers receive this type only after middleware authenticates
//! that credential, so request bodies may assert but never select authority.

use std::sync::Arc;

/// Maximum accepted length for a configured tenant identifier.
const MAX_TENANT_ID_LEN: usize = 128;

/// Tenant identity established by successful transport authentication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedTenant(Arc<str>);

/// Validates and exposes authenticated tenant identities.
impl AuthenticatedTenant {
    /// Validate a configured tenant identifier and construct its identity.
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty() {
            return Err("must not be empty".to_string());
        }
        if value.len() > MAX_TENANT_ID_LEN {
            return Err(format!("must contain at most {MAX_TENANT_ID_LEN} bytes"));
        }
        if value.trim() != value || value.chars().any(char::is_whitespace) {
            return Err("must not contain whitespace".to_string());
        }
        if value.chars().any(char::is_control) {
            return Err("must not contain control characters".to_string());
        }
        Ok(Self(Arc::from(value)))
    }

    /// Borrow the tenant identifier established by authentication.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return whether an optional compatibility claim matches this identity.
    pub fn matches_claim(&self, claimed: Option<&str>) -> bool {
        claimed.is_none_or(|value| value == self.as_str())
    }
}

#[cfg(test)]
/// Tests for authenticated tenant validation and claim matching.
mod tests {
    use super::*;

    /// Valid tenant identifiers retain their exact configured value.
    #[test]
    fn accepts_safe_tenant_identifier() {
        let tenant = AuthenticatedTenant::parse("tenant:92ec3f4b").expect("valid tenant");
        assert_eq!(tenant.as_str(), "tenant:92ec3f4b");
    }

    /// Empty, whitespace-bearing, control-bearing, and oversized identifiers fail closed.
    #[test]
    fn rejects_ambiguous_tenant_identifiers() {
        for invalid in ["", " tenant", "tenant name", "tenant\nother"] {
            assert!(AuthenticatedTenant::parse(invalid).is_err(), "{invalid:?}");
        }
        assert!(AuthenticatedTenant::parse(&"x".repeat(MAX_TENANT_ID_LEN + 1)).is_err());
    }

    /// Omitted or equal compatibility claims pass while foreign claims fail.
    #[test]
    fn matches_only_own_tenant_claim() {
        let tenant = AuthenticatedTenant::parse("tenant-a").expect("valid tenant");
        assert!(tenant.matches_claim(None));
        assert!(tenant.matches_claim(Some("tenant-a")));
        assert!(!tenant.matches_claim(Some("tenant-b")));
    }
}
