//! Agent-name -> PrincipalId resolution for the absorbed bridge.
//!
//! The standalone bridge passed bare agent usernames (`agent: &str`). Henosis
//! kernel APIs (`henosis_pistis::authority::CapabilityCheckRequest.principal`,
//! and the Chiasm/Broca stores in the Kleos seam) require a typed
//! `syntheos_contracts::PrincipalId`. There is no registry lookup in any store
//! crate, so the bridge resolves a username deterministically: a UUID v5
//! (stable SHA-1 digest of a fixed namespace + the username), restamped as a
//! UUID v8 because `PrincipalId::from_uuid` enforces the syntheos v8 invariant
//! (it rejects non-v8). The SAME convention is used by the Pistis seam and the
//! Kleos seam so both resolve an agent to the same principal.

use syntheos_contracts::PrincipalId;
use syntheos_contracts::TenantId;
use uuid::Uuid;

/// Fixed namespace for hashing rift-bridge agent usernames into principals.
const RIFT_AGENT_NAMESPACE: Uuid = Uuid::from_bytes([
    0x6e, 0x4f, 0x9a, 0x21, 0x7d, 0x3c, 0x4b, 0x88, 0x9e, 0x21, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
]);

/// Resolve a bridge agent username to a deterministic, registry-free
/// [`PrincipalId`]. Deterministic: the same username always yields the same
/// principal, across both the Pistis and Kleos seams and across process runs.
pub fn principal_for_agent(agent: &str) -> PrincipalId {
    let v5 = Uuid::new_v5(&RIFT_AGENT_NAMESPACE, agent.as_bytes());
    // Restamp the v5 digest bytes as v8 to satisfy the PrincipalId v8 invariant.
    let v8 = Uuid::new_v8(v5.into_bytes());
    PrincipalId::from_uuid(v8).expect("new_v8 always yields a v8 UUID")
}

/// Deterministic, registry-free tenant the in-process bridge writes under, so
/// its Chiasm/Broca records land in one stable tenant across reboots. Same
/// v5->v8 convention as `principal_for_agent`.
pub fn bridge_tenant() -> TenantId {
    let v5 = Uuid::new_v5(&RIFT_AGENT_NAMESPACE, b"rift-bridge-tenant");
    TenantId::from_uuid(Uuid::new_v8(v5.into_bytes())).expect("new_v8 always yields a v8 UUID")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same username resolves to the same principal every time.
    #[test]
    fn deterministic() {
        assert_eq!(principal_for_agent("architect"), principal_for_agent("architect"));
    }

    /// Different usernames resolve to different principals.
    #[test]
    fn distinct_agents_distinct_principals() {
        assert_ne!(principal_for_agent("architect"), principal_for_agent("scribe"));
    }

    /// The bridge tenant is deterministic across calls.
    #[test]
    fn bridge_tenant_is_deterministic() {
        assert_eq!(super::bridge_tenant(), super::bridge_tenant());
    }
}
