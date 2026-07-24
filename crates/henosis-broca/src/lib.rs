#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-broca
//!
//! Broca records tenant-scoped agent actions and optionally turns them into concise narration.
//!
//! An [`ActionEntry`] records WHO (the
//! acting agent's [`syntheos_contracts::PrincipalId`]), WHERE (a
//! [`syntheos_contracts::TenantId`]-scoped feed), and WHAT (service + action + payload), and
//! every logged action publishes a typed `narration.logged` event onto the in-process
//! [`syntheos_axon`] bus. No `user_id: i64` survives in any public type.
//!
//! Narration is layered: caller-supplied sentence, else the template renderer
//! ([`narrate_from_template`]), else the pluggable async [`Narrator`] seam -- filled by a
//! Synapse/Foundry-backed implementation at server wiring time. Storage is SQLite
//! via [`BrocaStore`] with a versioned SQLite schema.
//!
//! The service provides action logging, query and statistics operations, template narration,
//! and lazy narration through [`BrocaStore::get_or_narrate`].

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
