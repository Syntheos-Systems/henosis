//! An opaque reference to a Phylax-held secret. This is never the secret value
//! itself; only a handle that authorized callers can resolve through Phylax.

use serde::{Deserialize, Serialize};

/// A reference to a secret held by Phylax. Carries no secret material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialHandle {
    /// Opaque identifier of the credential. A `String` rather than a typed UUID
    /// newtype because Phylax owns the credential namespace and its identifier
    /// format is not the substrate's to define.
    pub id: String,
    /// The scope the credential is valid for (e.g. `kleos:write`).
    pub scope: String,
}

/// Tests for credential handle wire contracts.
#[cfg(test)]
mod tests {
    use super::*;

    /// Credential handles roundtrip without exposing secret material.
    #[test]
    fn credential_handle_roundtrip() {
        let c = CredentialHandle {
            id: "cred_123".to_string(),
            scope: "kleos:write".to_string(),
        };
        let json = serde_json::to_string(&c).expect("serialize");
        let back: CredentialHandle = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(c, back);
    }

    /// Credential handles reject misspelled fields.
    #[test]
    fn credential_handle_rejects_unknown_fields() {
        let json = r#"{"id":"cred_123","scope":"kleos:write","scpoe":"bad"}"#;
        let err = serde_json::from_str::<CredentialHandle>(json).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }
}
