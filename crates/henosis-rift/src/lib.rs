//! HumanGate over Rift approvals (Story 4.5).
//!
//! The `human` slot of the dispatcher's canonical gate chain. [`HumanGate`]
//! escalates approval-required actions to a human via Rift (an Axon
//! notification) and blocks on an [`Approver`] until they approve or the request
//! times out -- fail-closed (a timeout denies). [`RegistryApprover`] is the
//! out-of-band approval channel the server wires to Rift; tests inject their own
//! [`Approver`].
//!
//! The Rift HumanGate crate the roadmap references as `crates/henosis-rift`. The
//! standalone Rift server/bridge live in `henosis-rift-server`/-`bridge`; this
//! crate is the gate that governs the dispatcher's `human` slot.

pub mod approver;
pub mod gate;

pub use approver::RegistryApprover;
pub use gate::{
    ApprovalDecision, ApprovalRequest, Approver, HumanApprovalRequested, HumanGate, HUMAN_CHANNEL,
};
