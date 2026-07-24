//! Pistis: the capability authority behind the dispatcher's `pistis` gate slot.
//!
//! Henosis owns this compatibility decision core on its principal model. The
//! full proprietary Pistis service is not distributed in this repository.
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
pub use gate::{
    Clock, InMemoryRoomStateSource, PistisGate, RoomStateSource, SystemClock, ToolActionPolicy,
};
pub use model::{
    ActionKind, AdmittedPrincipal, Capability, Outcome, OutcomeAttestation, OutcomeStatement,
    RoomManifest, RoomPolicy, RoomScope,
};
pub use room::{RoomState, RoomTrustStore, VerifiedRoomState};
pub use trust::compute_trust;
