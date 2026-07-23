#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-soma
//!
//! Soma, the agent presence and quality kernel service for the Henosis substrate.
//!
//! The Kleos soma registry keyed agents on an `i64` surrogate id inside a `user_id: i64` shard.
//! This extraction makes the agent a first-class principal: a registration is Soma's
//! PROJECTION of a canonical [`syntheos_contracts::PrincipalId`] (the projection convention's
//! worked example), so [`AgentPresence`] keys on the agent's own principal id, registration
//! verifies the principal exists in the canonical directory (and never mints one -- enrollment
//! belongs to the authority), status is a typed [`PresenceStatus`], and
//! lifecycle changes publish typed events onto the in-process [`syntheos_axon`] bus. No
//! `user_id: i64` survives in any public type.
//!
//! Storage is SQLite via [`SomaStore`], following the kernel-crate migration convention
//! (`PRAGMA user_version` + `migrations/Vn__*.sql`).
//!
//! ## Scope
//!
//! Soma provides presence registration, heartbeats, status, reads and listing, stale detection,
//! capability search, quality updates, per-tenant stats, and a one-time legacy-data backfill
//! ([`backfill`], with the `soma-backfill` CLI behind the `backfill-cli` feature). It does not
//! manage groups or activity logs; Broca owns narration.

pub mod backfill;
pub mod error;
pub mod events;
pub mod model;
pub mod store;

pub use backfill::{BackfillOptions, BackfillReport, backfill_from_kleos};
pub use error::SomaError;
pub use events::{
    AGENT_CHANNEL, AgentDeregistered, AgentHeartbeat, AgentQualityUpdated, AgentRegistered,
    AgentStatusChanged,
};
pub use model::{
    AgentPresence, PresenceFilter, PresenceStatus, QualityPatch, RegisterAgent, SomaStats,
};
pub use store::SomaStore;
