//! Embedded credential compatibility storage for local Henosis integrations.
//!
//! The store encrypts secrets at rest and lets an authorized principal use a secret without ever
//! holding it. Supported operations include signing, verification, derivation, and allowlisted
//! command execution with injected secret material.
//!
//! This crate supports loopback integrations and tests. Production credential operations cross
//! the authenticated `phylaxd` broker boundary instead. Ownership is scoped by
//! [`syntheos_contracts::TenantId`], events are typed, authentication belongs to the dispatcher,
//! and approval policy belongs to the Human gate.
//!
//! The crate has three layers: the field-encrypted secret store with its owner-tier
//! administration surface ([`store`]), the capability policies and the four use-without-holding
//! resolve modes ([`policy_store`], [`resolve`]), and the fail-closed
//! [`Gate`](syntheos_contracts::Gate) implementation for the broker slot ([`gate`]).
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

pub use error::CredentialStoreError;
pub use gate::CredentialStoreGate;
pub use model::{ExecOutcome, Policy, ResolveMode, SecretData, SignAlgo};
pub use store::CredentialStore;
