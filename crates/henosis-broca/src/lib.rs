#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-broca
//!
//! Broca, the action-narration kernel service, extracted from `kleos-lib` onto the Henosis
//! substrate (Phase 1 Story 1.3, the third kernel service in the workspace).
//!
//! The Kleos broca log keyed rows on a stringly `agent` inside a `user_id: i64` shard. This
//! extraction puts the actor on the principal model: an [`ActionEntry`] records WHO (the
//! acting agent's [`syntheos_contracts::PrincipalId`]), WHERE (a
//! [`syntheos_contracts::TenantId`]-scoped feed), and WHAT (service + action + payload), and
//! every logged action publishes a typed `narration.logged` event onto the in-process
//! [`syntheos_axon`] bus. No `user_id: i64` survives in any public type.
//!
//! Narration is layered: caller-supplied sentence, else the ported template renderer
//! ([`narrate_from_template`]), else the pluggable async [`Narrator`] seam -- filled by a
//! Synapse/Foundry-backed implementation at server wiring time (Phase 4), exactly the
//! evolve-without-breaking pattern of the dispatcher's `OutputFilter` slot. Storage is SQLite
//! via [`BrocaStore`], following the kernel-crate migration convention.
//!
//! ## Scope
//!
//! Slice 1 (this commit): the action log (log/get/query/stats), template narration, the
//! `Narrator` seam with lazy [`BrocaStore::get_or_narrate`]. NOT ported here: the Kleos `ask`
//! LLM-to-SQL surface (Synapse-coupled, Phase 4) and the legacy backfill (a later slice if the
//! historical Kleos feed proves worth absorbing -- narration is telemetry, not record).

pub mod error;
pub mod events;
pub mod model;
pub mod narrate;
pub mod store;

pub use error::BrocaError;
pub use events::{ActionLogged, NARRATION_CHANNEL};
pub use model::{ActionEntry, ActionFilter, BrocaStats, LogAction};
pub use narrate::{narrate_from_template, Narrator};
pub use store::BrocaStore;
