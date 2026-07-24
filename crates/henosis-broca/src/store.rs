//! The SQLite-backed Broca narration log.
//!
//! Actions are scoped by [`TenantId`] and [`PrincipalId`], published as typed
//! `narration.logged` events on the in-process [`AxonBus`], and stored in a versioned SQLite
//! schema. One `Connection` is serialized by a `Mutex`.
//!
//! Narration is layered: a caller-supplied sentence wins; otherwise the template renderer
//! runs at log time; otherwise the optional pluggable [`Narrator`] (an LLM seam filled at
//! server wiring time) can be consulted lazily via [`BrocaStore::get_or_narrate`].

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TenantId, Timestamp, TypedEvent};

use crate::error::BrocaError;
use crate::events::ActionLogged;
use crate::model::{ActionEntry, ActionFilter, BrocaStats, LogAction};
use crate::narrate::{narrate_from_template, Narrator};

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (see the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/V1__broca_actions.sql"))];

/// The columns of `broca_actions`, in the order [`read_raw`] reads them.
const ACTION_COLUMNS: &str =
    "id, tenant, principal_id, service, action, payload, narrative, created_at";

/// The narration log.
///
/// Share it as `Arc<BrocaStore>`; all methods take `&self`.
pub struct BrocaStore {
    /// The one connection, serialized by a `Mutex` (rusqlite `Connection` is `Send`, not `Sync`).
    conn: Mutex<Connection>,
    /// The bus narration events are published onto.
    bus: Arc<AxonBus>,
    /// The optional LLM narrator seam. `None` leaves unmatched actions without narration.
    narrator: Option<Box<dyn Narrator>>,
}

/// Map a generic rusqlite error to an opaque backend error.
fn berr(e: rusqlite::Error) -> BrocaError {
    BrocaError::Backend(e.to_string())
}

/// Serialize a [`Timestamp`] to its stored RFC3339-UTC string (via the contracts wire form).
fn ts_to_db(ts: &Timestamp) -> Result<String, BrocaError> {
    serde_json::to_value(ts)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| BrocaError::Backend("timestamp serialize".to_string()))
}

/// Parse a stored RFC3339 string back into a UTC-normalized [`Timestamp`].
fn ts_from_db(s: &str) -> Result<Timestamp, BrocaError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| BrocaError::Backend(format!("timestamp parse {s:?}: {e}")))
}

/// A [`Timestamp`] as integer Unix nanoseconds, the `created_at_ns` storage form.
fn ts_nanos(ts: &Timestamp) -> i64 {
    ts.as_offset_date_time().unix_timestamp_nanos() as i64
}

/// The raw column values of one `broca_actions` row, before parsing into typed fields.
struct RawAction {
    /// Log id.
    id: i64,
    /// TenantId string.
    tenant: String,
    /// PrincipalId string.
    principal_id: String,
    /// Service name.
    service: String,
    /// Action token.
    action: String,
    /// Payload JSON text.
    payload: String,
    /// Narrative, if any.
    narrative: Option<String>,
    /// Insertion time (RFC3339).
    created_at: String,
}

/// Read a `broca_actions` row positionally into a [`RawAction`] (column order = [`ACTION_COLUMNS`]).
fn read_raw(row: &rusqlite::Row) -> rusqlite::Result<RawAction> {
    Ok(RawAction {
        id: row.get(0)?,
        tenant: row.get(1)?,
        principal_id: row.get(2)?,
        service: row.get(3)?,
        action: row.get(4)?,
        payload: row.get(5)?,
        narrative: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// Converts raw storage rows into public action entries.
impl RawAction {
    /// Parse raw columns into a typed [`ActionEntry`], surfacing any corrupt value as a
    /// backend error.
    fn into_entry(self) -> Result<ActionEntry, BrocaError> {
        Ok(ActionEntry {
            id: self.id,
            tenant: self.tenant.parse::<TenantId>().map_err(|e| {
                BrocaError::Backend(format!("corrupt tenant {:?}: {e}", self.tenant))
            })?,
            principal_id: self.principal_id.parse::<PrincipalId>().map_err(|e| {
                BrocaError::Backend(format!("corrupt principal_id {:?}: {e}", self.principal_id))
            })?,
            service: self.service,
            action: self.action,
            payload: serde_json::from_str(&self.payload).map_err(|e| {
                BrocaError::Backend(format!("corrupt payload {:?}: {e}", self.payload))
            })?,
            narrative: self.narrative,
            created_at: ts_from_db(&self.created_at)?,
        })
    }
}

/// Implements the Broca store operations.
impl BrocaStore {
    /// Open (creating the file if absent) a store at `path`, applying any pending migrations.
    /// No narrator is attached; see [`Self::with_narrator`].
    pub fn open(path: impl AsRef<Path>, bus: Arc<AxonBus>) -> Result<Self, BrocaError> {
        let conn = Connection::open(path).map_err(berr)?;
        Self::from_conn(conn, bus)
    }

    /// Open an ephemeral in-memory store. For tests and throwaway use.
    pub fn open_in_memory(bus: Arc<AxonBus>) -> Result<Self, BrocaError> {
        let conn = Connection::open_in_memory().map_err(berr)?;
        Self::from_conn(conn, bus)
    }

    /// Attach a pluggable [`Narrator`] consulted by [`Self::get_or_narrate`] when neither a
    /// caller-supplied sentence nor a template produced one. Builder-style, used at server
    /// wiring time.
    pub fn with_narrator(mut self, narrator: Box<dyn Narrator>) -> Self {
        self.narrator = Some(narrator);
        self
    }

    /// Enable foreign keys, apply migrations, and wrap the connection.
    fn from_conn(mut conn: Connection, bus: Arc<AxonBus>) -> Result<Self, BrocaError> {
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(berr)?;
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            bus,
            narrator: None,
        })
    }

    /// Lock the connection, recovering from a poisoned mutex.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Publish a narration event, fire-and-forget. A publish failure is logged, never fatal --
    /// telemetry must not change a log operation's outcome.
    fn emit<E: TypedEvent>(&self, event: &E, tenant: TenantId, principal: PrincipalId) {
        if let Err(e) = self.bus.publish_event(event, tenant, principal) {
            tracing::warn!(error = %e, kind = E::KIND, "failed to publish broca narration event");
        }
    }

    /// Record an action and emit `narration.logged`.
    ///
    /// A caller-supplied narrative wins; otherwise the template renderer is consulted, and an
    /// action with no template logs with no narrative (the lazy [`Self::get_or_narrate`] path
    /// can fill it in later). The payload must be a JSON object when supplied.
    pub async fn log(&self, req: LogAction) -> Result<ActionEntry, BrocaError> {
        let payload = req.payload.unwrap_or_else(|| serde_json::json!({}));
        if !payload.is_object() {
            return Err(BrocaError::InvalidInput(
                "payload must be a JSON object".to_string(),
            ));
        }
        let service = req.service.unwrap_or_else(|| "henosis".to_string());
        let narrative = req
            .narrative
            .or_else(|| narrate_from_template(&req.action, &payload));
        let now = Timestamp::now();
        let entry = {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO broca_actions \
                 (tenant, principal_id, service, action, payload, narrative, created_at, created_at_ns) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    req.tenant.to_string(),
                    req.principal_id.to_string(),
                    &service,
                    &req.action,
                    payload.to_string(),
                    &narrative,
                    ts_to_db(&now)?,
                    ts_nanos(&now),
                ],
            )
            .map_err(berr)?;
            ActionEntry {
                id: conn.last_insert_rowid(),
                tenant: req.tenant,
                principal_id: req.principal_id,
                service,
                action: req.action,
                payload,
                narrative,
                created_at: now,
            }
        };
        self.emit(
            &ActionLogged {
                action_id: entry.id,
                principal_id: entry.principal_id.to_string(),
                service: entry.service.clone(),
                action: entry.action.clone(),
                narrative: entry.narrative.clone(),
            },
            entry.tenant,
            entry.principal_id,
        );
        Ok(entry)
    }

    /// Look up one action by id, scoped to its tenant. `Ok(None)` if absent or in another
    /// tenant (indistinguishable by design).
    pub async fn get(&self, tenant: TenantId, id: i64) -> Result<Option<ActionEntry>, BrocaError> {
        let conn = self.lock();
        Self::get_in(&conn, tenant, id)
    }

    /// Tenant-scoped lookup against an arbitrary connection (shared with the narrate path).
    fn get_in(
        conn: &Connection,
        tenant: TenantId,
        id: i64,
    ) -> Result<Option<ActionEntry>, BrocaError> {
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {ACTION_COLUMNS} FROM broca_actions WHERE id = ?1 AND tenant = ?2"
                ),
                rusqlite::params![id, tenant.to_string()],
                read_raw,
            )
            .optional()
            .map_err(berr)?;
        raw.map(RawAction::into_entry).transpose()
    }

    /// Query a tenant's action feed, newest-first, AND-filtered by [`ActionFilter`].
    pub async fn query(
        &self,
        tenant: TenantId,
        filter: ActionFilter,
    ) -> Result<Vec<ActionEntry>, BrocaError> {
        let mut sql = format!("SELECT {ACTION_COLUMNS} FROM broca_actions WHERE tenant = ?1");
        let mut args: Vec<rusqlite::types::Value> = vec![tenant.to_string().into()];
        let mut n = 1;
        if let Some(principal) = &filter.principal_id {
            n += 1;
            sql.push_str(&format!(" AND principal_id = ?{n}"));
            args.push(principal.to_string().into());
        }
        if let Some(service) = &filter.service {
            n += 1;
            sql.push_str(&format!(" AND service = ?{n}"));
            args.push(service.clone().into());
        }
        if let Some(action) = &filter.action {
            n += 1;
            sql.push_str(&format!(" AND action = ?{n}"));
            args.push(action.clone().into());
        }
        if let Some(since) = &filter.since {
            n += 1;
            // Integer nanoseconds, not text comparison: see the V1 migration note.
            sql.push_str(&format!(" AND created_at_ns >= ?{n}"));
            args.push(ts_nanos(since).into());
        }
        sql.push_str(" ORDER BY id DESC");
        // limit/offset are `usize`, safe to inline. OFFSET needs a LIMIT in SQLite (-1 = unbounded).
        match (filter.limit, filter.offset) {
            (Some(l), Some(o)) => sql.push_str(&format!(" LIMIT {l} OFFSET {o}")),
            (Some(l), None) => sql.push_str(&format!(" LIMIT {l}")),
            (None, Some(o)) => sql.push_str(&format!(" LIMIT -1 OFFSET {o}")),
            (None, None) => {}
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql).map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), read_raw)
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(berr)?.into_entry()?);
        }
        Ok(out)
    }

    /// Fetch an action and ensure it carries a narrative: an existing sentence is returned
    /// as-is; otherwise the template renderer is tried, then the attached [`Narrator`] (if
    /// any). A derived sentence is persisted before returning. With no narrator attached and
    /// no template, the entry comes back unchanged with `narrative: None`.
    /// [`BrocaError::NotFound`] if the id does not exist in the tenant.
    pub async fn get_or_narrate(
        &self,
        tenant: TenantId,
        id: i64,
    ) -> Result<ActionEntry, BrocaError> {
        let entry = self
            .get(tenant, id)
            .await?
            .ok_or(BrocaError::NotFound(id))?;
        if entry.narrative.is_some() {
            return Ok(entry);
        }
        let derived = match narrate_from_template(&entry.action, &entry.payload) {
            Some(sentence) => Some(sentence),
            None => match &self.narrator {
                Some(narrator) => Some(narrator.narrate(&entry.action, &entry.payload).await?),
                None => None,
            },
        };
        let Some(sentence) = derived else {
            return Ok(entry);
        };
        {
            let conn = self.lock();
            conn.execute(
                "UPDATE broca_actions SET narrative = ?1 WHERE id = ?2 AND tenant = ?3",
                rusqlite::params![&sentence, id, tenant.to_string()],
            )
            .map_err(berr)?;
        }
        Ok(ActionEntry {
            narrative: Some(sentence),
            ..entry
        })
    }

    /// Aggregate action counts for one tenant.
    pub async fn stats(&self, tenant: TenantId) -> Result<BrocaStats, BrocaError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT service, action, principal_id, COUNT(*) FROM broca_actions \
                 WHERE tenant = ?1 GROUP BY service, action, principal_id",
            )
            .map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params![tenant.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(berr)?;
        let mut stats = BrocaStats {
            total: 0,
            by_service: std::collections::BTreeMap::new(),
            by_action: std::collections::BTreeMap::new(),
            by_principal: std::collections::BTreeMap::new(),
        };
        for row in rows {
            let (service, action, principal, count) = row.map_err(berr)?;
            stats.total += count;
            *stats.by_service.entry(service).or_insert(0) += count;
            *stats.by_action.entry(action).or_insert(0) += count;
            *stats.by_principal.entry(principal).or_insert(0) += count;
        }
        Ok(stats)
    }
}

/// Apply every migration whose version exceeds `PRAGMA user_version`, each in its own transaction,
/// bumping `user_version` as it goes. Idempotent: an up-to-date database applies nothing.
fn apply_migrations(conn: &mut Connection) -> Result<(), BrocaError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(berr)?;
    for (v, sql) in MIGRATIONS {
        if *v > version {
            let tx = conn.transaction().map_err(berr)?;
            tx.execute_batch(sql)
                .map_err(|e| BrocaError::Backend(format!("migration V{v} failed: {e}")))?;
            tx.pragma_update(None, "user_version", *v).map_err(berr)?;
            tx.commit().map_err(berr)?;
            version = *v;
        }
    }
    Ok(())
}

#[cfg(test)]
/// Tests Broca store persistence, scoping, narration, and statistics.
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// A store on a fresh in-memory db plus the bus it publishes to.
    fn store() -> (BrocaStore, Arc<AxonBus>) {
        let bus = Arc::new(AxonBus::new());
        let store = BrocaStore::open_in_memory(bus.clone()).expect("open");
        (store, bus)
    }

    /// A minimal LogAction for `principal` in `tenant`.
    fn log_action(tenant: TenantId, principal: PrincipalId, action: &str) -> LogAction {
        LogAction {
            tenant,
            principal_id: principal,
            service: None,
            action: action.to_string(),
            payload: None,
            narrative: None,
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
    /// Confirms that a logged templated action can be retrieved unchanged.
    async fn log_then_get_roundtrips_with_template_narrative() {
        let (store, bus) = store();
        let mut rx = bus.subscribe("narration");
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let entry = store
            .log(LogAction {
                payload: Some(serde_json::json!({
                    "agent": "claude", "title": "ship broca", "project": "henosis"
                })),
                ..log_action(tenant, principal, "task.created")
            })
            .await
            .expect("log");
        assert_eq!(entry.service, "henosis", "service defaults");
        assert_eq!(
            entry.narrative.as_deref(),
            Some("claude started a new task: \"ship broca\" in henosis"),
            "template narrative derived at log time"
        );
        assert_eq!(drain_kinds(&mut rx), ["narration.logged"]);
        let got = store
            .get(tenant, entry.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got, entry);
    }

    #[tokio::test]
    /// Confirms caller narration takes precedence and unmatched actions remain bare.
    async fn caller_narrative_wins_and_unknown_action_logs_without_one() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let entry = store
            .log(LogAction {
                narrative: Some("custom sentence".to_string()),
                payload: Some(serde_json::json!({"title": "x"})),
                ..log_action(tenant, principal, "task.created")
            })
            .await
            .expect("log");
        assert_eq!(entry.narrative.as_deref(), Some("custom sentence"));

        let bare = store
            .log(log_action(tenant, principal, "custom.exotic.event"))
            .await
            .expect("log");
        assert!(
            bare.narrative.is_none(),
            "no template, no narrator -> no narrative"
        );
    }

    #[tokio::test]
    /// Rejects payload values that are not JSON objects.
    async fn log_rejects_non_object_payload() {
        let (store, _bus) = store();
        let err = store
            .log(LogAction {
                payload: Some(serde_json::json!([1, 2, 3])),
                ..log_action(TenantId::new(), PrincipalId::new(), "x.y")
            })
            .await
            .expect_err("non-object payload");
        assert!(matches!(err, BrocaError::InvalidInput(_)));
    }

    #[tokio::test]
    /// Confirms action reads are scoped to the requested tenant.
    async fn get_is_tenant_scoped() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let entry = store
            .log(log_action(tenant, principal, "task.output"))
            .await
            .expect("log");
        assert!(store.get(tenant, entry.id).await.expect("get").is_some());
        assert!(store
            .get(TenantId::new(), entry.id)
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    /// Confirms query filters, pagination, ordering, and tenant scoping.
    async fn query_filters_and_paginates() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let a = PrincipalId::new();
        let b = PrincipalId::new();
        store
            .log(log_action(tenant, a, "task.output"))
            .await
            .expect("log");
        store
            .log(LogAction {
                service: Some("soma".to_string()),
                ..log_action(tenant, b, "agent.heartbeat")
            })
            .await
            .expect("log");
        store
            .log(log_action(tenant, a, "task.output"))
            .await
            .expect("log");

        // By principal.
        let mine = store
            .query(
                tenant,
                ActionFilter {
                    principal_id: Some(a),
                    ..Default::default()
                },
            )
            .await
            .expect("query");
        assert_eq!(mine.len(), 2);
        // By service.
        let soma = store
            .query(
                tenant,
                ActionFilter {
                    service: Some("soma".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("query");
        assert_eq!(soma.len(), 1);
        assert_eq!(soma[0].action, "agent.heartbeat");
        // By action type.
        let outputs = store
            .query(
                tenant,
                ActionFilter {
                    action: Some("task.output".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("query");
        assert_eq!(outputs.len(), 2);
        // Newest first + limit/offset pagination.
        let all = store
            .query(tenant, ActionFilter::default())
            .await
            .expect("query");
        assert_eq!(all.len(), 3);
        assert!(all[0].id > all[2].id, "newest first");
        let page2 = store
            .query(
                tenant,
                ActionFilter {
                    limit: Some(2),
                    offset: Some(2),
                    ..Default::default()
                },
            )
            .await
            .expect("query");
        assert_eq!(page2.len(), 1);
        // Another tenant sees nothing.
        assert!(store
            .query(TenantId::new(), ActionFilter::default())
            .await
            .expect("query")
            .is_empty());
    }

    #[tokio::test]
    /// Confirms timestamp filtering retains only entries at or after the cutoff.
    async fn since_filters_on_nanosecond_instants() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        store
            .log(log_action(tenant, principal, "task.output"))
            .await
            .expect("log");
        let cut = Timestamp::now();
        store
            .log(log_action(tenant, principal, "task.plan"))
            .await
            .expect("log");

        let recent = store
            .query(
                tenant,
                ActionFilter {
                    since: Some(cut),
                    ..Default::default()
                },
            )
            .await
            .expect("query");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].action, "task.plan");
    }

    /// A canned narrator for the seam test.
    struct CannedNarrator;

    /// Returns a fixed sentence regardless of input.
    #[async_trait]
    /// Implements canned narration for seam tests.
    impl Narrator for CannedNarrator {
        /// Produces a fixed sentence containing the action name.
        async fn narrate(
            &self,
            action: &str,
            _payload: &serde_json::Value,
        ) -> Result<String, BrocaError> {
            Ok(format!("something happened: {action}"))
        }
    }

    #[tokio::test]
    /// Confirms template and custom narrator results are layered and persisted.
    async fn get_or_narrate_layers_template_then_narrator() {
        let bus = Arc::new(AxonBus::new());
        let store = BrocaStore::open_in_memory(bus)
            .expect("open")
            .with_narrator(Box::new(CannedNarrator));
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());

        // No template for this action: the narrator fills it in, and it persists.
        let bare = store
            .log(log_action(tenant, principal, "custom.exotic.event"))
            .await
            .expect("log");
        assert!(bare.narrative.is_none());
        let narrated = store
            .get_or_narrate(tenant, bare.id)
            .await
            .expect("narrate");
        assert_eq!(
            narrated.narrative.as_deref(),
            Some("something happened: custom.exotic.event")
        );
        let persisted = store
            .get(tenant, bare.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(
            persisted.narrative, narrated.narrative,
            "derived sentence persisted"
        );

        // An existing narrative is returned untouched.
        let again = store
            .get_or_narrate(tenant, bare.id)
            .await
            .expect("narrate");
        assert_eq!(again.narrative, narrated.narrative);

        // Unknown id in the tenant is NotFound.
        assert!(matches!(
            store
                .get_or_narrate(tenant, 999)
                .await
                .expect_err("unknown"),
            BrocaError::NotFound(999)
        ));
    }

    #[tokio::test]
    /// Confirms unmatched actions remain unnarrated without a configured narrator.
    async fn get_or_narrate_without_narrator_leaves_none() {
        let (store, _bus) = store();
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let bare = store
            .log(log_action(tenant, principal, "custom.exotic.event"))
            .await
            .expect("log");
        let entry = store
            .get_or_narrate(tenant, bare.id)
            .await
            .expect("narrate");
        assert!(
            entry.narrative.is_none(),
            "unmatched action has no narration"
        );
    }

    #[tokio::test]
    /// Confirms statistics count only actions in the requested tenant.
    async fn stats_count_per_tenant() {
        let (store, _bus) = store();
        let tenant = TenantId::new();
        let a = PrincipalId::new();
        store
            .log(log_action(tenant, a, "task.output"))
            .await
            .expect("log");
        store
            .log(log_action(tenant, a, "task.output"))
            .await
            .expect("log");
        store
            .log(LogAction {
                service: Some("soma".to_string()),
                ..log_action(tenant, PrincipalId::new(), "agent.heartbeat")
            })
            .await
            .expect("log");
        store
            .log(log_action(TenantId::new(), PrincipalId::new(), "task.plan"))
            .await
            .expect("log");

        let stats = store.stats(tenant).await.expect("stats");
        assert_eq!(stats.total, 3);
        assert_eq!(stats.by_service.get("henosis"), Some(&2));
        assert_eq!(stats.by_service.get("soma"), Some(&1));
        assert_eq!(stats.by_action.get("task.output"), Some(&2));
        assert_eq!(stats.by_principal.get(&a.to_string()), Some(&2));
    }

    #[tokio::test]
    /// Confirms stored actions survive reopening a file-backed database.
    async fn actions_persist_across_reopen() {
        let tmp = std::env::temp_dir().join(format!("henosis-broca-{}.sqlite", PrincipalId::new()));
        let (tenant, principal) = (TenantId::new(), PrincipalId::new());
        let id;
        {
            let store = BrocaStore::open(&tmp, Arc::new(AxonBus::new())).expect("open");
            id = store
                .log(log_action(tenant, principal, "task.output"))
                .await
                .expect("log")
                .id;
        }
        {
            let store = BrocaStore::open(&tmp, Arc::new(AxonBus::new())).expect("reopen");
            let got = store
                .get(tenant, id)
                .await
                .expect("get")
                .expect("present after reopen");
            assert_eq!(got.action, "task.output");
        }
        let _ = std::fs::remove_file(&tmp);
    }
}
