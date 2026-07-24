//! Stripe-facing billing primitives for Plutus.
//!
//! [`signature`] verifies inbound Stripe webhook deliveries; [`pipeline`] then decides what a
//! verified event means and applies it to an org's entitlement, tier, and quota. This module
//! also hosts the entitlement domain types that back `org.plan_tier`: [`Entitlement`] (a tier
//! grant sourced from either a live Stripe subscription or a manual operator grant),
//! [`EntitlementStatus`], [`EntitlementSource`], and [`BillingEventRecord`] (the
//! idempotency-log row shape). The corresponding storage lives on `PlutusStore` in
//! `crate::store`; this module owns only the pure data types and their text-form parsing,
//! mirroring how `crate::quota` owns `QuotaTier`.

pub mod pipeline;
pub mod signature;

pub use pipeline::{
    apply_decision, decide, parse_event, BillingDecision, BillingOutcome, DecideError, StripeEvent,
};
#[cfg(any(test, feature = "test-helpers"))]
pub use signature::sign_stripe_payload;
pub use signature::{verify_stripe_signature, SignatureError, DEFAULT_TOLERANCE_SECS};

use std::fmt;
use std::str::FromStr;

use syntheos_contracts::TenantId;

use crate::quota::QuotaTier;

/// The lifecycle status of an [`Entitlement`], mirroring the Stripe subscription states
/// Plutus cares about (or `Active` for a manual grant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntitlementStatus {
    /// The entitlement is in force; the tenant's tier reflects it.
    Active,
    /// The underlying Stripe subscription has a failed payment but has not yet been canceled.
    PastDue,
    /// The entitlement has been canceled and no longer grants its tier.
    Canceled,
}

/// Display an `EntitlementStatus` as its canonical lowercase text (matches the DB column value).
impl fmt::Display for EntitlementStatus {
    /// Write the canonical status string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error returned when an entitlement status string is not a recognized variant.
#[derive(Debug)]
pub struct EntitlementStatusParseError(String);

/// Display an entitlement-status parse error, naming the unrecognized input.
impl fmt::Display for EntitlementStatusParseError {
    /// Write the unrecognized status string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown entitlement status: {:?}", self.0)
    }
}

/// Parse an `EntitlementStatus` from its canonical text form.
impl FromStr for EntitlementStatus {
    /// Entitlement-status parse error.
    type Err = EntitlementStatusParseError;

    /// Parse the canonical lowercase status name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(EntitlementStatus::Active),
            "past_due" => Ok(EntitlementStatus::PastDue),
            "canceled" => Ok(EntitlementStatus::Canceled),
            other => Err(EntitlementStatusParseError(other.to_string())),
        }
    }
}

/// `EntitlementStatus` methods.
impl EntitlementStatus {
    /// Return the canonical text representation stored in the `entitlement.status` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            EntitlementStatus::Active => "active",
            EntitlementStatus::PastDue => "past_due",
            EntitlementStatus::Canceled => "canceled",
        }
    }
}

/// Where an [`Entitlement`] originated: a live Stripe subscription or an operator-issued
/// manual grant (e.g. a comped account, an internal test org).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntitlementSource {
    /// The entitlement is driven by a Stripe subscription; `stripe_subscription_id` is set.
    Stripe,
    /// The entitlement was granted manually with no backing Stripe subscription.
    Manual,
}

/// Display an `EntitlementSource` as its canonical lowercase text (matches the DB column value).
impl fmt::Display for EntitlementSource {
    /// Write the canonical source string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error returned when an entitlement source string is not a recognized variant.
#[derive(Debug)]
pub struct EntitlementSourceParseError(String);

/// Display an entitlement-source parse error, naming the unrecognized input.
impl fmt::Display for EntitlementSourceParseError {
    /// Write the unrecognized source string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown entitlement source: {:?}", self.0)
    }
}

/// Parse an `EntitlementSource` from its canonical text form.
impl FromStr for EntitlementSource {
    /// Entitlement-source parse error.
    type Err = EntitlementSourceParseError;

    /// Parse the canonical lowercase source name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stripe" => Ok(EntitlementSource::Stripe),
            "manual" => Ok(EntitlementSource::Manual),
            other => Err(EntitlementSourceParseError(other.to_string())),
        }
    }
}

/// `EntitlementSource` methods.
impl EntitlementSource {
    /// Return the canonical text representation stored in the `entitlement.source` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            EntitlementSource::Stripe => "stripe",
            EntitlementSource::Manual => "manual",
        }
    }
}

/// A tier grant backing a tenant's `org.plan_tier`, sourced from either a Stripe
/// subscription or a manual operator grant. Mirrors one row of the `entitlement` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entitlement {
    /// The `entitlement.id` primary key (BIGSERIAL).
    pub id: i64,
    /// The tenant (org) this entitlement grants a tier to.
    pub tenant_id: TenantId,
    /// The quota tier this entitlement grants.
    pub tier: QuotaTier,
    /// Whether this entitlement came from Stripe or a manual grant.
    pub source: EntitlementSource,
    /// The Stripe subscription id backing this entitlement, or `None` for a manual grant.
    pub stripe_subscription_id: Option<String>,
    /// The current lifecycle status of this entitlement.
    pub status: EntitlementStatus,
    /// The RFC3339 end of the current billing period, or `None` when not applicable
    /// (e.g. a manual grant has no billing period).
    pub current_period_end: Option<String>,
}

/// A processed Stripe webhook event, as logged in the `billing_event` idempotency table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingEventRecord {
    /// The Stripe event id (e.g. `evt_...`), the table's primary key.
    pub event_id: String,
    /// The Stripe event type (e.g. `customer.subscription.updated`).
    pub event_type: String,
    /// A short human-readable description of how the event was handled, or `None` when
    /// the event has not yet been processed.
    pub outcome: Option<String>,
}
