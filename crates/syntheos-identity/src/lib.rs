#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-identity
//!
//! The principal directory for the Henosis agent OS: Phase 0 unit 4.
//!
//! [`syntheos_contracts::Principal`] is the canonical actor; this crate is where actors are
//! enrolled and looked up. A gate turns the `PrincipalId` carried in a
//! [`syntheos_contracts::GateRequest`] into a full actor by consulting a
//! [`PrincipalDirectory`]. The trait is async and `Result`-returning so the Phase 0
//! [`InMemoryDirectory`] and the persistent [`SqliteDirectory`] are interchangeable behind the
//! same surface, without changing any call site.
//!
//! Two backends ship: [`InMemoryDirectory`] (process-local, no persistence -- for tests and the
//! earliest boot) and [`SqliteDirectory`] (the unit-6 DB decision -- persists across restarts,
//! which the `user_id -> PrincipalId` backfill and Phase 1 extraction require).
//!
//! ## What this is not
//!
//! Not transport/request authentication -- KLEOSv1 request signing (Ed25519 + PIV P-256) lives in
//! `syntheos-memory-gateway` and maps a signed request to a principal; this is the principal model
//! it maps onto. Not tenancy, roles, quota, grants, or scopes (Plutus / Pistis / Phylax own
//! those). No update, delete, or revocation yet.

pub mod accounts;
/// Persistent machine-token and operator-refresh authority credential storage.
pub mod authority;
pub mod directory;
pub mod error;
pub mod sqlite;

pub use accounts::OperatorAccount;
pub use authority::{
    MachineToken, MachineTokenIssued, OperatorRefreshSession, RefreshSessionIssued,
};
pub use directory::{InMemoryDirectory, PrincipalDirectory};
pub use error::DirectoryError;
pub use sqlite::SqliteDirectory;
