//! Pistis: the capability authority behind the dispatcher's `pistis` gate slot.
//!
//! An independent, copy-and-own snapshot of the `GhostFrame/pistis` decision
//! core, reworked onto the Henosis principal model. Pistis stays a standalone
//! project; Henosis owns this snapshot of the authorization math the same way it
//! absorbed Eidolon and Phylax.
//!
//! What lives here is the *decision* core: ed25519 admission identity
//! ([`crypto`]), the capability + trust-input taxonomy ([`model`]), a focused
//! materialized [`room::RoomState`], the [`trust`] math, and the
//! [`authority`] capability check. The signed-event replay / quorum / revocation
//! engine that materializes a `RoomState` from a room's history is NOT absorbed
//! -- it stays in Pistis and is reached through the gate's `RoomStateSource`
//! seam.
//!
//! Everything here is fail-closed: an authorization verdict is computed, never
//! assumed, and the gate converts an inability-to-decide into a denial.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod authority;
pub mod crypto;
pub mod error;
pub mod gate;
pub mod model;
pub mod room;
pub mod trust;

pub use authority::{
    authorize_capabilities, CapabilityCheckDecision, CapabilityCheckRequest, CapabilityRequirement,
};
pub use error::{PistisError, Result};
pub use gate::{Clock, PistisGate, RoomStateSource, SystemClock};
pub use model::{
    ActionKind, AdmittedPrincipal, Capability, Outcome, OutcomeAttestation, RoomPolicy,
};
pub use room::RoomState;
pub use trust::compute_trust;
