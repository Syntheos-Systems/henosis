//! The error type for the absorbed Pistis authority.
//!
//! Deliberately small: the capability-decision core only needs to signal a
//! cryptographic verification failure. Authorization verdicts are NOT errors --
//! they flow back as a [`crate::authority::CapabilityCheckDecision`] (a denied
//! decision is a normal result, not an `Err`). The gate maps a genuine
//! inability-to-decide onto a fail-closed `GateError`; an ordinary deny stays a
//! `GateDecision::Deny`.

use thiserror::Error;

/// A fault in the Pistis cryptographic primitives.
#[derive(Debug, Error)]
pub enum PistisError {
    /// A signature did not validate, or a key was malformed.
    #[error("signature invalid: {0}")]
    SignatureInvalid(String),
    /// A raw room snapshot violated the gate-pinned trust-chain contract.
    #[error("invalid room state: {0}")]
    InvalidRoomState(String),
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, PistisError>;
