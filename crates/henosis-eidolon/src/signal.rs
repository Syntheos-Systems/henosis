//! The drift-signal seam: where the gate reads the requesting principal's active drift state.
//!
//! The server adapts `ThymusStore` to this trait at wiring time, the same pattern as
//! Thymus's `QualitySink` (adapted over Soma), so `henosis-eidolon` never depends on
//! `henosis-thymus`.

use async_trait::async_trait;
use syntheos_contracts::{PrincipalId, TenantId};

use crate::policy::DriftSeverity;

/// One active drift observation for a principal, as seen by the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftFlag {
    /// The drift category token (e.g. `safety`, `priority`). Opaque to the gate; logged in the
    /// deny reason.
    pub drift_type: String,
    /// How serious it is, mapped to Eidolon's severity scale by the adapter.
    pub severity: DriftSeverity,
}

/// Read access to a principal's active drift flags.
///
/// An `Err` means the drift authority could not answer (unreachable, backend failure). The gate
/// converts it to a `GateError`, which the dispatcher denies on -- an unreachable authority must
/// never degrade to an unchecked Allow.
#[async_trait]
pub trait DriftSignal: Send + Sync {
    /// The active drift flags for `agent` within `tenant`. An empty vec means no known drift.
    async fn active_drift(
        &self,
        tenant: TenantId,
        agent: PrincipalId,
    ) -> Result<Vec<DriftFlag>, String>;
}
