#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-dispatch
//!
//! The unified action dispatcher for the Henosis agent OS.
//!
//! Every tool/action call in the OS passes through one [`Dispatcher`]. It runs an ordered,
//! fail-closed chain of authorization [`syntheos_contracts::Gate`]s; if every gate allows the
//! action, an [`Executor`] performs it. At each branch the dispatcher emits a lifecycle event
//! (`action.invoked` / `.completed` / `.failed` / `.denied` / `.approval_required`) onto the
//! [`syntheos_axon`] bus, where reactors (narration, evaluation, the future durable audit path)
//! observe the action stream.
//!
//! ## Fail-closed posture
//!
//! The gate trait and the canonical chain order already exist; the real authorities do not.
//! The dispatcher is fail-closed BY CONSTRUCTION: [`Dispatcher::new`] rejects an empty chain and
//! any chain that is not *exactly* the canonical authority set (`pistis -> plutus -> eidolon ->
//! human -> phylaxd`, [`dispatcher::CANONICAL_GATE_ORDER`]) -- a missing, duplicated, or misordered
//! authority, or any extra non-canonical gate, is refused. A gate that cannot reach a decision
//! returns `Err`, which the dispatcher also denies (fail-closed). The fail-closed deny fixtures
//! remain available for isolated tests. Allow-all placeholders (`stubs::stub_gate_chain`,
//! `stubs::EchoExecutor`) exist for tests only, behind the non-default `stubs` cargo feature --
//! they never compile into a release build. The production server supplies all five real gates
//! and its in-process executor through these trait-object seams.
//!
//! ## What this is not
//!
//! Authority, tool, output-filter, approval-resolution, and persistence implementations live in
//! their owning crates. This crate only enforces their ordering and execution lifecycle.

pub mod deny;
pub mod dispatcher;
pub mod error;
pub mod executor;
pub mod guard;
pub mod outcome;
/// Allow-all test placeholders, compiled only for this crate's own tests or under the
/// non-default `stubs` feature -- never into a default or release build.
#[cfg(any(test, feature = "stubs"))]
pub mod stubs;

pub use deny::{DenyExecutor, DenyGate, deny_gate_chain};
pub use dispatcher::{CANONICAL_GATE_ORDER, Dispatcher};
pub use error::DispatchError;
pub use executor::{Executor, ExecutorError};
pub use guard::{ExecutionDecision, ExecutionGuard, ExecutionOutcome};
pub use outcome::DispatchOutcome;
