//! The SQLite-backed Soma presence store.
//!
//! Reimplements the Kleos soma agent registry (`kleos-lib/src/services/soma.rs`) against the
//! Henosis substrate: an agent IS a canonical principal, so [`AgentPresence`] keys on the
//! agent's own [`PrincipalId`] (replacing the Kleos `i64` surrogate + `user_id` shard), and
//! registration VERIFIES the principal exists in the canonical directory -- it never mints one
//! (the enrollment authority owns principal creation).
//! Lifecycle events are typed and published to the in-process [`AxonBus`]; schema is managed by
//! the kernel-crate migration convention (`PRAGMA user_version` + `migrations/Vn__*.sql`).
//! Concurrency: one `Connection` behind a `Mutex`, the chiasm precedent.
//!
//! The store provides presence registration, heartbeats, status, reads, stale listing,
//! capability search, quality updates, and stats. It does not manage groups or agent logs;
//! Broca owns narration and [`crate::backfill`] handles legacy imports.
//!
//! One deliberate behavior fix over Kleos: a bare heartbeat revives `pending` as well as
//! `offline` to `online`. In Kleos only `offline` revived, so agents registered as `pending`
//! stayed `pending` forever while heartbeating -- the production registry shows 40+ agents
//! stuck that way. A heartbeat is liveness evidence; `error` stays sticky until an explicit
//! status override or `set_status` clears it.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TenantId, Timestamp, TypedEvent};
use syntheos_identity::PrincipalDirectory;

use crate::error::SomaError;
use crate::events::{
    AgentDeregistered, AgentHeartbeat, AgentQualityUpdated, AgentRegistered, AgentStatusChanged,
};
use crate::model::{
    AgentPresence, PresenceFilter, PresenceStatus, QualityPatch, RegisterAgent, SomaStats,
};

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/V1__soma_presence.sql")),
    (2, include_str!("../migrations/V2__soma_legacy_maps.sql")),
    (
        3,
        include_str!("../migrations/V3__soma_legacy_map_source.sql"),
    ),
];

/// The columns of `soma_presence`, in the order [`read_raw`] reads them.
const PRESENCE_COLUMNS: &str = "principal_id, tenant, name, agent_type, description, \
    capabilities, status, config, heartbeat_at, quality_score, drift_flags, created_at, updated_at";

/// The agent presence + quality store.
///
/// Share it as `Arc<SomaStore>`; all methods take `&self`.
pub struct SomaStore {
    /// The one connection, serialized by a `Mutex` (rusqlite `Connection` is `Send`, not `Sync`).
    conn: Mutex<Connection>,
    /// The bus presence-lifecycle events are published onto.
    bus: Arc<AxonBus>,
    /// The canonical directory registration verifies principals against (lookup only -- Soma
    /// never calls `enroll`; see projection convention section 6, check 5).
    directory: Arc<dyn PrincipalDirectory>,
}

/// Map a generic rusqlite error to an opaque backend error.
pub(crate) fn berr(e: rusqlite::Error) -> SomaError {
    SomaError::Backend(e.to_string())
}

/// Serialize a [`Timestamp`] to its stored RFC3339-UTC string (via the contracts wire form).
pub(crate) fn ts_to_db(ts: &Timestamp) -> Result<String, SomaError> {
    serde_json::to_value(ts)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| SomaError::Backend("timestamp serialize".to_string()))
}

/// Parse a stored RFC3339 string back into a UTC-normalized [`Timestamp`].
pub(crate) fn ts_from_db(s: &str) -> Result<Timestamp, SomaError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| SomaError::Backend(format!("timestamp parse {s:?}: {e}")))
}

/// The raw column values of one `soma_presence` row, before parsing into typed
/// [`AgentPresence`] fields.
struct RawPresence {
    /// PrincipalId string.
    principal_id: String,
    /// TenantId string.
    tenant: String,
    /// Working label.
    name: String,
    /// Coarse category.
    agent_type: String,
    /// Optional description.
    description: Option<String>,
    /// JSON array of capability strings.
    capabilities: String,
    /// PresenceStatus token.
    status: String,
    /// JSON object of configuration.
    config: String,
    /// Last heartbeat (RFC3339), if any.
    heartbeat_at: Option<String>,
    /// Latest quality score, if any.
    quality_score: Option<f64>,
    /// JSON array of drift-flag strings.
    drift_flags: String,
    /// Creation time (RFC3339).
    created_at: String,
    /// Last-modification time (RFC3339).
    updated_at: String,
}

/// Read a `soma_presence` row positionally into a [`RawPresence`] (column order =
/// [`PRESENCE_COLUMNS`]).
fn read_raw(row: &rusqlite::Row) -> rusqlite::Result<RawPresence> {
    Ok(RawPresence {
        principal_id: row.get(0)?,
        tenant: row.get(1)?,
        name: row.get(2)?,
        agent_type: row.get(3)?,
        description: row.get(4)?,
        capabilities: row.get(5)?,
        status: row.get(6)?,
        config: row.get(7)?,
        heartbeat_at: row.get(8)?,
        quality_score: row.get(9)?,
        drift_flags: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

/// Methods for `RawPresence`.
impl RawPresence {
    /// Parse raw columns into a typed [`AgentPresence`], surfacing any corrupt value as a
    /// backend error. Strict (no Kleos-style fallback-to-empty): every row is written by this
    /// store with valid JSON, so a parse failure is corruption worth surfacing, not papering
    /// over. The legacy backfill sanitizes before insert.
    fn into_presence(self) -> Result<AgentPresence, SomaError> {
        Ok(AgentPresence {
            principal_id: self.principal_id.parse::<PrincipalId>().map_err(|e| {
                SomaError::Backend(format!("corrupt principal_id {:?}: {e}", self.principal_id))
            })?,
            tenant: self.tenant.parse::<TenantId>().map_err(|e| {
                SomaError::Backend(format!("corrupt tenant {:?}: {e}", self.tenant))
            })?,
            name: self.name,
            agent_type: self.agent_type,
            description: self.description,
            capabilities: serde_json::from_str(&self.capabilities).map_err(|e| {
                SomaError::Backend(format!("corrupt capabilities {:?}: {e}", self.capabilities))
            })?,
            status: PresenceStatus::parse(&self.status)?,
            config: serde_json::from_str(&self.config).map_err(|e| {
                SomaError::Backend(format!("corrupt config {:?}: {e}", self.config))
            })?,
            heartbeat_at: self.heartbeat_at.as_deref().map(ts_from_db).transpose()?,
            quality_score: self.quality_score,
            drift_flags: serde_json::from_str(&self.drift_flags).map_err(|e| {
                SomaError::Backend(format!("corrupt drift_flags {:?}: {e}", self.drift_flags))
            })?,
            created_at: ts_from_db(&self.created_at)?,
            updated_at: ts_from_db(&self.updated_at)?,
        })
    }
}

/// Methods for `SomaStore`.
impl SomaStore {
    /// Open (creating the file if absent) a store at `path`, applying any pending migrations.
    pub fn open(
        path: impl AsRef<Path>,
        bus: Arc<AxonBus>,
        directory: Arc<dyn PrincipalDirectory>,
    ) -> Result<Self, SomaError> {
        let conn = Connection::open(path).map_err(berr)?;
        Self::from_conn(conn, bus, directory)
    }

    /// Open an ephemeral in-memory store. For tests and throwaway use.
    pub fn open_in_memory(
        bus: Arc<AxonBus>,
        directory: Arc<dyn PrincipalDirectory>,
    ) -> Result<Self, SomaError> {
        let conn = Connection::open_in_memory().map_err(berr)?;
        Self::from_conn(conn, bus, directory)
    }

    /// Enable foreign keys, apply migrations, and wrap the connection.
    fn from_conn(
        mut conn: Connection,
        bus: Arc<AxonBus>,
        directory: Arc<dyn PrincipalDirectory>,
    ) -> Result<Self, SomaError> {
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(berr)?;
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            bus,
            directory,
        })
    }

    /// Lock the connection, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Publish a presence event, fire-and-forget. A publish failure is logged, never fatal --
    /// telemetry must not change a presence operation's outcome.
    fn emit<E: TypedEvent>(&self, event: &E, tenant: TenantId, principal: PrincipalId) {
        if let Err(e) = self.bus.publish_event(event, tenant, principal) {
            tracing::warn!(error = %e, kind = E::KIND, "failed to publish soma presence event");
        }
    }

    /// Register (or re-register) an agent's presence and emit `agent.registered`.
    ///
    /// The principal must already be enrolled in the canonical directory
    /// ([`SomaError::UnknownPrincipal`] otherwise) -- Soma never mints principals. A fresh
    /// registration starts [`PresenceStatus::Pending`] with no heartbeat; re-registering the
    /// same principal updates its label/type/description/capabilities/config in place while
    /// preserving status, heartbeat, and quality (the Kleos upsert-by-name semantics, keyed by
    /// principal). The tenant is fixed at first registration (move = deregister + register).
    /// A different agent already holding `(tenant, name)` is [`SomaError::NameTaken`].
    pub async fn register(&self, req: RegisterAgent) -> Result<AgentPresence, SomaError> {
        if req.name.trim().is_empty() {
            return Err(SomaError::InvalidInput("agent name required".to_string()));
        }
        if req.agent_type.trim().is_empty() {
            return Err(SomaError::InvalidInput("agent type required".to_string()));
        }
        if let Some(config) = &req.config {
            if !config.is_object() {
                return Err(SomaError::InvalidInput(
                    "config must be a JSON object".to_string(),
                ));
            }
        }
        // Projection rule: verify the canonical principal exists; never mint one here.
        let enrolled = self
            .directory
            .lookup(req.principal_id)
            .await
            .map_err(|e| SomaError::Directory(e.to_string()))?;
        if enrolled.is_none() {
            return Err(SomaError::UnknownPrincipal(req.principal_id));
        }

        let now = ts_to_db(&Timestamp::now())?;
        let capabilities = serde_json::to_string(&req.capabilities.clone().unwrap_or_default())
            .map_err(|e| SomaError::Backend(format!("capabilities serialize: {e}")))?;
        let config = req
            .config
            .clone()
            .unwrap_or_else(|| serde_json::json!({}))
            .to_string();
        let presence = {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO soma_presence (principal_id, tenant, name, agent_type, description, \
                 capabilities, status, config, drift_flags, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, '[]', ?8, ?8) \
                 ON CONFLICT (principal_id) DO UPDATE SET \
                     name = excluded.name, agent_type = excluded.agent_type, \
                     description = excluded.description, capabilities = excluded.capabilities, \
                     config = excluded.config, updated_at = excluded.updated_at \
                 WHERE soma_presence.tenant = excluded.tenant",
                rusqlite::params![
                    req.principal_id.to_string(),
                    req.tenant.to_string(),
                    &req.name,
                    &req.agent_type,
                    &req.description,
                    capabilities,
                    config,
                    now,
                ],
            )
            .map_err(|e| match &e {
                // A (tenant, name) UNIQUE violation means another agent holds the label.
                rusqlite::Error::SqliteFailure(f, Some(msg))
                    if f.code == rusqlite::ErrorCode::ConstraintViolation
                        && msg.contains("soma_presence.name") =>
                {
                    SomaError::NameTaken(req.name.clone())
                }
                _ => berr(e),
            })?;
            Self::get_in(&conn, req.tenant, req.principal_id)?
                .ok_or(SomaError::NotFound(req.principal_id))?
        };
        self.emit(
            &AgentRegistered {
                principal_id: presence.principal_id.to_string(),
                name: presence.name.clone(),
                agent_type: presence.agent_type.clone(),
            },
            presence.tenant,
            presence.principal_id,
        );
        Ok(presence)
    }

    /// Look up an agent's presence within `tenant`. `Ok(None)` if it is not registered there.
    pub async fn get(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<Option<AgentPresence>, SomaError> {
        let conn = self.lock();
        Self::get_in(&conn, tenant, principal)
    }

    /// Presence lookup against an arbitrary connection (also used inside register/update paths).
    fn get_in(
        conn: &Connection,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<Option<AgentPresence>, SomaError> {
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {PRESENCE_COLUMNS} FROM soma_presence \
                     WHERE tenant = ?1 AND principal_id = ?2"
                ),
                rusqlite::params![tenant.to_string(), principal.to_string()],
                read_raw,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawPresence::into_presence).transpose()
    }

    /// Look up an agent by its `(tenant, name)` label -- unique by schema, so at most one row.
    /// The label is a working alias for humans and legacy callers; the principal id is the
    /// identity.
    pub async fn get_by_name(
        &self,
        tenant: TenantId,
        name: &str,
    ) -> Result<Option<AgentPresence>, SomaError> {
        let conn = self.lock();
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {PRESENCE_COLUMNS} FROM soma_presence \
                     WHERE tenant = ?1 AND name = ?2"
                ),
                rusqlite::params![tenant.to_string(), name],
                read_raw,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawPresence::into_presence).transpose()
    }

    /// List registered agents in `tenant`, newest-registered first, AND-filtered by
    /// [`PresenceFilter`].
    ///
    /// Tenant-scoped: presence rows carry a tenant, and every query restricts results to the
    /// supplied value. Network-facing callers must derive that value from authenticated request
    /// context; only the explicitly loopback-compatible routes accept a caller assertion.
    pub async fn list(
        &self,
        tenant: TenantId,
        filter: PresenceFilter,
    ) -> Result<Vec<AgentPresence>, SomaError> {
        let mut sql = format!("SELECT {PRESENCE_COLUMNS} FROM soma_presence WHERE tenant = ?1");
        let mut args: Vec<rusqlite::types::Value> = vec![tenant.to_string().into()];
        let mut n = 1;
        if let Some(agent_type) = &filter.agent_type {
            n += 1;
            sql.push_str(&format!(" AND agent_type = ?{n}"));
            args.push(agent_type.clone().into());
        }
        if let Some(status) = &filter.status {
            n += 1;
            sql.push_str(&format!(" AND status = ?{n}"));
            args.push(status.as_str().to_string().into());
        }
        sql.push_str(" ORDER BY created_at DESC");
        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql).map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), read_raw)
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(berr)?.into_presence()?);
        }
        Ok(out)
    }

    /// Record a liveness heartbeat, refreshing `heartbeat_at` and emitting `agent.heartbeat`.
    ///
    /// With no override, `pending` and `offline` agents revive to `online` (a heartbeat IS
    /// liveness evidence; in Kleos `pending` never revived and agents stuck there forever) and
    /// `error` stays sticky. An explicit `status_override` sets that status -- the typed
    /// [`PresenceStatus`] makes an invalid token unrepresentable. Returns the status after the
    /// heartbeat. [`SomaError::NotFound`] if the principal was never registered.
    pub async fn heartbeat(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        status_override: Option<PresenceStatus>,
    ) -> Result<PresenceStatus, SomaError> {
        let now = ts_to_db(&Timestamp::now())?;
        let result = {
            let conn = self.lock();
            let token: Option<String> = match status_override {
                Some(status) => conn.query_row(
                    "UPDATE soma_presence SET heartbeat_at = ?1, updated_at = ?1, status = ?4 \
                     WHERE tenant = ?2 AND principal_id = ?3 RETURNING status",
                    rusqlite::params![
                        now,
                        tenant.to_string(),
                        principal.to_string(),
                        status.as_str()
                    ],
                    |r| r.get(0),
                ),
                None => conn.query_row(
                    "UPDATE soma_presence SET heartbeat_at = ?1, updated_at = ?1, \
                     status = CASE WHEN status IN ('pending', 'offline') THEN 'online' \
                                   ELSE status END \
                     WHERE tenant = ?2 AND principal_id = ?3 RETURNING status",
                    rusqlite::params![now, tenant.to_string(), principal.to_string()],
                    |r| r.get(0),
                ),
            }
            .optional()
            .map_err(berr)?;
            token
        };
        let Some(status) = result else {
            return Err(SomaError::NotFound(principal));
        };
        let status = PresenceStatus::parse(&status)?;
        self.emit(
            &AgentHeartbeat {
                principal_id: principal.to_string(),
                status: status.as_str().to_string(),
            },
            tenant,
            principal,
        );
        Ok(status)
    }

    /// Explicitly set an agent's status (no heartbeat refresh) and emit `agent.status_changed`.
    /// [`SomaError::NotFound`] if the principal was never registered.
    pub async fn set_status(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        status: PresenceStatus,
    ) -> Result<(), SomaError> {
        let now = ts_to_db(&Timestamp::now())?;
        let updated = {
            let conn = self.lock();
            conn.execute(
                "UPDATE soma_presence SET status = ?1, updated_at = ?2 \
                 WHERE tenant = ?3 AND principal_id = ?4",
                rusqlite::params![
                    status.as_str(),
                    now,
                    tenant.to_string(),
                    principal.to_string()
                ],
            )
            .map_err(berr)?
        };
        if updated == 0 {
            return Err(SomaError::NotFound(principal));
        }
        self.emit(
            &AgentStatusChanged {
                principal_id: principal.to_string(),
                status: status.as_str().to_string(),
            },
            tenant,
            principal,
        );
        Ok(())
    }

    /// Remove an agent's presence registration. Returns whether a row was removed; emits
    /// `agent.deregistered` on a real removal. The canonical principal is NOT touched --
    /// deleting a projection never cascades into the directory (projection convention
    /// section 4).
    pub async fn delete(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<bool, SomaError> {
        // Fetch first so the event can carry the row's tenant.
        let Some(presence) = self.get(tenant, principal).await? else {
            return Ok(false);
        };
        let removed = {
            let conn = self.lock();
            conn.execute(
                "DELETE FROM soma_presence WHERE tenant = ?1 AND principal_id = ?2",
                rusqlite::params![tenant.to_string(), principal.to_string()],
            )
            .map_err(berr)?
        };
        if removed > 0 {
            self.emit(
                &AgentDeregistered {
                    principal_id: principal.to_string(),
                },
                presence.tenant,
                presence.principal_id,
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List `online` agents whose last heartbeat is older than `threshold_secs`. Read-only
    /// (the Kleos `get_stale_agents` semantics): callers decide whether to `set_status` them
    /// offline. Agents that never heartbeated are excluded -- they are `pending`, not stale.
    ///
    /// The overdue comparison is computed in Rust rather than SQL so it does not depend on
    /// SQLite parsing nanosecond-precision RFC3339 timestamps (the chiasm precedent).
    ///
    /// Intentionally cross-tenant and NOT HTTP-exposed: this is an internal liveness reaper
    /// that must sweep every tenant's stale agents. Unlike [`Self::list`] (a caller-facing
    /// discovery API), it is driven by a trusted background task, so it carries no tenant
    /// predicate by design.
    pub async fn list_stale(&self, threshold_secs: i64) -> Result<Vec<AgentPresence>, SomaError> {
        let candidates: Vec<AgentPresence> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {PRESENCE_COLUMNS} FROM soma_presence \
                     WHERE status = 'online' AND heartbeat_at IS NOT NULL"
                ))
                .map_err(berr)?;
            let rows = stmt.query_map([], read_raw).map_err(berr)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(berr)?.into_presence()?);
            }
            out
        };
        let now = Timestamp::now().as_offset_date_time();
        Ok(candidates
            .into_iter()
            .filter(|p| {
                p.heartbeat_at.as_ref().is_some_and(|hb| {
                    (now - hb.as_offset_date_time()).as_seconds_f64() > threshold_secs as f64
                })
            })
            .collect())
    }

    /// Return every agent in `tenant` advertising exactly `capability`. A SQL `LIKE`
    /// prefilter narrows the row set; an exact-match post-filter in Rust discards substring
    /// false positives (`code` must not match `code-review`) -- the Kleos algorithm, ported.
    ///
    /// Tenant-scoped for the same reason as [`Self::list`]: capability discovery must not
    /// cross the tenant boundary.
    pub async fn find_by_capability(
        &self,
        tenant: TenantId,
        capability: &str,
    ) -> Result<Vec<AgentPresence>, SomaError> {
        let like = format!("%{capability}%");
        let candidates: Vec<AgentPresence> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {PRESENCE_COLUMNS} FROM soma_presence \
                     WHERE tenant = ?1 AND capabilities LIKE ?2"
                ))
                .map_err(berr)?;
            let rows = stmt
                .query_map(rusqlite::params![tenant.to_string(), like], read_raw)
                .map_err(berr)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(berr)?.into_presence()?);
            }
            out
        };
        Ok(candidates
            .into_iter()
            .filter(|p| p.capabilities.iter().any(|c| c == capability))
            .collect())
    }

    /// Apply a quality-signal update (Thymus evaluation / supervision) and emit
    /// `agent.quality_updated`. At least one patch field must be set
    /// ([`SomaError::InvalidInput`] otherwise); [`SomaError::NotFound`] if the principal was
    /// never registered. Returns the updated presence.
    pub async fn update_quality(
        &self,
        tenant: TenantId,
        principal: PrincipalId,
        patch: QualityPatch,
    ) -> Result<AgentPresence, SomaError> {
        if patch.quality_score.is_none() && patch.drift_flags.is_none() {
            return Err(SomaError::InvalidInput(
                "at least one of quality_score or drift_flags must be provided".to_string(),
            ));
        }
        let now = ts_to_db(&Timestamp::now())?;
        let presence = {
            let mut conn = self.lock();
            let tx = conn.transaction().map_err(berr)?;
            let mut presence =
                Self::get_in(&tx, tenant, principal)?.ok_or(SomaError::NotFound(principal))?;
            if let Some(score) = patch.quality_score {
                presence.quality_score = Some(score);
            }
            if let Some(flags) = patch.drift_flags {
                presence.drift_flags = flags;
            }
            presence.updated_at = ts_from_db(&now)?;
            tx.execute(
                "UPDATE soma_presence SET quality_score = ?1, drift_flags = ?2, updated_at = ?3 \
                 WHERE tenant = ?4 AND principal_id = ?5",
                rusqlite::params![
                    presence.quality_score,
                    serde_json::to_string(&presence.drift_flags)
                        .map_err(|e| SomaError::Backend(format!("drift_flags serialize: {e}")))?,
                    now,
                    tenant.to_string(),
                    principal.to_string(),
                ],
            )
            .map_err(berr)?;
            tx.commit().map_err(berr)?;
            presence
        };
        self.emit(
            &AgentQualityUpdated {
                principal_id: principal.to_string(),
                quality_score: presence.quality_score,
                drift_flag_count: presence.drift_flags.len() as u64,
            },
            presence.tenant,
            presence.principal_id,
        );
        Ok(presence)
    }

    /// Aggregate presence counts for one tenant.
    pub async fn stats(&self, tenant: TenantId) -> Result<SomaStats, SomaError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT agent_type, status, COUNT(*) FROM soma_presence \
                 WHERE tenant = ?1 GROUP BY agent_type, status",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![tenant.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(berr)?;
        let mut stats = SomaStats {
            total: 0,
            online: 0,
            by_type: std::collections::BTreeMap::new(),
            by_status: std::collections::BTreeMap::new(),
        };
        for row in rows {
            let (agent_type, status, count) = row.map_err(berr)?;
            stats.total += count;
            if status == "online" {
                stats.online += count;
            }
            *stats.by_type.entry(agent_type).or_insert(0) += count;
            *stats.by_status.entry(status).or_insert(0) += count;
        }
        Ok(stats)
    }
}

/// Apply every migration whose version exceeds `PRAGMA user_version`, each in its own transaction,
/// bumping `user_version` as it goes. Idempotent: an up-to-date database applies nothing.
pub(crate) fn apply_migrations(conn: &mut Connection) -> Result<(), SomaError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(berr)?;
    for (v, sql) in MIGRATIONS {
        if *v > version {
            let tx = conn.transaction().map_err(berr)?;
            tx.execute_batch(sql)
                .map_err(|e| SomaError::Backend(format!("migration V{v} failed: {e}")))?;
            tx.pragma_update(None, "user_version", *v).map_err(berr)?;
            tx.commit().map_err(berr)?;
            version = *v;
        }
    }
    Ok(())
}

#[cfg(test)]
/// Unit tests for this module.
mod tests {
    use super::*;
    use syntheos_contracts::PrincipalKind;
    use syntheos_identity::InMemoryDirectory;

    /// A store on a fresh in-memory db, its bus, and the directory it verifies against.
    fn store() -> (SomaStore, Arc<AxonBus>, Arc<InMemoryDirectory>) {
        let bus = Arc::new(AxonBus::new());
        let directory = Arc::new(InMemoryDirectory::new());
        let store = SomaStore::open_in_memory(bus.clone(), directory.clone()).expect("open");
        (store, bus, directory)
    }

    /// Enroll an Agent principal in `directory` and return a registration request for it.
    async fn enrolled_agent(
        directory: &InMemoryDirectory,
        tenant: TenantId,
        name: &str,
    ) -> RegisterAgent {
        let principal = directory
            .enroll(PrincipalKind::Agent, Some(name.to_string()))
            .await
            .expect("enroll");
        RegisterAgent {
            principal_id: principal.id,
            tenant,
            name: name.to_string(),
            agent_type: "coding".to_string(),
            description: None,
            capabilities: None,
            config: None,
        }
    }

    /// Drain the kind strings currently buffered on a raw subscriber.
    fn drain_kinds(
        rx: &mut tokio::sync::broadcast::Receiver<syntheos_contracts::AxonEnvelope>,
    ) -> Vec<String> {
        let mut kinds = Vec::new();
        while let Ok(env) = rx.try_recv() {
            kinds.push(env.kind);
        }
        kinds
    }

    #[tokio::test]
    /// Register then get roundtrips with defaults.
    async fn register_then_get_roundtrips_with_defaults() {
        let (store, _bus, directory) = store();
        let tenant = TenantId::new();
        let req = enrolled_agent(&directory, tenant, "claude-code").await;
        let made = store.register(req.clone()).await.expect("register");
        assert_eq!(made.status, PresenceStatus::Pending);
        assert!(made.capabilities.is_empty());
        assert_eq!(made.config, serde_json::json!({}));
        assert!(made.drift_flags.is_empty());
        assert!(made.heartbeat_at.is_none());
        let got = store
            .get(tenant, req.principal_id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got, made);
    }

    #[tokio::test]
    /// Register requires enrolled principal.
    async fn register_requires_enrolled_principal() {
        let (store, _bus, _directory) = store();
        let req = RegisterAgent {
            principal_id: PrincipalId::new(), // never enrolled
            tenant: TenantId::new(),
            name: "ghost".to_string(),
            agent_type: "cli".to_string(),
            description: None,
            capabilities: None,
            config: None,
        };
        let err = store.register(req).await.expect_err("must reject");
        assert!(matches!(err, SomaError::UnknownPrincipal(_)));
    }

    #[tokio::test]
    /// Register emits and upsert preserves liveness.
    async fn register_emits_and_upsert_preserves_liveness() {
        let (store, bus, directory) = store();
        let mut rx = bus.subscribe("agent");
        let tenant = TenantId::new();
        let mut req = enrolled_agent(&directory, tenant, "worker").await;
        store.register(req.clone()).await.expect("register");
        assert_eq!(drain_kinds(&mut rx), ["agent.registered"]);

        // Mark it online with a heartbeat and give it a score.
        store
            .heartbeat(tenant, req.principal_id, None)
            .await
            .expect("heartbeat");
        store
            .update_quality(
                tenant,
                req.principal_id,
                QualityPatch {
                    quality_score: Some(0.9),
                    ..Default::default()
                },
            )
            .await
            .expect("quality");
        let _ = drain_kinds(&mut rx);

        // Re-register with evolved capabilities: identity fields update, liveness survives.
        req.agent_type = "coding".to_string();
        req.capabilities = Some(vec!["rust".to_string()]);
        let re = store.register(req.clone()).await.expect("re-register");
        assert_eq!(re.capabilities, vec!["rust".to_string()]);
        assert_eq!(re.status, PresenceStatus::Online, "status preserved");
        assert!(re.heartbeat_at.is_some(), "heartbeat preserved");
        assert_eq!(re.quality_score, Some(0.9), "quality preserved");
        assert_eq!(drain_kinds(&mut rx), ["agent.registered"]);
    }

    #[tokio::test]
    /// Register rejects empty name and type.
    async fn register_rejects_empty_name_and_type() {
        let (store, _bus, directory) = store();
        let tenant = TenantId::new();
        let mut req = enrolled_agent(&directory, tenant, "x").await;
        req.name = "  ".to_string();
        assert!(matches!(
            store.register(req.clone()).await.expect_err("empty name"),
            SomaError::InvalidInput(_)
        ));
        req.name = "x".to_string();
        req.agent_type = "".to_string();
        assert!(matches!(
            store.register(req).await.expect_err("empty type"),
            SomaError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    /// Register rejects taken name in tenant.
    async fn register_rejects_taken_name_in_tenant() {
        let (store, _bus, directory) = store();
        let tenant = TenantId::new();
        store
            .register(enrolled_agent(&directory, tenant, "shared-label").await)
            .await
            .expect("first registration");
        // A DIFFERENT principal wanting the same (tenant, name) is rejected...
        let err = store
            .register(enrolled_agent(&directory, tenant, "shared-label").await)
            .await
            .expect_err("name collision");
        assert!(matches!(err, SomaError::NameTaken(n) if n == "shared-label"));
        // ...but the same name in another tenant is fine.
        store
            .register(enrolled_agent(&directory, TenantId::new(), "shared-label").await)
            .await
            .expect("other tenant");
    }

    #[tokio::test]
    /// Heartbeat revives pending and offline keeps error sticky.
    async fn heartbeat_revives_pending_and_offline_keeps_error_sticky() {
        let (store, bus, directory) = store();
        let mut rx = bus.subscribe("agent");
        let tenant = TenantId::new();
        let req = enrolled_agent(&directory, tenant, "hb").await;
        let principal = req.principal_id;
        store.register(req).await.expect("register");
        let _ = drain_kinds(&mut rx);

        // Pending -> online on first heartbeat (the Kleos wart, fixed).
        let status = store
            .heartbeat(tenant, principal, None)
            .await
            .expect("heartbeat");
        assert_eq!(status, PresenceStatus::Online);
        assert_eq!(drain_kinds(&mut rx), ["agent.heartbeat"]);

        // Offline -> online.
        store
            .set_status(tenant, principal, PresenceStatus::Offline)
            .await
            .expect("offline");
        assert_eq!(
            store
                .heartbeat(tenant, principal, None)
                .await
                .expect("heartbeat"),
            PresenceStatus::Online
        );

        // Error is sticky under a bare heartbeat...
        store
            .set_status(tenant, principal, PresenceStatus::Error)
            .await
            .expect("error");
        assert_eq!(
            store
                .heartbeat(tenant, principal, None)
                .await
                .expect("heartbeat"),
            PresenceStatus::Error
        );
        // ...until an explicit override clears it.
        assert_eq!(
            store
                .heartbeat(tenant, principal, Some(PresenceStatus::Online))
                .await
                .expect("heartbeat"),
            PresenceStatus::Online
        );

        // Unknown principal is NotFound.
        assert!(matches!(
            store
                .heartbeat(tenant, PrincipalId::new(), None)
                .await
                .expect_err("unknown"),
            SomaError::NotFound(_)
        ));
    }

    #[tokio::test]
    /// Set status updates and emits.
    async fn set_status_updates_and_emits() {
        let (store, bus, directory) = store();
        let mut rx = bus.subscribe("agent");
        let tenant = TenantId::new();
        let req = enrolled_agent(&directory, tenant, "s").await;
        let principal = req.principal_id;
        store.register(req).await.expect("register");
        let _ = drain_kinds(&mut rx);

        store
            .set_status(tenant, principal, PresenceStatus::Online)
            .await
            .expect("set");
        assert_eq!(drain_kinds(&mut rx), ["agent.status_changed"]);
        let got = store
            .get(tenant, principal)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.status, PresenceStatus::Online);

        assert!(matches!(
            store
                .set_status(tenant, PrincipalId::new(), PresenceStatus::Online)
                .await
                .expect_err("unknown"),
            SomaError::NotFound(_)
        ));
    }

    #[tokio::test]
    /// List filters by type status and limit.
    async fn list_filters_by_type_status_and_limit() {
        let (store, _bus, directory) = store();
        let tenant = TenantId::new();
        let coder = enrolled_agent(&directory, tenant, "coder").await;
        store.register(coder.clone()).await.expect("register");
        let mut cli = enrolled_agent(&directory, tenant, "cli-tool").await;
        cli.agent_type = "cli".to_string();
        store.register(cli.clone()).await.expect("register");
        store
            .heartbeat(tenant, cli.principal_id, None)
            .await
            .expect("heartbeat");

        let coding = store
            .list(
                tenant,
                PresenceFilter {
                    agent_type: Some("coding".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("list");
        assert_eq!(coding.len(), 1);
        assert_eq!(coding[0].name, "coder");

        let online = store
            .list(
                tenant,
                PresenceFilter {
                    status: Some(PresenceStatus::Online),
                    ..Default::default()
                },
            )
            .await
            .expect("list");
        assert_eq!(online.len(), 1);
        assert_eq!(online[0].name, "cli-tool");

        assert_eq!(
            store
                .list(
                    tenant,
                    PresenceFilter {
                        limit: Some(1),
                        ..Default::default()
                    }
                )
                .await
                .expect("list")
                .len(),
            1
        );
        assert_eq!(
            store
                .list(tenant, PresenceFilter::default())
                .await
                .expect("list")
                .len(),
            2
        );
    }

    #[tokio::test]
    /// List is tenant scoped.
    async fn list_is_tenant_scoped() {
        // Cross-tenant isolation: an agent registered under tenant A must not
        // appear in tenant B's listing.
        let (store, _bus, directory) = store();
        let tenant_a = TenantId::new();
        let tenant_b = TenantId::new();
        let a = enrolled_agent(&directory, tenant_a, "a-agent").await;
        store.register(a).await.expect("register a");

        let seen_a = store
            .list(tenant_a, PresenceFilter::default())
            .await
            .expect("list a");
        assert_eq!(seen_a.len(), 1, "tenant A sees its own agent");

        let seen_b = store
            .list(tenant_b, PresenceFilter::default())
            .await
            .expect("list b");
        assert!(seen_b.is_empty(), "tenant B must not see tenant A's agent");
    }

    #[tokio::test]
    /// Principal-scoped reads and mutations reject a foreign tenant without changing the row.
    async fn principal_operations_are_tenant_scoped() {
        let (store, bus, directory) = store();
        let mut rx = bus.subscribe("agent");
        let tenant_a = TenantId::new();
        let tenant_b = TenantId::new();
        let mut req = enrolled_agent(&directory, tenant_a, "tenant-a-agent").await;
        let principal = req.principal_id;
        store.register(req.clone()).await.expect("register");
        let _ = drain_kinds(&mut rx);

        assert!(store
            .get(tenant_b, principal)
            .await
            .expect("foreign get")
            .is_none());
        assert!(matches!(
            store.heartbeat(tenant_b, principal, None).await,
            Err(SomaError::NotFound(id)) if id == principal
        ));
        assert!(matches!(
            store
                .set_status(tenant_b, principal, PresenceStatus::Online)
                .await,
            Err(SomaError::NotFound(id)) if id == principal
        ));
        assert!(matches!(
            store
                .update_quality(
                    tenant_b,
                    principal,
                    QualityPatch {
                        quality_score: Some(0.9),
                        ..Default::default()
                    },
                )
                .await,
            Err(SomaError::NotFound(id)) if id == principal
        ));
        assert!(!store
            .delete(tenant_b, principal)
            .await
            .expect("foreign delete"));

        req.tenant = tenant_b;
        req.name = "foreign-rename".to_string();
        assert!(matches!(
            store.register(req).await,
            Err(SomaError::NotFound(id)) if id == principal
        ));
        assert!(drain_kinds(&mut rx).is_empty());

        let unchanged = store
            .get(tenant_a, principal)
            .await
            .expect("owner get")
            .expect("owner row");
        assert_eq!(unchanged.tenant, tenant_a);
        assert_eq!(unchanged.name, "tenant-a-agent");
        assert_eq!(unchanged.status, PresenceStatus::Pending);
        assert_eq!(unchanged.quality_score, None);
    }

    #[tokio::test]
    /// Get by name finds within tenant.
    async fn get_by_name_finds_within_tenant() {
        let (store, _bus, directory) = store();
        let tenant = TenantId::new();
        let req = enrolled_agent(&directory, tenant, "lookup-me").await;
        store.register(req.clone()).await.expect("register");
        let got = store
            .get_by_name(tenant, "lookup-me")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.principal_id, req.principal_id);
        // The label does not resolve in a different tenant.
        assert!(store
            .get_by_name(TenantId::new(), "lookup-me")
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    /// Delete removes and emits but never touches directory.
    async fn delete_removes_and_emits_but_never_touches_directory() {
        let (store, bus, directory) = store();
        let mut rx = bus.subscribe("agent");
        let tenant = TenantId::new();
        let req = enrolled_agent(&directory, tenant, "doomed").await;
        let principal = req.principal_id;
        store.register(req).await.expect("register");
        let _ = drain_kinds(&mut rx);

        assert!(store.delete(tenant, principal).await.expect("delete"));
        assert_eq!(drain_kinds(&mut rx), ["agent.deregistered"]);
        assert!(store.get(tenant, principal).await.expect("get").is_none());
        // The canonical principal survives projection deletion (convention section 4).
        assert!(directory.lookup(principal).await.expect("lookup").is_some());
        // Idempotent: a second delete is a no-op, no event.
        assert!(!store.delete(tenant, principal).await.expect("delete"));
        assert!(drain_kinds(&mut rx).is_empty());
    }

    #[tokio::test]
    /// List stale finds overdue online agents only.
    async fn list_stale_finds_overdue_online_agents_only() {
        let (store, _bus, directory) = store();
        let tenant = TenantId::new();
        let beating = enrolled_agent(&directory, tenant, "beating").await;
        store.register(beating.clone()).await.expect("register");
        store
            .heartbeat(tenant, beating.principal_id, None)
            .await
            .expect("heartbeat");
        let pending = enrolled_agent(&directory, tenant, "never-beat").await;
        store.register(pending.clone()).await.expect("register");

        // A generous threshold finds nothing.
        assert!(store.list_stale(3600).await.expect("stale").is_empty());
        // Threshold 0: any elapsed time is overdue -- but only the online, beaten agent counts.
        let stale = store.list_stale(0).await.expect("stale");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].principal_id, beating.principal_id);
    }

    #[tokio::test]
    /// Find by capability is exact.
    async fn find_by_capability_is_exact() {
        let (store, _bus, directory) = store();
        let tenant = TenantId::new();
        let mut a = enrolled_agent(&directory, tenant, "a").await;
        a.capabilities = Some(vec!["code".to_string(), "review".to_string()]);
        store.register(a.clone()).await.expect("register");
        let mut b = enrolled_agent(&directory, tenant, "b").await;
        b.capabilities = Some(vec!["code-review".to_string()]);
        store.register(b).await.expect("register");

        // `code` matches only the exact entry, not the `code-review` substring superset.
        let found = store
            .find_by_capability(tenant, "code")
            .await
            .expect("find");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].principal_id, a.principal_id);
        assert_eq!(
            store
                .find_by_capability(tenant, "missing")
                .await
                .expect("find")
                .len(),
            0
        );
    }

    #[tokio::test]
    /// Update quality patches and emits.
    async fn update_quality_patches_and_emits() {
        let (store, bus, directory) = store();
        let mut rx = bus.subscribe("agent");
        let tenant = TenantId::new();
        let req = enrolled_agent(&directory, tenant, "scored").await;
        let principal = req.principal_id;
        store.register(req).await.expect("register");
        let _ = drain_kinds(&mut rx);

        // Score only.
        let p = store
            .update_quality(
                tenant,
                principal,
                QualityPatch {
                    quality_score: Some(0.8),
                    ..Default::default()
                },
            )
            .await
            .expect("score");
        assert_eq!(p.quality_score, Some(0.8));
        assert_eq!(drain_kinds(&mut rx), ["agent.quality_updated"]);

        // Drift flags only; the score survives.
        let p = store
            .update_quality(
                tenant,
                principal,
                QualityPatch {
                    drift_flags: Some(vec!["persona-drift".to_string()]),
                    ..Default::default()
                },
            )
            .await
            .expect("drift");
        assert_eq!(p.quality_score, Some(0.8));
        assert_eq!(p.drift_flags, vec!["persona-drift".to_string()]);

        // An empty patch is rejected; an unknown principal is NotFound.
        assert!(matches!(
            store
                .update_quality(tenant, principal, QualityPatch::default())
                .await
                .expect_err("empty patch"),
            SomaError::InvalidInput(_)
        ));
        assert!(matches!(
            store
                .update_quality(
                    tenant,
                    PrincipalId::new(),
                    QualityPatch {
                        quality_score: Some(0.1),
                        ..Default::default()
                    },
                )
                .await
                .expect_err("unknown"),
            SomaError::NotFound(_)
        ));
    }

    #[tokio::test]
    /// Stats counts per tenant.
    async fn stats_counts_per_tenant() {
        let (store, _bus, directory) = store();
        let tenant = TenantId::new();
        let other_tenant = TenantId::new();
        let a = enrolled_agent(&directory, tenant, "a").await;
        store.register(a.clone()).await.expect("register");
        store
            .heartbeat(tenant, a.principal_id, None)
            .await
            .expect("heartbeat");
        let mut b = enrolled_agent(&directory, tenant, "b").await;
        b.agent_type = "cli".to_string();
        store.register(b).await.expect("register");
        store
            .register(enrolled_agent(&directory, other_tenant, "elsewhere").await)
            .await
            .expect("register");

        let stats = store.stats(tenant).await.expect("stats");
        assert_eq!(stats.total, 2);
        assert_eq!(stats.online, 1);
        assert_eq!(stats.by_type.get("coding"), Some(&1));
        assert_eq!(stats.by_type.get("cli"), Some(&1));
        assert_eq!(stats.by_status.get("online"), Some(&1));
        assert_eq!(stats.by_status.get("pending"), Some(&1));
        // The other tenant's registry is its own.
        assert_eq!(store.stats(other_tenant).await.expect("stats").total, 1);
    }

    #[tokio::test]
    /// Presence persists across reopen.
    async fn presence_persists_across_reopen() {
        let tmp = std::env::temp_dir().join(format!("henosis-soma-{}.sqlite", PrincipalId::new()));
        let directory = Arc::new(InMemoryDirectory::new());
        let tenant = TenantId::new();
        let principal;
        {
            let store =
                SomaStore::open(&tmp, Arc::new(AxonBus::new()), directory.clone()).expect("open");
            let req = enrolled_agent(&directory, tenant, "durable").await;
            principal = req.principal_id;
            store.register(req).await.expect("register");
        }
        {
            let store = SomaStore::open(&tmp, Arc::new(AxonBus::new()), directory).expect("reopen");
            let got = store
                .get(tenant, principal)
                .await
                .expect("get")
                .expect("present after reopen");
            assert_eq!(got.name, "durable");
        }
        let _ = std::fs::remove_file(&tmp);
    }
}
