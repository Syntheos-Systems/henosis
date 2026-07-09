//! Plutus billing surface (Story 6.4a): Stripe-facing billing primitives.
//!
//! Story 6.4a scopes this module to webhook signature verification only
//! ([`signature`]); later billing stories layer subscription sync, invoice ingestion, and
//! usage-based billing hooks on top without touching this module's contract.

pub mod signature;

pub use signature::{verify_stripe_signature, SignatureError, DEFAULT_TOLERANCE_SECS};
