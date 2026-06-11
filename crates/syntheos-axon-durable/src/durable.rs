//! The durable bus itself: write-through publish, positional replay, named cursors, prune.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;
use syntheos_axon::AxonBus;
use syntheos_contracts::{AxonEnvelope, PrincipalId, TenantId, TypedEvent};

use crate::error::DurableAxonError;

/// Ordered schema migrations, applied by `PRAGMA user_version`. Append-only (the DB convention).
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/V1__axon_events.sql"))];

/// Map a generic rusqlite error to an opaque backend error.
fn berr(e: rusqlite::Error) -> DurableAxonError {
    DurableAxonError::Backend(e.to_string())
}

/// Serialize a [`syntheos_contracts::Timestamp`] to its stored RFC3339-UTC string (via the
/// contracts wire form -- `Timestamp` has no `Display`).
fn ts_to_db(ts: &syntheos_contracts::Timestamp) -> Result<String, DurableAxonError> {
    serde_json::to_value(ts)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| DurableAxonError::Backend("timestamp serialize".to_string()))
}

/// Parse a stored RFC3339 string back into a [`syntheos_contracts::Timestamp`].
fn ts_from_db(s: &str) -> Result<syntheos_contracts::Timestamp, DurableAxonError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| DurableAxonError::Backend(format!("timestamp parse {s:?}: {e}")))
}

/// Parse a stored id string (EventId/TenantId/PrincipalId) via its `FromStr`.
fn id_from_db<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, DurableAxonError>
where
    T::Err: std::fmt::Display,
{
    s.parse::<T>()
        .map_err(|e| DurableAxonError::Backend(format!("corrupt {what} {s:?}: {e}")))
}

/// Apply every migration whose version exceeds `PRAGMA user_version`, each in its own
/// transaction, bumping `user_version` as it goes. Idempotent.
fn apply_migrations(conn: &mut Connection) -> Result<(), DurableAxonError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(berr)?;
    for (v, sql) in MIGRATIONS {
        if *v > version {
            let tx = conn.transaction().map_err(berr)?;
            tx.execute_batch(sql)
                .map_err(|e| DurableAxonError::Backend(format!("migration V{v} failed: {e}")))?;
            tx.pragma_update(None, "user_version", *v).map_err(berr)?;
            tx.commit().map_err(berr)?;
            version = *v;
        }
    }
    Ok(())
}

/// One raw `axon_events` row: (seq, event_id, channel, kind, tenant, principal, occurred_at,
/// payload), positionally.
type RawEventRow = (i64, String, String, String, String, String, String, String);

/// Read one `axon_events` row positionally into a [`RawEventRow`].
fn read_stored(row: &rusqlite::Row) -> rusqlite::Result<RawEventRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

/// Parse the raw column tuple into a [`StoredEnvelope`].
fn parse_stored(raw: RawEventRow) -> Result<StoredEnvelope, DurableAxonError> {
    let (seq, event_id, channel, kind, tenant, principal, occurred_at, payload) = raw;
    Ok(StoredEnvelope {
        seq,
        envelope: AxonEnvelope {
            id: id_from_db(&event_id, "event id")?,
            channel,
            kind,
            tenant: id_from_db(&tenant, "tenant")?,
            principal: id_from_db(&principal, "principal")?,
            occurred_at: ts_from_db(&occurred_at)?,
            payload: serde_json::from_str(&payload)
                .map_err(|e| DurableAxonError::Backend(format!("payload parse: {e}")))?,
        },
    })
}

/// One persisted event: its position in the durable log plus the envelope itself.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEnvelope {
    /// The envelope's position in the durable log (monotonic, never reused).
    pub seq: i64,
    /// The envelope as it was published.
    pub envelope: AxonEnvelope,
}

/// The durable write-through bus. Share it as `Arc<DurableAxonBus>`; all methods take `&self`.
///
/// Wraps the in-process [`AxonBus`]: every publish appends to SQLite first, then fans out live.
/// Subscribing for live delivery goes through [`DurableAxonBus::bus`] -- the sidecar adds no
/// subscription machinery of its own.
pub struct DurableAxonBus {
    /// The one connection, serialized by a `Mutex` (rusqlite `Connection` is `Send`, not `Sync`).
    conn: Mutex<Connection>,
    /// The wrapped in-process bus the write-through fans out on.
    bus: Arc<AxonBus>,
}

impl DurableAxonBus {
    /// Open (or create) the durable log at `path`, applying migrations, wrapping `bus`.
    pub fn open(path: impl AsRef<Path>, bus: Arc<AxonBus>) -> Result<Self, DurableAxonError> {
        let mut conn = Connection::open(path).map_err(berr)?;
        // The log is the audit record: pay the fsync cost of durability on every commit.
        conn.pragma_update(None, "journal_mode", "wal")
            .map_err(berr)?;
        conn.pragma_update(None, "foreign_keys", "on")
            .map_err(berr)?;
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            bus,
        })
    }

    /// Open an in-memory durable log (tests; durability obviously ends with the process).
    pub fn open_in_memory(bus: Arc<AxonBus>) -> Result<Self, DurableAxonError> {
        let mut conn = Connection::open_in_memory().map_err(berr)?;
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            bus,
        })
    }

    /// The wrapped in-process bus, for live (lossy) subscription alongside the durable log.
    pub fn bus(&self) -> &Arc<AxonBus> {
        &self.bus
    }

    /// Persist `env` to the durable log, then fan it out on the in-process bus. Returns the
    /// assigned `seq`. If the append fails the event is NOT fanned out: the durable row is the
    /// record, and an audit consumer must never see an event that has no row.
    pub fn publish(&self, env: &AxonEnvelope) -> Result<i64, DurableAxonError> {
        let seq = {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO axon_events \
                 (event_id, channel, kind, tenant, principal, occurred_at, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    env.id.to_string(),
                    &env.channel,
                    &env.kind,
                    env.tenant.to_string(),
                    env.principal.to_string(),
                    ts_to_db(&env.occurred_at)?,
                    serde_json::to_string(&env.payload).map_err(|e| DurableAxonError::Backend(
                        format!("payload serialize: {e}")
                    ))?,
                ],
            )
            .map_err(berr)?;
            conn.last_insert_rowid()
        };
        // Persisted -- now fan out live. Reach 0 is fine; the row is the record.
        self.bus.publish(env);
        Ok(seq)
    }

    /// Build an envelope from a typed event and [`DurableAxonBus::publish`] it.
    pub fn publish_event<E: TypedEvent>(
        &self,
        event: &E,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<i64, DurableAxonError> {
        let env = event
            .to_envelope(tenant, principal)
            .map_err(|e| DurableAxonError::Backend(format!("envelope build: {e}")))?;
        self.publish(&env)
    }

    /// Read-only positional replay: up to `limit` events for `tenant` with `seq > after_seq`,
    /// in `seq` order, optionally filtered to one channel. Touches no cursor.
    pub fn replay(
        &self,
        tenant: TenantId,
        channel: Option<&str>,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredEnvelope>, DurableAxonError> {
        let conn = self.lock();
        Self::read_after(&conn, tenant, channel, after_seq, limit)
    }

    /// The shared range read both replay and consume use: events for `tenant` with
    /// `seq > after_seq`, optionally one channel, in `seq` order, capped at `limit`.
    fn read_after(
        conn: &Connection,
        tenant: TenantId,
        channel: Option<&str>,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredEnvelope>, DurableAxonError> {
        let mut sql = "SELECT seq, event_id, channel, kind, tenant, principal, occurred_at, \
                       payload FROM axon_events WHERE tenant = ?1 AND seq > ?2"
            .to_string();
        let mut args: Vec<rusqlite::types::Value> =
            vec![tenant.to_string().into(), after_seq.into()];
        if let Some(channel) = channel {
            sql.push_str(" AND channel = ?3");
            args.push(channel.to_string().into());
        }
        sql.push_str(&format!(" ORDER BY seq LIMIT {limit}"));
        let mut stmt = conn.prepare(&sql).map_err(berr)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), read_stored)
            .map_err(berr)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_stored(row.map_err(berr)?)?);
        }
        Ok(out)
    }

    /// Deliver up to `limit` not-yet-consumed events to the named consumer for
    /// (`tenant`, `channel`), advancing its cursor to the last delivered `seq` atomically with
    /// the read. An empty batch leaves the cursor untouched.
    pub fn consume(
        &self,
        consumer: &str,
        tenant: TenantId,
        channel: &str,
        limit: usize,
    ) -> Result<Vec<StoredEnvelope>, DurableAxonError> {
        if consumer.trim().is_empty() {
            return Err(DurableAxonError::InvalidInput(
                "consumer name must be non-empty (anonymous cursors would collide)".to_string(),
            ));
        }
        // One transaction: the batch read and the cursor advance commit together, so a crash
        // between them cannot deliver a batch twice nor skip one.
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(berr)?;
        let last_seq: i64 = tx
            .query_row(
                "SELECT last_seq FROM axon_cursors \
                 WHERE consumer = ?1 AND tenant = ?2 AND channel = ?3",
                rusqlite::params![consumer, tenant.to_string(), channel],
                |r| r.get(0),
            )
            .map_or_else(
                |e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(0),
                    other => Err(berr(other)),
                },
                Ok,
            )?;
        let batch = Self::read_after(&tx, tenant, Some(channel), last_seq, limit)?;
        if let Some(last) = batch.last() {
            tx.execute(
                "INSERT INTO axon_cursors (consumer, tenant, channel, last_seq, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(consumer, tenant, channel) DO UPDATE \
                 SET last_seq = excluded.last_seq, updated_at = excluded.updated_at",
                rusqlite::params![
                    consumer,
                    tenant.to_string(),
                    channel,
                    last.seq,
                    ts_to_db(&syntheos_contracts::Timestamp::now())?,
                ],
            )
            .map_err(berr)?;
        }
        tx.commit().map_err(berr)?;
        Ok(batch)
    }

    /// The named consumer's current cursor position for (`tenant`, `channel`); 0 when the
    /// consumer has never consumed there.
    pub fn cursor(
        &self,
        consumer: &str,
        tenant: TenantId,
        channel: &str,
    ) -> Result<i64, DurableAxonError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT last_seq FROM axon_cursors \
             WHERE consumer = ?1 AND tenant = ?2 AND channel = ?3",
            rusqlite::params![consumer, tenant.to_string(), channel],
            |r| r.get(0),
        )
        .map_or_else(
            |e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(0),
                other => Err(berr(other)),
            },
            Ok,
        )
    }

    /// Retention: delete every event with `seq < before_seq`, returning how many rows went.
    /// `seq` values are never reused (AUTOINCREMENT), so surviving cursors and replay positions
    /// stay valid; pruning past an unconsumed cursor is the operator's retention call, not an
    /// error.
    pub fn prune_before(&self, before_seq: i64) -> Result<usize, DurableAxonError> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM axon_events WHERE seq < ?1",
            rusqlite::params![before_seq],
        )
        .map_err(berr)
    }

    /// Lock the connection, recovering from a poisoned mutex (a panicked writer cannot corrupt
    /// SQLite state; the transaction either committed or rolled back).
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Tests for write-through persistence, replay, cursors, and retention.
#[cfg(test)]
mod tests {
    use syntheos_contracts::{EventId, Timestamp};

    use super::*;

    /// Build an envelope on `channel` for `tenant` with a payload tagged by `n`.
    fn env_for(tenant: TenantId, channel: &str, n: i64) -> AxonEnvelope {
        AxonEnvelope {
            id: EventId::new(),
            channel: channel.to_string(),
            kind: format!("{channel}.tick"),
            tenant,
            principal: PrincipalId::new(),
            occurred_at: Timestamp::now(),
            payload: serde_json::json!({ "n": n }),
        }
    }

    /// A durable bus over an in-memory log and a fresh in-process bus.
    fn durable() -> DurableAxonBus {
        DurableAxonBus::open_in_memory(Arc::new(AxonBus::new())).expect("open")
    }

    /// Write-through: a publish reaches a live subscriber AND lands in the durable log.
    #[tokio::test]
    async fn publish_persists_and_fans_out() {
        let d = durable();
        let tenant = TenantId::new();
        let mut rx = d.bus().subscribe("audit");
        let env = env_for(tenant, "audit", 1);
        let seq = d.publish(&env).expect("publish");
        assert!(seq > 0);
        assert_eq!(rx.recv().await.expect("live delivery"), env);
        let stored = d.replay(tenant, None, 0, 10).expect("replay");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].seq, seq);
        assert_eq!(stored[0].envelope, env);
    }

    /// The persisted envelope round-trips exactly (id, timestamps, payload included).
    #[tokio::test]
    async fn stored_envelope_roundtrips_exactly() {
        let d = durable();
        let tenant = TenantId::new();
        let env = env_for(tenant, "audit", 7);
        d.publish(&env).expect("publish");
        let stored = d.replay(tenant, None, 0, 10).expect("replay");
        assert_eq!(
            stored[0].envelope, env,
            "every field must survive the round trip"
        );
    }

    /// Envelopes survive a restart: reopen the same file and replay returns them.
    #[tokio::test]
    async fn envelopes_survive_restart() {
        let tmp = std::env::temp_dir().join(format!("axon-durable-{}.sqlite", EventId::new()));
        let tenant = TenantId::new();
        let env = env_for(tenant, "audit", 42);
        {
            let d = DurableAxonBus::open(&tmp, Arc::new(AxonBus::new())).expect("open");
            d.publish(&env).expect("publish");
        }
        {
            let d = DurableAxonBus::open(&tmp, Arc::new(AxonBus::new())).expect("reopen");
            let stored = d.replay(tenant, None, 0, 10).expect("replay");
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].envelope, env);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    /// Replay honors after_seq, limit, order, and the optional channel filter.
    #[tokio::test]
    async fn replay_returns_correct_subset() {
        let d = durable();
        let tenant = TenantId::new();
        let mut seqs = Vec::new();
        for n in 0..5 {
            seqs.push(d.publish(&env_for(tenant, "audit", n)).expect("publish"));
        }
        d.publish(&env_for(tenant, "other", 99))
            .expect("publish other channel");

        // after the 2nd event, limit 2 -> exactly the 3rd and 4th, in order.
        let got = d.replay(tenant, Some("audit"), seqs[1], 2).expect("replay");
        assert_eq!(
            got.iter().map(|s| s.seq).collect::<Vec<_>>(),
            vec![seqs[2], seqs[3]]
        );
        assert_eq!(got[0].envelope.payload["n"], serde_json::json!(2));

        // No channel filter -> the `other` channel event appears too.
        let all = d.replay(tenant, None, 0, 100).expect("replay all");
        assert_eq!(all.len(), 6);
    }

    /// Replay is tenant-scoped: tenant B never sees tenant A's events.
    #[tokio::test]
    async fn replay_is_tenant_scoped() {
        let d = durable();
        let (a, b) = (TenantId::new(), TenantId::new());
        d.publish(&env_for(a, "audit", 1)).expect("publish");
        assert_eq!(d.replay(a, None, 0, 10).expect("a").len(), 1);
        assert!(d.replay(b, None, 0, 10).expect("b").is_empty());
    }

    /// Consume delivers from the cursor forward and advances it; a second consume returns only
    /// what arrived since.
    #[tokio::test]
    async fn consume_advances_cursor() {
        let d = durable();
        let tenant = TenantId::new();
        for n in 0..3 {
            d.publish(&env_for(tenant, "audit", n)).expect("publish");
        }
        let first = d.consume("rift", tenant, "audit", 10).expect("consume");
        assert_eq!(first.len(), 3);
        assert_eq!(
            d.cursor("rift", tenant, "audit").expect("cursor"),
            first[2].seq
        );

        // Nothing new yet: an empty batch, cursor untouched.
        assert!(d
            .consume("rift", tenant, "audit", 10)
            .expect("consume")
            .is_empty());

        d.publish(&env_for(tenant, "audit", 3)).expect("publish");
        let second = d.consume("rift", tenant, "audit", 10).expect("consume");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].envelope.payload["n"], serde_json::json!(3));
    }

    /// Consume honors its limit and picks up where it left off mid-stream.
    #[tokio::test]
    async fn consume_respects_limit() {
        let d = durable();
        let tenant = TenantId::new();
        for n in 0..5 {
            d.publish(&env_for(tenant, "audit", n)).expect("publish");
        }
        let first = d.consume("rift", tenant, "audit", 2).expect("consume");
        let second = d.consume("rift", tenant, "audit", 2).expect("consume");
        let third = d.consume("rift", tenant, "audit", 2).expect("consume");
        assert_eq!(
            (first.len(), second.len(), third.len()),
            (2, 2, 1),
            "5 events over limit-2 batches"
        );
        assert_eq!(second[0].envelope.payload["n"], serde_json::json!(2));
        assert_eq!(third[0].envelope.payload["n"], serde_json::json!(4));
    }

    /// Cursors are independent per consumer name, per channel, and per tenant.
    #[tokio::test]
    async fn cursors_are_isolated() {
        let d = durable();
        let (a, b) = (TenantId::new(), TenantId::new());
        d.publish(&env_for(a, "audit", 1)).expect("publish");
        d.publish(&env_for(a, "task", 2)).expect("publish");
        d.publish(&env_for(b, "audit", 3)).expect("publish");

        // Consumer "rift" drains tenant A's audit channel; everything else is untouched.
        assert_eq!(d.consume("rift", a, "audit", 10).expect("c").len(), 1);
        assert_eq!(d.consume("rift", a, "task", 10).expect("c").len(), 1);
        assert_eq!(d.consume("rift", b, "audit", 10).expect("c").len(), 1);
        assert_eq!(d.consume("loom", a, "audit", 10).expect("c").len(), 1);
    }

    /// An empty consumer name is rejected: anonymous cursors would collide into one.
    #[tokio::test]
    async fn empty_consumer_name_rejected() {
        let d = durable();
        let err = d
            .consume("", TenantId::new(), "audit", 10)
            .expect_err("empty consumer must be rejected");
        assert!(
            matches!(err, DurableAxonError::InvalidInput(_)),
            "got {err:?}"
        );
    }

    /// The typed publish helper persists and assigns a seq like the raw path.
    #[tokio::test]
    async fn typed_publish_persists() {
        use serde::{Deserialize, Serialize};

        /// Test event on the `audit` channel.
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct AuditMark {
            /// What was audited.
            what: String,
        }
        impl TypedEvent for AuditMark {
            const CHANNEL: &'static str = "audit";
            const KIND: &'static str = "audit.mark";
        }

        let d = durable();
        let tenant = TenantId::new();
        let seq = d
            .publish_event(
                &AuditMark {
                    what: "gate".to_string(),
                },
                tenant,
                PrincipalId::new(),
            )
            .expect("typed publish");
        let stored = d.replay(tenant, Some("audit"), 0, 10).expect("replay");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].seq, seq);
        assert_eq!(stored[0].envelope.kind, "audit.mark");
    }

    /// Prune removes old rows; surviving seqs and cursors stay valid (never reused).
    #[tokio::test]
    async fn prune_keeps_seq_semantics() {
        let d = durable();
        let tenant = TenantId::new();
        let mut seqs = Vec::new();
        for n in 0..4 {
            seqs.push(d.publish(&env_for(tenant, "audit", n)).expect("publish"));
        }
        let removed = d.prune_before(seqs[2]).expect("prune");
        assert_eq!(removed, 2);
        let rest = d.replay(tenant, None, 0, 10).expect("replay");
        assert_eq!(
            rest.iter().map(|s| s.seq).collect::<Vec<_>>(),
            vec![seqs[2], seqs[3]],
            "pruned rows gone, surviving seqs unchanged"
        );
        // A new publish continues the sequence past the old maximum: no reuse.
        let next = d.publish(&env_for(tenant, "audit", 9)).expect("publish");
        assert!(next > seqs[3]);
    }
}
