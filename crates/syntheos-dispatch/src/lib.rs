#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-dispatch
//!
//! The unified action dispatcher for the Henosis agent OS: Phase 0 unit 3.
//!
//! Every tool/action call in the OS passes through one [`Dispatcher`]. It runs an ordered,
//! fail-closed chain of authorization [`syntheos_contracts::Gate`]s; if every gate allows the
//! action, an [`Executor`] performs it. At each branch the dispatcher emits a lifecycle event
//! (`action.invoked` / `.completed` / `.failed` / `.denied` / `.approval_required`) onto the
//! [`syntheos_axon`] bus, where reactors (narration, evaluation, the future durable audit path)
//! observe the action stream.
//!
//! ## Phase 0 posture
//!
//! The gate trait and the canonical chain order already exist; the real authorities do not.
//! [`stubs::stub_gate_chain`] assembles the canonical `pistis -> plutus -> eidolon -> human ->
//! phylax` chain with allow-all stubs, and [`stubs::EchoExecutor`] stands in for a real executor,
//! so the dispatcher runs end-to-end today. Real gates and executors swap in by trait object as
//! each authority lands (EidolonGate Phase 2; Pistis/Phylax Phase 3; Plutus Phase 6; Human via
//! Rift/Athena).
//!
//! ## What this is not
//!
//! No real gate or executor logic, no output filtering/redaction (a separate `OutputFilter` seam),
//! no approval *resolution* (the dispatcher only surfaces [`DispatchOutcome::RequiresApproval`] and
//! stops), and no persistence/audit.

pub mod dispatcher;
pub mod error;
pub mod executor;
pub mod outcome;
pub mod stubs;

pub use dispatcher::Dispatcher;
pub use error::DispatchError;
pub use executor::{Executor, ExecutorError};
pub use outcome::DispatchOutcome;
