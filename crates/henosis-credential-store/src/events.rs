//! Credential lifecycle events published onto the Axon bus.
//!
//! They implement the contracts' [`TypedEvent`] trait so any in-process reactor (narration,
//! the durable audit path) can subscribe without depending on this crate. Payloads carry
//! identifying strings and outcomes only -- never secret material, key bytes, signatures, or
//! derived output. This is the audit trail; the secret never lands on the bus.

use serde::{Deserialize, Serialize};
use syntheos_contracts::TypedEvent;

/// The channel every embedded credential-store event travels on.
pub const CREDENTIAL_STORE_CHANNEL: &str = "credential_store";

/// A secret was created or overwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretStored {
    /// The secret's category.
    pub category: String,
    /// The secret's name.
    pub name: String,
}

/// Emit `SecretStored`.
impl TypedEvent for SecretStored {
    const CHANNEL: &'static str = CREDENTIAL_STORE_CHANNEL;
    const KIND: &'static str = "secret.stored";
}

/// A secret was deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretDeleted {
    /// The deleted secret's category.
    pub category: String,
    /// The deleted secret's name.
    pub name: String,
}

/// Emit `SecretDeleted`.
impl TypedEvent for SecretDeleted {
    const CHANNEL: &'static str = CREDENTIAL_STORE_CHANNEL;
    const KIND: &'static str = "secret.deleted";
}
