//! HumanGate over Rift approvals.
//!
//! The `human` slot of the dispatcher's canonical gate chain. [`HumanGate`]
//! escalates approval-required actions to a human via Rift (an Axon
//! notification) and blocks on an [`Approver`] until they approve or the request
//! times out -- fail-closed (a timeout denies). [`RegistryApprover`] is the
//! out-of-band approval channel the server wires to Rift; tests inject their own
//! [`Approver`].
//!
//! The standalone Rift server and bridge live in `henosis-rift-server` and
//! `henosis-rift-bridge`; this
//! crate is the gate that governs the dispatcher's `human` slot.

pub mod approver;
pub mod gate;

pub use approver::RegistryApprover;
pub use gate::{
    approval_prompt, requires_human_approval, ApprovalDecision, ApprovalRequest, Approver,
    HumanApprovalRequested, HumanGate, HUMAN_CHANNEL,
};
