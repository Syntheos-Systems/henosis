//! The canonical actor: one per human, agent, service account, or integration.
//! Per-service projections (Soma presence, Pistis grants, Phylax scopes) stay in
//! those services. This is only the shared key, kind, and display name.

use serde::{Deserialize, Serialize};

use crate::ids::PrincipalId;

/// A canonical actor in the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    /// Stable identity of this actor.
    pub id: PrincipalId,
    /// What category of actor this is.
    pub kind: PrincipalKind,
    /// Optional human-readable label (e.g. a username or service name).
    pub display: Option<String>,
}

/// The category of a [`Principal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A human user.
    Human,
    /// An autonomous or semi-autonomous agent.
    Agent,
    /// A service account (machine-to-machine).
    Service,
    /// An external integration.
    Integration,
}

/// Tests for principal wire contracts.
#[cfg(test)]
mod tests {
    use super::*;

    /// Principal values roundtrip with the existing field shape.
    #[test]
    fn principal_roundtrip() {
        let p = Principal {
            id: PrincipalId::new(),
            kind: PrincipalKind::Agent,
            display: Some("eidolon".to_string()),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: Principal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(p, back);
    }

    /// Principal kinds serialize in snake_case.
    #[test]
    fn kind_serializes_snake_case() {
        let json = serde_json::to_string(&PrincipalKind::Service).expect("serialize");
        assert_eq!(json, "\"service\"");
    }

    /// Principal values reject misspelled fields.
    #[test]
    fn principal_rejects_unknown_fields() {
        let json = format!(
            "{{\"id\":\"{}\",\"kind\":\"agent\",\"display\":null,\"dispaly\":\"bad\"}}",
            PrincipalId::new().as_uuid()
        );
        let err = serde_json::from_str::<Principal>(&json).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }
}
