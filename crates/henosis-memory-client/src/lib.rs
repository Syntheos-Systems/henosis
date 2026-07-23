//! HTTP client and software Ed25519 request signer for Synapse memory and coordination access.
//!
//! The crate exposes only the surface used by Synapse memory tools. It supports software keys
//! without a PIV, YubiKey, PKCS#11, or ECDH bootstrap dependency.

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
