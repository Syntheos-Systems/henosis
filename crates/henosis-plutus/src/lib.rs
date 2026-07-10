//! `henosis-plutus`: the policy authority for the dispatcher gate chain.
//!
//! Provides [`PlutusStore`] (orgs, memberships, roles, quotas, usage) and
//! [`PlutusGate`], the real `Gate` that replaces the plutus deny-stub. The
//! [`billing`] module hosts Stripe-facing billing primitives, starting with
//! webhook signature verification.
//!
//! The gate check is a four-step fail-closed pipeline:
//! 1. Org must exist and be active.
//! 2. Principal must be a member whose role permits the action.
//! 3. Hard daily quota for the action's dimension must not be exceeded.
//! 4. Per-org token-bucket rate limit must not be exhausted.
//!
//! Any backing-store error returns `Err(GateError)`; the dispatcher denies on error.
//! There is no code path that returns `GateDecision::Allow` when an authority errored.

pub mod action_map;
pub mod backend;
pub mod billing;
pub mod gate;
pub mod quota;
pub mod rbac;
pub mod store;

pub use action_map::{map_invocation, ActionClass};
pub use backend::{OrgStatus, PolicyBackend};
#[cfg(any(test, feature = "test-helpers"))]
pub use backend::MockPolicyBackend;
pub use billing::{
    apply_decision, decide, parse_event, verify_stripe_signature, BillingDecision,
    BillingEventRecord, BillingOutcome, DecideError, Entitlement, EntitlementSource,
    EntitlementStatus, SignatureError, StripeEvent, DEFAULT_TOLERANCE_SECS,
};
pub use gate::{Clock, PlutusGate, WallClock};
#[cfg(any(test, feature = "test-helpers"))]
pub use gate::FrozenClock;
pub use quota::{QuotaConfig, QuotaDimension, QuotaOutcome, QuotaTier};
pub use rbac::{can, Permission, Role};
pub use store::PlutusStore;

/// Errors surfaced by the Plutus authority.
#[derive(Debug, thiserror::Error)]
pub enum PlutusError {
    /// A database operation failed.
    #[error("plutus store: {0}")]
    Store(String),
    /// A migration could not be applied.
    #[error("plutus migration: {0}")]
    Migration(String),
    /// A configuration invariant was violated (e.g. missing quota config for an org).
    #[error("plutus config: {0}")]
    Config(String),
}

/// Convert a `sqlx::Error` into a `PlutusError::Store`.
impl From<sqlx::Error> for PlutusError {
    /// Wrap the sqlx error message in the store variant.
    fn from(e: sqlx::Error) -> Self {
        PlutusError::Store(e.to_string())
    }
}

/// Convert a `sqlx::migrate::MigrateError` into a `PlutusError::Migration`.
impl From<sqlx::migrate::MigrateError> for PlutusError {
    /// Wrap the migration error message.
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        PlutusError::Migration(e.to_string())
    }
}

/// The crate-level result type.
pub type Result<T> = std::result::Result<T, PlutusError>;

#[cfg(test)]
mod tests {
    /// The crate is wired into the workspace and its error type constructs correctly.
    #[test]
    fn crate_builds_and_error_constructs() {
        let e = super::PlutusError::Store("x".into());
        assert!(e.to_string().contains("plutus store"));
    }
}
