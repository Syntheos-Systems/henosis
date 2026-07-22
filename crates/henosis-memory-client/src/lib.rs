//! `henosis-memory-client` -- transitional HTTP client + software Ed25519 request
//! signer for Synapse's memory/coordination access during absorption (Story 4.1).
//!
//! Copy-and-owned from `kleos-client` + `kleos-lib::{auth_piv, cred::bootstrap}`,
//! reduced to exactly the surface Synapse's memory tools call. Software keys only
//! (no PIV/YubiKey/PKCS#11 runtime dependency, no ECDH bootstrap). This bridge is
//! retired once in-process kernel dispatch lands (Wave 5-6); keep it minimal.

/// Bootstrap-bearer resolver (cred/phylaxd) and agent-slot helper.
pub mod bootstrap;
/// Generic string-path HTTP client over the KLEOSv1 signing protocol.
pub mod client;
/// Local error type (replaces kleos-lib's `EngError`).
pub mod error;
/// Software Ed25519 request signer plus session/replay primitives.
pub mod signer;

/// The HTTP client Synapse's memory tools dispatch through.
pub use client::Client;
/// Crate error + result alias.
pub use error::{MemoryClientError, Result};
/// Signer types consumed at the Synapse bootstrap call sites.
pub use signer::{RequestSigner, SignatureAlgo, SignedRequest};
