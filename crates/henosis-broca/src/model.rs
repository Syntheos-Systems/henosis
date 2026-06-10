//! The Broca domain types, reshaped onto the Henosis principal model.
//!
//! The Kleos `ActionEntry` carried a stringly `agent` and a `user_id: i64` owner. Here the
//! actor is the agent's own [`PrincipalId`], rows belong to a [`TenantId`], timestamps are
//! [`Timestamp`] (UTC), and the Kleos `axon_event_id` back-reference is gone (the in-process
//! bus is ephemeral; durable correlation is a Phase 2 `syntheos-axon-durable` concern). No
//! `user_id: i64` survives the port.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use syntheos_contracts::{PrincipalId, TenantId, Timestamp};

/// One narrated action: a row of the append-only Broca log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionEntry {
    /// Append-only log id.
    pub id: i64,
    /// Tenant the action belongs to.
    pub tenant: TenantId,
    /// The acting agent's principal.
    pub principal_id: PrincipalId,
    /// Originating service name (e.g. `chiasm`, `soma`).
    pub service: String,
    /// Action type token (e.g. `task.started`).
    pub action: String,
    /// Structured payload.
    pub payload: serde_json::Value,
    /// Human-readable sentence, when one exists (caller-supplied, template, or narrator).
    pub narrative: Option<String>,
    /// When the action was recorded.
    pub created_at: Timestamp,
}

/// Input to [`crate::BrocaStore::log`]: the action to record.
#[derive(Debug, Clone)]
pub struct LogAction {
    /// Tenant the action belongs to.
    pub tenant: TenantId,
    /// The acting agent's principal.
    pub principal_id: PrincipalId,
    /// Originating service name (defaults to `henosis`).
    pub service: Option<String>,
    /// Action type token.
    pub action: String,
    /// Structured payload (defaults to `{}`; must be a JSON object).
    pub payload: Option<serde_json::Value>,
    /// Pre-computed narrative. When `None`, the template renderer is consulted at log time.
    pub narrative: Option<String>,
}

/// Filters for [`crate::BrocaStore::query`]. All filters are AND-combined; `None` = no
/// constraint. Results are newest-first.
#[derive(Debug, Clone, Default)]
pub struct ActionFilter {
    /// Only actions by this principal.
    pub principal_id: Option<PrincipalId>,
    /// Only actions from this service.
    pub service: Option<String>,
    /// Only actions of this type.
    pub action: Option<String>,
    /// Only actions recorded at or after this instant.
    pub since: Option<Timestamp>,
    /// Maximum rows to return (`None` = no limit).
    pub limit: Option<usize>,
    /// Rows to skip (for pagination).
    pub offset: Option<usize>,
}

/// Aggregate action counts for one tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrocaStats {
    /// Total recorded actions.
    pub total: i64,
    /// Count per originating service.
    pub by_service: BTreeMap<String, i64>,
    /// Count per action type.
    pub by_action: BTreeMap<String, i64>,
    /// Count per acting principal (keyed by the principal id string).
    pub by_principal: BTreeMap<String, i64>,
}
