//! The dispatcher's error type: construction-time chain validation failures and an action
//! genuinely failing to run.

use crate::executor::ExecutorError;

/// A dispatcher failed to construct, or a dispatch failed to run.
///
/// Authorization decisions (deny, approval) are not errors -- they ride
/// [`crate::DispatchOutcome`]. `DispatchError` covers two things: a gate chain that is invalid
/// at construction time (the dispatcher is fail-closed BY CONSTRUCTION, so an empty or
/// non-canonical chain can never become a runnable dispatcher), and the action failing to
/// execute after it cleared every gate.
///
/// `#[non_exhaustive]`: failure modes may grow as real executors and a durable audit path land.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DispatchError {
    /// Construction was attempted with no gates at all. An ungated dispatcher would execute
    /// every action unconditionally, so this is rejected outright.
    #[error("gate chain is empty: a dispatcher without gates would allow everything (fail-closed)")]
    EmptyGateChain,

    /// Construction was attempted with a chain that is not *exactly* the canonical authority set:
    /// a missing authority, a duplicate, a misordering, or an extra non-canonical gate.
    #[error(
        "gate chain is not canonical: expected exactly the authority order {expected:?}, got {got:?}"
    )]
    NonCanonicalChain {
        /// The canonical authority order every chain must be, exactly.
        expected: Vec<String>,
        /// The gate names the rejected chain actually presented, in order.
        got: Vec<String>,
    },

    /// The action passed every gate but failed during execution.
    #[error("action execution failed: {0}")]
    Execution(#[from] ExecutorError),
}
