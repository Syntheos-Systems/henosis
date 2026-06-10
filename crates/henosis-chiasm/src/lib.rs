#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-chiasm
//!
//! Chiasm, the task-coordination kernel service, extracted from `kleos-lib` onto the Henosis
//! substrate (Phase 1, the first kernel service to land in the workspace).
//!
//! The Kleos chiasm service keyed every task on `user_id: i64`. This extraction replaces that with
//! the canonical principal model: a task is owned by a [`syntheos_contracts::PrincipalId`], every
//! read and write is scoped to that owner, identity is a [`syntheos_contracts::TaskId`] (UUID v8),
//! status is a typed [`TaskStatus`], and lifecycle changes publish typed events onto the in-process
//! [`syntheos_axon`] bus. No `user_id: i64` survives in any public type, per the
//! PrincipalProjection convention.
//!
//! Storage is SQLite via [`ChiasmStore`], following the kernel-crate migration convention
//! (`PRAGMA user_version` + `migrations/Vn__*.sql`).
//!
//! ## Scope
//!
//! Slices 1-4: task CRUD, change history, per-principal stats, the work queue (enqueue/claim),
//! heartbeat + stale detection, path claims (TTL leases scoped to the owner principal), the
//! dependency DAG (BFS cycle detection + auto-unblock), and the one-time
//! `user_id -> PrincipalId` legacy absorption backfill ([`backfill`], with the `chiasm-backfill`
//! CLI behind the `backfill-cli` feature). Agent bearer keys (`keys.rs` in Kleos) are
//! deliberately NOT ported here -- they are an authentication artifact that belongs to the security
//! authorities (Pistis/Phylax), not the task service. LLM plan generation defers to the Broca
//! extraction.

pub mod backfill;
pub mod error;
pub mod events;
pub mod model;
pub mod store;

pub use backfill::{backfill_from_kleos, BackfillOptions, BackfillReport};
pub use error::ChiasmError;
pub use events::{
    ClaimCreated, ClaimReleased, TaskClaimed, TaskCompleted, TaskCreated, TaskDeleted, TaskQueued,
    TaskStale, TaskUnblocked, TaskUpdated, TASK_CHANNEL,
};
pub use model::{
    ChiasmStats, Dependency, EnqueueTask, NewTask, PathClaim, PathConflict, Task, TaskFilter,
    TaskPatch, TaskStatus, TaskUpdate,
};
pub use store::ChiasmStore;
