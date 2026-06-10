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
//! ## Phase 0 posture: fail-closed
//!
//! The gate trait and the canonical chain order already exist; the real authorities do not.
//! The dispatcher is fail-closed BY CONSTRUCTION: [`Dispatcher::new`] rejects an empty chain and
//! any chain that is not *exactly* the canonical authority set (`pistis -> plutus -> eidolon ->
//! human -> phylax`, [`dispatcher::CANONICAL_GATE_ORDER`]) -- a missing, duplicated, or misordered
//! authority, or any extra non-canonical gate, is refused. A gate that cannot reach a decision
//! returns `Err`, which the dispatcher also denies (fail-closed). Until real authorities land,
//! the live binary wires [`deny::deny_gate_chain`] and [`deny::DenyExecutor`], denying every
//! action. Allow-all placeholders (`stubs::stub_gate_chain`,
//! `stubs::EchoExecutor`) exist for tests only, behind the non-default `stubs` cargo feature --
//! they never compile into a release build. Real gates and executors swap in by trait object as
//! each authority lands (EidolonGate Phase 2; Pistis/Phylax Phase 3; Plutus Phase 6; Human via
//! Rift/Athena).
//!
//! ## What this is not
//!
//! No real gate or executor logic, no output filtering/redaction (a separate `OutputFilter` seam),
//! no approval *resolution* (the dispatcher only surfaces [`DispatchOutcome::RequiresApproval`] and
//! stops), and no persistence/audit.

pub mod deny;
pub mod dispatcher;
pub mod error;
pub mod executor;
pub mod outcome;
/// Allow-all test placeholders, compiled only for this crate's own tests or under the
/// non-default `stubs` feature -- never into a default or release build.
#[cfg(any(test, feature = "stubs"))]
pub mod stubs;

pub use deny::{deny_gate_chain, DenyExecutor, DenyGate};
pub use dispatcher::{Dispatcher, CANONICAL_GATE_ORDER};
pub use error::DispatchError;
pub use executor::{Executor, ExecutorError};
pub use outcome::DispatchOutcome;
