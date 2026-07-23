#![deny(missing_docs)]
#![warn(clippy::all)]
//! # henosis-chiasm
//!
//! Chiasm is the task-coordination service for tenant-scoped agent work.
//!
//! A task is owned by a [`syntheos_contracts::PrincipalId`], every
//! read and write is scoped to that owner, identity is a [`syntheos_contracts::TaskId`] (UUID v8),
//! status is a typed [`TaskStatus`], and lifecycle changes publish typed events onto the in-process
//! [`syntheos_axon`] bus.
//!
//! Storage is SQLite via [`ChiasmStore`], versioned with `PRAGMA user_version` and
//! `migrations/Vn__*.sql`.
//!
//! Chiasm provides task CRUD, change history, per-principal statistics, the work queue
//! (enqueue/claim),
//! heartbeat + stale detection, path claims (TTL leases scoped to the owner principal), the
//! dependency DAG (BFS cycle detection + auto-unblock), and the one-time
//! legacy data import ([`backfill`], with the `chiasm-backfill` CLI behind the
//! `backfill-cli` feature). Authentication and LLM planning are handled by their dedicated
//! services.

pub mod backfill;
pub mod error;
pub mod events;
pub mod model;
pub mod store;

pub use backfill::{BackfillOptions, BackfillReport, backfill_from_kleos};
pub use error::ChiasmError;
pub use events::{
    ClaimCreated, ClaimReleased, TASK_CHANNEL, TaskClaimed, TaskCompleted, TaskCreated,
    TaskDeleted, TaskQueued, TaskStale, TaskUnblocked, TaskUpdated,
};
pub use model::{
    ChiasmStats, Dependency, EnqueueTask, NewTask, PathClaim, PathConflict, Task, TaskActivity,
    TaskFilter, TaskPatch, TaskStatus, TaskUpdate,
};
pub use store::ChiasmStore;
