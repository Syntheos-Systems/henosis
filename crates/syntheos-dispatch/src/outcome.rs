//! The result of a dispatch: executed, denied, or awaiting approval.

use serde::{Deserialize, Serialize};

/// What [`crate::Dispatcher::dispatch`] returns when the request was handled without an
/// execution failure.
///
/// Denied / RequiresApproval are *outcomes*, not errors -- a gate doing its job is the normal
/// path, not a fault. Genuine execution failures surface as [`crate::DispatchError`] instead.
///
/// This derives `Serialize`/`Deserialize` for logging and persistence only; like
/// [`syntheos_contracts::GateDecision`] it carries no cross-version wire guarantee.
///
/// `#[non_exhaustive]`: outcomes may grow; downstream matches must keep a wildcard arm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DispatchOutcome {
    /// Every gate allowed the action and it executed; carries the executor's result.
    Executed {
        /// The executor's JSON result.
        result: serde_json::Value,
    },
    /// A gate rejected the action before execution.
    Denied {
        /// The gate that denied it (its [`syntheos_contracts::Gate::name`]).
        gate: String,
        /// Why it was denied.
        reason: String,
    },
    /// A gate escalated the action for explicit approval; it did not execute.
    RequiresApproval {
        /// The gate that requested approval.
        gate: String,
        /// The prompt to present to the approver.
        prompt: String,
    },
}
