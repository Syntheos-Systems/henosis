//! Phylax: the credential authority absorbed from Kleos onto the Henosis principal model.
//!
//! Phylax holds secrets encrypted at rest and lets an authorized principal USE a secret without
//! ever holding it -- signing, verifying, deriving, or running an allowlisted command with the
//! secret injected -- so credential material never crosses the agent boundary. It backs the
//! dispatcher's `phylax` gate slot.
//!
//! Henosis owns this credential-policy implementation locally while upstream Kleos remains
//! unchanged. This crate is a snapshot reworked onto the principal model
//! (ownership is a [`syntheos_contracts::TenantId`], not a `user_id: i64`), with its own
//! field-level encryption, typed Axon events instead of an audit table, and the phylaxd auth /
//! approval-flow / SSH-CA machinery deliberately left behind (authn is the dispatcher's job;
//! human-in-the-loop is the Human gate's).
//!
//! The crate has three layers: the field-encrypted secret store with its owner-tier
//! administration surface ([`store`]), the capability policies and the four use-without-holding
//! resolve modes ([`policy_store`], [`resolve`]), and the fail-closed
//! [`Gate`](syntheos_contracts::Gate) impl for the dispatcher's phylax slot ([`gate`]).
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod crypto;
pub mod error;
pub mod events;
pub mod gate;
pub mod model;
pub mod policy_store;
pub mod resolve;
pub mod store;

pub use error::PhylaxError;
pub use gate::PhylaxGate;
pub use model::{ExecOutcome, Policy, ResolveMode, SecretData, SignAlgo};
pub use store::PhylaxStore;
