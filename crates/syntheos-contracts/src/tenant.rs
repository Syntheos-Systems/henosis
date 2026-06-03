//! The billing/isolation subject. Deliberately minimal: Plutus owns roles, quota,
//! and entitlements, so nothing about those is baked into the substrate here.

use serde::{Deserialize, Serialize};

use crate::ids::TenantId;

/// A billing/isolation boundary. Minimal by design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    /// Stable identity of this tenant.
    pub id: TenantId,
    /// URL-safe short name (e.g. `acme`).
    pub slug: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_roundtrip() {
        let t = Tenant {
            id: TenantId::new(),
            slug: "acme".to_string(),
        };
        let json = serde_json::to_string(&t).expect("serialize");
        let back: Tenant = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(t, back);
    }
}
