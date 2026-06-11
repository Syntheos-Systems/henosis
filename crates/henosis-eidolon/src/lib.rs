#![deny(missing_docs)]
//! Eidolon: the policy authority for the dispatcher's `eidolon` gate slot.
//!
//! [`EidolonGate`] is a [`syntheos_contracts::Gate`] implementation that runs three check
//! families against every request, in order:
//!
//! 1. **Scope violation** -- the invocation payload must not address a tenant or principal other
//!    than the one in the [`syntheos_contracts::RequestContext`].
//! 2. **Prompt injection** -- the invocation payload must not contain a forbidden pattern
//!    (case- and whitespace-insensitive substring match against [`EidolonPolicy`] patterns).
//! 3. **Persona drift** -- the requesting principal's active drift flags, read through the
//!    [`DriftSignal`] seam, must all sit below the policy's deny threshold.
//!
//! Fail-closed by type: a check that cannot complete (the drift authority unreachable) returns
//! [`syntheos_contracts::GateError`], which the dispatcher denies on. No internal error path can
//! produce an `Allow`.
//!
//! The drift signal is a trait seam, consistent with the kernel convention (kernel crates never
//! depend on each other): the server adapts `ThymusStore` to [`DriftSignal`] at wiring time
//! (Story 2.6), exactly as Soma is adapted to Thymus's `QualitySink`.
//!
//! [`EidolonOutputFilter`] is the OUTPUT side of the same authority (the dispatcher's
//! `with_output_filter` slot): it scrubs credential-bearing fields from executor results per the
//! same shared [`EidolonPolicy`], so input policy and output redaction are configured once.

mod gate;
mod output_filter;
mod policy;
mod signal;
pub mod supervisor;

pub use gate::EidolonGate;
pub use output_filter::{EidolonOutputFilter, REDACTED};
pub use policy::{
    default_injection_patterns, default_sensitive_fields, DriftSeverity, EidolonError,
    EidolonPolicy,
};
pub use signal::{DriftFlag, DriftSignal};
