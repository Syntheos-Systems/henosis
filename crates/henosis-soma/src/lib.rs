#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-soma
//!
//! Soma, the agent presence + quality kernel service, extracted from `kleos-lib` onto the
//! Henosis substrate (Phase 1 Story 1.2, the second kernel service in the workspace).
//!
//! The Kleos soma registry keyed agents on an `i64` surrogate id inside a `user_id: i64` shard.
//! This extraction makes the agent a first-class principal: a registration is Soma's
//! PROJECTION of a canonical [`syntheos_contracts::PrincipalId`] (the projection convention's
//! worked example), so [`AgentPresence`] keys on the agent's own principal id, registration
//! verifies the principal exists in the canonical directory (and never mints one -- enrollment
//! belongs to the authority, Pistis from Phase 2), status is a typed [`PresenceStatus`], and
//! lifecycle changes publish typed events onto the in-process [`syntheos_axon`] bus. No
//! `user_id: i64` survives in any public type.
//!
//! Storage is SQLite via [`SomaStore`], following the kernel-crate migration convention
//! (`PRAGMA user_version` + `migrations/Vn__*.sql`).
//!
//! ## Scope
//!
//! Slices 1-2: presence register/heartbeat/status, reads and listing, stale detection,
//! capability search, quality updates, per-tenant stats, and the one-time legacy absorption
//! backfill ([`backfill`], reusing chiasm's `user_id` map per convention 3.4, with the
//! `soma-backfill` CLI behind the `backfill-cli` feature). Kleos `soma_groups` and
//! `soma_agent_logs` are deliberately NOT ported here -- logs overlap Broca narration
//! (Story 1.3), and groups can follow as a later slice if the OS needs them.

pub mod backfill;
pub mod error;
pub mod events;
pub mod model;
pub mod store;

pub use backfill::{backfill_from_kleos, BackfillOptions, BackfillReport};
pub use error::SomaError;
pub use events::{
    AgentDeregistered, AgentHeartbeat, AgentQualityUpdated, AgentRegistered, AgentStatusChanged,
    AGENT_CHANNEL,
};
pub use model::{
    AgentPresence, PresenceFilter, PresenceStatus, QualityPatch, RegisterAgent, SomaStats,
};
pub use store::SomaStore;
