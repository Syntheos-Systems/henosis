//! Phylax: the credential authority absorbed from Kleos onto the Henosis principal model.
//!
//! Phylax holds secrets encrypted at rest and lets an authorized principal USE a secret without
//! ever holding it -- signing, verifying, deriving, or running an allowlisted command with the
//! secret injected -- so credential material never crosses the agent boundary. It backs the
//! dispatcher's `phylax` gate slot.
//!
//! This is a copy-and-own absorption (the agent-forge pattern): Kleos keeps shipping `kleos-phylax`
//! standalone, while this crate is Henosis's own snapshot reworked onto the principal model
//! (ownership is a [`syntheos_contracts::TenantId`], not a `user_id: i64`), with its own
//! field-level encryption, typed Axon events instead of an audit table, and the credd auth /
//! approval-flow / SSH-CA machinery deliberately left behind (authn is the dispatcher's job;
//! human-in-the-loop is the Human gate's).
//!
//! Slice 1 (this commit) lands the encrypted secret store and its owner-tier administration
//! surface. Capability policies, the four resolve modes (sign/verify/derive/exec), and the
//! fail-closed [`Gate`](syntheos_contracts::Gate) impl follow in later slices.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod crypto;
pub mod error;
pub mod events;
pub mod model;
pub mod policy_store;
pub mod resolve;
pub mod store;

pub use error::PhylaxError;
pub use model::{ExecOutcome, Policy, ResolveMode, SecretData, SignAlgo};
pub use store::PhylaxStore;
