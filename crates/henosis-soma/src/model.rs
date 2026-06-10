//! The Soma domain types, reshaped onto the Henosis principal model.
//!
//! The Kleos `Agent` carried an `id: i64` surrogate key, a stringly `status`, and a
//! `user_id: i64` owner. Here the agent IS a canonical principal: [`AgentPresence`] is keyed by
//! the agent's own [`PrincipalId`], status is a typed [`PresenceStatus`], timestamps are
//! [`Timestamp`] (UTC), and drift flags are a typed `Vec<String>`. No `user_id: i64` survives
//! the port.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use syntheos_contracts::{PrincipalId, TenantId, Timestamp};

use crate::error::SomaError;

/// The liveness state of a registered agent.
///
/// Serializes snake_case (`pending`, `online`, ...), matching the Kleos status strings so the
/// legacy backfill maps one-to-one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceStatus {
    /// Registered but not yet seen alive.
    Pending,
    /// Heartbeating within its window.
    Online,
    /// Known to be away (explicitly set, or staled by a sweeper).
    Offline,
    /// The agent reported a fault.
    Error,
}

impl PresenceStatus {
    /// The canonical storage/wire token for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            PresenceStatus::Pending => "pending",
            PresenceStatus::Online => "online",
            PresenceStatus::Offline => "offline",
            PresenceStatus::Error => "error",
        }
    }

    /// Parse a status token, rejecting anything unknown ([`SomaError::InvalidStatus`]).
    pub fn parse(s: &str) -> Result<Self, SomaError> {
        match s {
            "pending" => Ok(PresenceStatus::Pending),
            "online" => Ok(PresenceStatus::Online),
            "offline" => Ok(PresenceStatus::Offline),
            "error" => Ok(PresenceStatus::Error),
            other => Err(SomaError::InvalidStatus(other.to_string())),
        }
    }
}

/// A registered agent's presence record: Soma's projection of a canonical principal.
///
/// This is NOT the principal itself -- `syntheos-identity` owns that record exclusively.
/// `principal_id` is the only link back to it (projection convention section 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPresence {
    /// The agent's own canonical principal id. One presence row per agent.
    pub principal_id: PrincipalId,
    /// Tenant the registration belongs to (Axon envelope scope).
    pub tenant: TenantId,
    /// Working label for the agent (unique per tenant).
    pub name: String,
    /// Coarse category (e.g. `coding`, `cli`).
    pub agent_type: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Capability strings advertised by the agent (stored as a JSON array).
    pub capabilities: Vec<String>,
    /// Current liveness state.
    pub status: PresenceStatus,
    /// Agent-specific configuration (an arbitrary JSON object).
    pub config: serde_json::Value,
    /// Last heartbeat, or `None` if never beaten.
    pub heartbeat_at: Option<Timestamp>,
    /// Latest quality score from evaluation (Thymus), if any.
    pub quality_score: Option<f64>,
    /// Drift-flag strings raised by supervision.
    pub drift_flags: Vec<String>,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-modification time.
    pub updated_at: Timestamp,
}

/// The fields required to register (or re-register) an agent's presence.
///
/// `principal_id` must already be enrolled in the canonical directory -- registration verifies
/// and never mints (projection convention section 1). Re-registering the same principal
/// updates its label/type/description/capabilities/config in place, preserving status,
/// heartbeat, and quality (the Kleos upsert-by-name semantics, keyed by principal instead).
#[derive(Debug, Clone)]
pub struct RegisterAgent {
    /// The agent's canonical principal id.
    pub principal_id: PrincipalId,
    /// Tenant the registration belongs to.
    pub tenant: TenantId,
    /// Working label (unique per tenant).
    pub name: String,
    /// Coarse category.
    pub agent_type: String,
    /// Optional description.
    pub description: Option<String>,
    /// Capability strings (defaults to none).
    pub capabilities: Option<Vec<String>>,
    /// Agent-specific configuration object (defaults to `{}`).
    pub config: Option<serde_json::Value>,
}

/// Filters for [`crate::SomaStore::list`]. All filters are AND-combined; `None` = no constraint.
#[derive(Debug, Clone, Default)]
pub struct PresenceFilter {
    /// Only agents of this type.
    pub agent_type: Option<String>,
    /// Only agents in this status.
    pub status: Option<PresenceStatus>,
    /// Maximum rows to return (`None` = no limit).
    pub limit: Option<usize>,
}

/// Aggregate presence counts for one tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SomaStats {
    /// Total registered agents.
    pub total: i64,
    /// Agents currently `online`.
    pub online: i64,
    /// Count per agent type.
    pub by_type: BTreeMap<String, i64>,
    /// Count per status token.
    pub by_status: BTreeMap<String, i64>,
}

/// A partial update to an agent's quality signal. At least one field must be set.
#[derive(Debug, Clone, Default)]
pub struct QualityPatch {
    /// New quality score (`None` leaves it unchanged).
    pub quality_score: Option<f64>,
    /// Replacement drift-flag set (`None` leaves it unchanged).
    pub drift_flags: Option<Vec<String>>,
}
