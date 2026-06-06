//! # syntheos-contracts
//!
//! Canonical data types and cross-service trait interfaces for the Henosis agent OS.
//!
//! This crate is the substrate every Henosis service binds to. It carries data and
//! interface only: no runtime, no service dependencies, no implementations.
//!
//! ## Wire conventions
//!
//! - IDs are UUID v8, serialized as their canonical hyphenated string (serde transparent).
//! - Timestamps are RFC3339 (e.g. `2026-06-02T13:40:07Z`).
//!
//! These match `syntheos-memory-gateway`'s wire-contract compliance, so the gateway and
//! the contracts agree from day one.

#![deny(missing_docs)]
#![warn(clippy::all)]

pub mod ids;

pub use ids::{EventId, PrincipalId, TaskId, TenantId};

pub mod time;

pub use time::Timestamp;

pub mod principal;
pub mod tenant;

pub use principal::{Principal, PrincipalKind};
pub use tenant::Tenant;

pub mod event;

pub use event::{AxonEnvelope, TypedEvent};

pub mod action;
pub mod task;

pub use action::{RequestContext, ToolInvocation};
pub use task::TaskRef;

pub mod credential;

pub use credential::CredentialHandle;

pub mod gate;

pub use gate::{Gate, GateDecision, GateRequest};

pub mod lifecycle;

pub use lifecycle::{
    ActionCompleted, ActionDenied, ActionFailed, ActionInvoked, ApprovalRequired,
};
