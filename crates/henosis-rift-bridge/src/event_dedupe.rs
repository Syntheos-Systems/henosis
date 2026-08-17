//! Durable consumer-side replay suppression for stable Rift gateway events.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use henosis_sqlite::OpenedDatabase;
use rusqlite::config::DbConfig;
use rusqlite::{OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::rift_client::RiftMessageEvent;

/// Current on-disk schema understood by this bridge build.
const EVENT_LEDGER_SCHEMA_VERSION: i64 = 1;

/// Maximum distinct events retained before all later admission stops fail-closed.
///
/// Claims are intentionally never pruned automatically. At sustained traffic this
/// finite ceiling therefore requires an operator migration before the millionth
/// distinct event; silently recycling identities would weaken replay protection.
const MAX_RETAINED_EVENTS: i64 = 1_000_000;

/// Exact SQLite `table_xinfo` row expected for one canonical ledger column.
type ExpectedTableColumn<'a> = (i64, &'a str, &'a str, i64, Option<&'a str>, i64, i64);

/// SQLite schema for the constant-time retained-claim counter.
const CREATE_EVENT_LEDGER_METADATA: &str = r#"CREATE TABLE event_ledger_metadata (
    singleton INTEGER PRIMARY KEY
        CHECK (singleton = 1),
    retained_events INTEGER NOT NULL
        CHECK (retained_events >= 0 AND retained_events <= 1000000)
) STRICT;"#;

/// SQLite schema for payload-bound processing and completion claims.
const CREATE_PROCESSED_RIFT_EVENTS: &str = r#"CREATE TABLE processed_rift_events (
    event_id BLOB PRIMARY KEY NOT NULL
        CHECK (typeof(event_id) = 'blob' AND length(event_id) = 16),
    message_id BLOB NOT NULL
        CHECK (typeof(message_id) = 'blob' AND length(message_id) = 16),
    payload_fingerprint BLOB NOT NULL
        CHECK (typeof(payload_fingerprint) = 'blob' AND length(payload_fingerprint) = 32),
    status TEXT NOT NULL
        CHECK (status IN ('processing', 'completed')),
    claimed_at INTEGER NOT NULL,
    completed_at INTEGER,
    CHECK (
        (status = 'processing' AND completed_at IS NULL)
        OR (status = 'completed' AND completed_at IS NOT NULL)
    )
) STRICT, WITHOUT ROWID;
"#;

/// Failure to establish or mutate trustworthy durable event state.
#[derive(Debug, Error)]
pub(crate) enum EventStateError {
    /// The hardened filesystem boundary rejected the database path or leaf.
    #[error(transparent)]
    Open(#[from] henosis_sqlite::OpenError),
    /// SQLite could not prove or durably mutate the expected state.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// Durable state violated an application-level invariant.
    #[error("{0}")]
    Unsafe(String),
}

/// One SQLite connection serialized across event guards in this process.
struct LedgerState {
    /// Guarded SQLite connection containing durable claims.
    database: OpenedDatabase,
}

/// Private, server-scoped durable admission state shared with active guards.
#[derive(Clone)]
pub(crate) struct DurableEventLedger {
    /// Serialized connection state retained by every admitted event guard.
    state: Arc<Mutex<LedgerState>>,
    /// Bound path included in reconciliation diagnostics without message content.
    path: Arc<PathBuf>,
}

/// Result of atomically inspecting and, for new events, claiming one identity.
pub(crate) enum EventAdmission {
    /// This process owns the durable processing claim and may dispatch effects.
    Fresh(EventGuard),
    /// The same payload already completed and must not dispatch again.
    CompletedDuplicate,
    /// An earlier attempt may have produced effects and remains quarantined.
    ProcessingDuplicate {
        /// Unix timestamp at which the ambiguous attempt obtained its claim.
        claimed_at: i64,
    },
}

/// Capability proving a processing claim was durably inserted before dispatch.
pub(crate) struct EventGuard {
    /// Ledger that owns the claim.
    ledger: DurableEventLedger,
    /// Immutable event payload bound to the claim fingerprint.
    event: RiftMessageEvent,
    /// Fingerprint inserted during admission and required again at completion.
    fingerprint: [u8; 32],
}

/// Keeps failed-test diagnostics useful without exposing message content.
impl std::fmt::Debug for EventGuard {
    /// Render only stable identifiers and no payload text.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventGuard")
            .field("event_id", &self.event.event_id)
            .field("message_id", &self.event.message.id)
            .finish_non_exhaustive()
    }
}

/// Makes admission variants inspectable in tests without rendering payload content.
impl std::fmt::Debug for EventAdmission {
    /// Render the admission state and only the guard's safe identifiers.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh(guard) => formatter.debug_tuple("Fresh").field(guard).finish(),
            Self::CompletedDuplicate => formatter.write_str("CompletedDuplicate"),
            Self::ProcessingDuplicate { claimed_at } => formatter
                .debug_struct("ProcessingDuplicate")
                .field("claimed_at", claimed_at)
                .finish(),
        }
    }
}

/// Existing claim fields required to distinguish replay from identity collision.
struct StoredClaim {
    /// Message identity recorded with the event.
    message_id: Vec<u8>,
    /// Hash of every message field consumed by the room.
    fingerprint: Vec<u8>,
    /// Durable lifecycle state.
    status: String,
    /// Initial processing-claim time used for reconciliation.
    claimed_at: i64,
}

/// Implements durable event admission and completion transitions.
impl DurableEventLedger {
    /// Open, integrity-check, and migrate a private event ledger at one literal path.
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, EventStateError> {
        let path = path.as_ref().to_path_buf();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| {
                EventStateError::Unsafe(
                    "event ledger path must have an explicit private parent directory".to_string(),
                )
            })?;
        henosis_sqlite::ensure_private_directory(parent)?;
        let mut database = henosis_sqlite::open_database(&path)?;
        initialize_database(&mut database)?;
        Ok(Self {
            state: Arc::new(Mutex::new(LedgerState { database })),
            path: Arc::new(path),
        })
    }

    /// Build an isolated in-memory ledger for room unit tests that do not verify restart behavior.
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self, EventStateError> {
        let mut database = OpenedDatabase::open_in_memory()?;
        initialize_database(&mut database)?;
        Ok(Self {
            state: Arc::new(Mutex::new(LedgerState { database })),
            path: Arc::new(PathBuf::from(":memory:")),
        })
    }

    /// Offload durable admission so full-fsync SQLite never blocks an async worker.
    pub(crate) async fn admit(
        &self,
        event: RiftMessageEvent,
    ) -> Result<EventAdmission, EventStateError> {
        let ledger = self.clone();
        tokio::task::spawn_blocking(move || ledger.admit_blocking(event))
            .await
            .map_err(|error| {
                EventStateError::Unsafe(format!(
                    "event admission worker failed before dispatch permission: {error}"
                ))
            })?
    }

    /// Atomically return permission only after a new processing claim is durable.
    fn admit_blocking(&self, event: RiftMessageEvent) -> Result<EventAdmission, EventStateError> {
        let fingerprint = event_fingerprint(&event);
        let claimed_at = chrono::Utc::now().timestamp();
        let mut state = self.lock_state()?;
        let transaction = state
            .database
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = read_claim(&transaction, event.event_id)?;

        if let Some(stored) = stored {
            validate_matching_claim(&event, &fingerprint, &stored)?;
            transaction.commit()?;
            return match stored.status.as_str() {
                "completed" => Ok(EventAdmission::CompletedDuplicate),
                "processing" => {
                    tracing::error!(
                        event_id = %event.event_id,
                        message_id = %event.message.id,
                        claimed_at = stored.claimed_at,
                        ledger_path = %self.path.display(),
                        "Rift event has an ambiguous processing claim; automatic replay is quarantined and operator reconciliation is required"
                    );
                    Ok(EventAdmission::ProcessingDuplicate {
                        claimed_at: stored.claimed_at,
                    })
                }
                other => Err(EventStateError::Unsafe(format!(
                    "event {} has unsupported durable status {other:?}",
                    event.event_id
                ))),
            };
        }

        let reserved = transaction.execute(
            "UPDATE event_ledger_metadata \
             SET retained_events = retained_events + 1 \
             WHERE singleton = 1 AND retained_events < ?1",
            [MAX_RETAINED_EVENTS],
        )?;
        if reserved != 1 {
            return Err(EventStateError::Unsafe(format!(
                "event ledger reached its permanent fail-closed capacity of \
                 {MAX_RETAINED_EVENTS} retained claims; operator migration is required"
            )));
        }
        transaction.execute(
            "INSERT INTO processed_rift_events \
             (event_id, message_id, payload_fingerprint, status, claimed_at, completed_at) \
             VALUES (?1, ?2, ?3, 'processing', ?4, NULL)",
            rusqlite::params![
                event.event_id.as_bytes().as_slice(),
                event.message.id.as_bytes().as_slice(),
                fingerprint.as_slice(),
                claimed_at,
            ],
        )?;
        transaction.commit()?;
        Ok(EventAdmission::Fresh(EventGuard {
            ledger: self.clone(),
            event,
            fingerprint,
        }))
    }

    /// Obtain the connection lock or reject poisoned in-process state.
    fn lock_state(&self) -> Result<MutexGuard<'_, LedgerState>, EventStateError> {
        self.state.lock().map_err(|_| {
            EventStateError::Unsafe("event ledger lock was poisoned; dispatch denied".to_string())
        })
    }

    /// Transition claimed events to completed in one durable transaction.
    fn complete_guards(&self, guards: &[EventGuard]) -> Result<(), EventStateError> {
        if guards.is_empty() {
            return Ok(());
        }
        if guards
            .iter()
            .any(|guard| !Arc::ptr_eq(&guard.ledger.state, &self.state))
        {
            return Err(EventStateError::Unsafe(
                "completion batch mixed claims from different event ledgers".to_string(),
            ));
        }

        let completed_at = chrono::Utc::now().timestamp();
        let mut state = self.lock_state()?;
        let transaction = state
            .database
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for guard in guards {
            let stored = read_claim(&transaction, guard.event.event_id)?.ok_or_else(|| {
                EventStateError::Unsafe(format!(
                    "event {} lost its durable processing claim before completion",
                    guard.event.event_id
                ))
            })?;
            validate_matching_claim(&guard.event, &guard.fingerprint, &stored)?;
            match stored.status.as_str() {
                "processing" => {
                    let changed = transaction.execute(
                        "UPDATE processed_rift_events \
                         SET status = 'completed', completed_at = ?1 \
                         WHERE event_id = ?2 AND status = 'processing'",
                        rusqlite::params![completed_at, guard.event.event_id.as_bytes().as_slice()],
                    )?;
                    if changed != 1 {
                        return Err(EventStateError::Unsafe(format!(
                            "event {} completion did not update exactly one processing claim",
                            guard.event.event_id
                        )));
                    }
                }
                "completed" => {}
                other => {
                    return Err(EventStateError::Unsafe(format!(
                        "event {} has unsupported durable status {other:?}",
                        guard.event.event_id
                    )));
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

/// Implements access to a claimed event and its post-effect completion transition.
impl EventGuard {
    /// Borrow the immutable message consumed by the room.
    pub(crate) fn message(&self) -> &crate::types::RoomMessage {
        &self.event.message
    }

    /// Return the stable event identifier for safe diagnostics.
    pub(crate) fn event_id(&self) -> Uuid {
        self.event.event_id
    }

    /// Mark one successfully disposed event completed without blocking an async worker.
    pub(crate) async fn complete(self) -> Result<(), EventStateError> {
        let ledger = self.ledger.clone();
        tokio::task::spawn_blocking(move || ledger.complete_guards(std::slice::from_ref(&self)))
            .await
            .map_err(|error| {
                EventStateError::Unsafe(format!(
                    "event completion worker failed with claim retained as processing: {error}"
                ))
            })?
    }

    /// Atomically mark a cascade's successfully disposed events without blocking async work.
    pub(crate) async fn complete_all(guards: Vec<Self>) -> Result<(), EventStateError> {
        let Some(first) = guards.first() else {
            return Ok(());
        };
        let ledger = first.ledger.clone();
        tokio::task::spawn_blocking(move || ledger.complete_guards(&guards))
            .await
            .map_err(|error| {
                EventStateError::Unsafe(format!(
                    "event completion worker failed with claims retained as processing: {error}"
                ))
            })?
    }
}

/// Configure durability, validate integrity, and initialize the one supported schema.
fn initialize_database(database: &mut OpenedDatabase) -> Result<(), EventStateError> {
    database.busy_timeout(Duration::from_secs(5))?;
    database.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    database.execute_batch(
        "PRAGMA trusted_schema = OFF;\
         PRAGMA foreign_keys = ON;\
         PRAGMA journal_mode = DELETE;\
         PRAGMA synchronous = FULL;\
         PRAGMA cell_size_check = ON;",
    )?;
    let integrity: String = database.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(EventStateError::Unsafe(format!(
            "event ledger integrity check failed: {integrity}"
        )));
    }

    let schema_version: i64 = database.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match schema_version {
        0 => initialize_schema(database)?,
        EVENT_LEDGER_SCHEMA_VERSION => {}
        other => {
            return Err(EventStateError::Unsafe(format!(
                "unsupported event ledger schema version {other}"
            )));
        }
    }
    validate_schema_and_rows(database)
}

/// Create the first schema only when the dedicated database contains no application objects.
fn initialize_schema(database: &mut OpenedDatabase) -> Result<(), EventStateError> {
    let application_objects: i64 = database.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if application_objects != 0 {
        return Err(EventStateError::Unsafe(
            "unversioned event ledger already contains application objects".to_string(),
        ));
    }
    let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(CREATE_EVENT_LEDGER_METADATA)?;
    transaction.execute(
        "INSERT INTO event_ledger_metadata (singleton, retained_events) VALUES (1, 0)",
        [],
    )?;
    transaction.execute_batch(CREATE_PROCESSED_RIFT_EVENTS)?;
    transaction.pragma_update(None, "user_version", EVENT_LEDGER_SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

/// Prove exact object and column shapes, the O(1) counter, and every row invariant.
fn validate_schema_and_rows(database: &OpenedDatabase) -> Result<(), EventStateError> {
    let mut object_statement = database.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema \
         WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let objects = object_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                normalize_schema_sql(&row.get::<_, String>(3)?),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected_objects = vec![
        (
            "table".to_string(),
            "event_ledger_metadata".to_string(),
            "event_ledger_metadata".to_string(),
            normalize_schema_sql(CREATE_EVENT_LEDGER_METADATA),
        ),
        (
            "table".to_string(),
            "processed_rift_events".to_string(),
            "processed_rift_events".to_string(),
            normalize_schema_sql(CREATE_PROCESSED_RIFT_EVENTS),
        ),
    ];
    if objects != expected_objects {
        return Err(EventStateError::Unsafe(format!(
            "event ledger schema objects differ from the two expected tables: {objects:?}"
        )));
    }

    validate_table_columns(
        database,
        "event_ledger_metadata",
        &[
            (0, "singleton", "INTEGER", 0, None, 1, 0),
            (1, "retained_events", "INTEGER", 1, None, 0, 0),
        ],
    )?;
    validate_table_columns(
        database,
        "processed_rift_events",
        &[
            (0, "event_id", "BLOB", 1, None, 1, 0),
            (1, "message_id", "BLOB", 1, None, 0, 0),
            (2, "payload_fingerprint", "BLOB", 1, None, 0, 0),
            (3, "status", "TEXT", 1, None, 0, 0),
            (4, "claimed_at", "INTEGER", 1, None, 0, 0),
            (5, "completed_at", "INTEGER", 0, None, 0, 0),
        ],
    )?;

    let invalid_rows: i64 = database.query_row(
        "SELECT COUNT(*) FROM processed_rift_events \
         WHERE typeof(event_id) != 'blob' OR length(event_id) != 16 \
            OR typeof(message_id) != 'blob' OR length(message_id) != 16 \
            OR typeof(payload_fingerprint) != 'blob' OR length(payload_fingerprint) != 32 \
            OR status NOT IN ('processing', 'completed') \
            OR typeof(claimed_at) != 'integer' \
            OR (status = 'processing' AND completed_at IS NOT NULL) \
            OR (status = 'completed' AND typeof(completed_at) != 'integer')",
        [],
        |row| row.get(0),
    )?;
    if invalid_rows != 0 {
        return Err(EventStateError::Unsafe(format!(
            "event ledger contains {invalid_rows} invalid claim rows"
        )));
    }

    let retained_events: i64 = database.query_row(
        "SELECT retained_events FROM event_ledger_metadata WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let actual_events: i64 =
        database.query_row("SELECT COUNT(*) FROM processed_rift_events", [], |row| {
            row.get(0)
        })?;
    if retained_events != actual_events || !(0..=MAX_RETAINED_EVENTS).contains(&retained_events) {
        return Err(EventStateError::Unsafe(format!(
            "event ledger retained counter {retained_events} does not match \
             the validated claim count {actual_events}"
        )));
    }
    Ok(())
}

/// Canonicalize insignificant whitespace and a terminal semicolon in stored DDL.
fn normalize_schema_sql(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Require a fixed SQLite table to expose exactly the expected visible columns.
fn validate_table_columns(
    database: &OpenedDatabase,
    table: &str,
    expected: &[ExpectedTableColumn<'_>],
) -> Result<(), EventStateError> {
    let query = match table {
        "event_ledger_metadata" => {
            "SELECT cid, name, type, \"notnull\", dflt_value, pk, hidden \
             FROM pragma_table_xinfo('event_ledger_metadata') ORDER BY cid"
        }
        "processed_rift_events" => {
            "SELECT cid, name, type, \"notnull\", dflt_value, pk, hidden \
             FROM pragma_table_xinfo('processed_rift_events') ORDER BY cid"
        }
        _ => {
            return Err(EventStateError::Unsafe(format!(
                "refused dynamic event ledger table validation for {table:?}"
            )));
        }
    };
    let mut statement = database.prepare(query)?;
    let actual = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected_owned = expected
        .iter()
        .map(
            |(cid, name, kind, not_null, default, primary_key, hidden)| {
                (
                    *cid,
                    (*name).to_string(),
                    (*kind).to_string(),
                    *not_null,
                    default.map(str::to_string),
                    *primary_key,
                    *hidden,
                )
            },
        )
        .collect::<Vec<_>>();
    if actual != expected_owned {
        return Err(EventStateError::Unsafe(format!(
            "event ledger table {table:?} has unexpected columns: {actual:?}"
        )));
    }
    Ok(())
}

/// Read one durable claim without treating an absent identity as an error.
fn read_claim(
    connection: &rusqlite::Connection,
    event_id: Uuid,
) -> Result<Option<StoredClaim>, rusqlite::Error> {
    connection
        .query_row(
            "SELECT message_id, payload_fingerprint, status, claimed_at \
             FROM processed_rift_events WHERE event_id = ?1",
            [event_id.as_bytes().as_slice()],
            |row| {
                Ok(StoredClaim {
                    message_id: row.get(0)?,
                    fingerprint: row.get(1)?,
                    status: row.get(2)?,
                    claimed_at: row.get(3)?,
                })
            },
        )
        .optional()
}

/// Reject reuse of a stable event identity for any different consumed payload.
fn validate_matching_claim(
    event: &RiftMessageEvent,
    fingerprint: &[u8; 32],
    stored: &StoredClaim,
) -> Result<(), EventStateError> {
    if stored.message_id.as_slice() != event.message.id.as_bytes()
        || stored.fingerprint.as_slice() != fingerprint
    {
        return Err(EventStateError::Unsafe(format!(
            "event {} was replayed with a different payload fingerprint",
            event.event_id
        )));
    }
    Ok(())
}

/// Hash every message field capable of changing room behavior or attribution.
fn event_fingerprint(event: &RiftMessageEvent) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"henosis-rift-message-create-v1");
    hash_field(&mut hasher, event.message.id.as_bytes());
    hash_field(&mut hasher, event.message.channel_id.as_bytes());
    hash_field(&mut hasher, event.message.author_id.as_bytes());
    hash_field(&mut hasher, event.message.author_username.as_bytes());
    hash_field(&mut hasher, event.message.content.as_bytes());
    hash_field(&mut hasher, event.message.message_type.as_bytes());
    hash_field(
        &mut hasher,
        event
            .message
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            .as_bytes(),
    );
    hasher.finalize().into()
}

/// Add one unambiguous length-delimited byte field to a payload fingerprint.
fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(test)]
/// Tests for durable restart, collision, corruption, and filesystem boundaries.
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::{DurableEventLedger, EventAdmission};
    use crate::rift_client::RiftMessageEvent;
    use crate::types::RoomMessage;

    /// Build one stable gateway event around an independently selectable event ID.
    fn message_event(event_id: Uuid, content: &str) -> RiftMessageEvent {
        RiftMessageEvent {
            event_id,
            message: RoomMessage {
                id: Uuid::new_v4(),
                channel_id: Uuid::new_v4(),
                author_id: Uuid::new_v4(),
                author_username: "human".to_string(),
                content: content.to_string(),
                message_type: "user".to_string(),
                created_at: Utc::now(),
            },
        }
    }

    /// Allocate a unique test database path without sharing state across test processes.
    fn test_database_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!(
                "henosis-rift-event-dedupe-{label}-{}",
                Uuid::new_v4()
            ))
            .join("events.sqlite")
    }

    /// Restrict a test-created database leaf to the service-only Unix contract.
    #[cfg(unix)]
    fn make_database_private(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("make test database private");
    }

    /// Restrict a test-created parent to the service-only Unix contract.
    #[cfg(unix)]
    fn make_directory_private(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("make test directory private");
    }

    /// Windows security is established by its platform opener rather than Unix mode bits.
    #[cfg(not(unix))]
    fn make_database_private(_path: &std::path::Path) {}

    /// Windows directory security is established by its platform opener rather than Unix mode bits.
    #[cfg(not(unix))]
    fn make_directory_private(_path: &std::path::Path) {}

    /// A successful disposition remains suppressed after the database is closed and reopened.
    #[tokio::test]
    async fn committed_event_survives_reopen_while_distinct_event_remains_fresh() {
        let path = test_database_path("reopen");
        let event = message_event(Uuid::new_v4(), "first");
        let distinct = message_event(Uuid::new_v4(), "second");

        {
            let ledger = DurableEventLedger::open(&path).expect("open ledger");
            let EventAdmission::Fresh(guard) = ledger
                .admit(event.clone())
                .await
                .expect("admit first event")
            else {
                panic!("new event must be fresh");
            };
            guard.complete().await.expect("commit completed effect");
        }

        let reopened = DurableEventLedger::open(&path).expect("reopen ledger");
        assert!(matches!(
            reopened.admit(event).await.expect("inspect replay"),
            EventAdmission::CompletedDuplicate
        ));
        assert!(matches!(
            reopened
                .admit(distinct)
                .await
                .expect("inspect distinct event"),
            EventAdmission::Fresh(_)
        ));
    }

    /// Dropping an in-flight admission leaves a durable processing quarantine after restart.
    #[tokio::test]
    async fn abandoned_event_is_quarantined_after_reopen() {
        let path = test_database_path("abandon");
        let event = message_event(Uuid::new_v4(), "retry me");

        {
            let ledger = DurableEventLedger::open(&path).expect("open ledger");
            assert!(matches!(
                ledger.admit(event.clone()).await.expect("admit event"),
                EventAdmission::Fresh(_)
            ));
        }

        let reopened = DurableEventLedger::open(&path).expect("reopen ledger");
        assert!(matches!(
            reopened
                .admit(event)
                .await
                .expect("inspect abandoned event"),
            EventAdmission::ProcessingDuplicate { .. }
        ));
    }

    /// An identical replay is suppressed even while its first delivery is still in flight.
    #[tokio::test]
    async fn in_flight_replay_is_suppressed() {
        let path = test_database_path("in-flight");
        let event = message_event(Uuid::new_v4(), "same delivery");
        let ledger = DurableEventLedger::open(&path).expect("open ledger");
        let first = ledger.admit(event.clone()).await.expect("admit event");
        assert!(matches!(first, EventAdmission::Fresh(_)));
        assert!(matches!(
            ledger.admit(event).await.expect("inspect queued replay"),
            EventAdmission::ProcessingDuplicate { .. }
        ));
    }

    /// Reusing one durable identity for different content is corruption, not a duplicate.
    #[tokio::test]
    async fn event_id_payload_collision_fails_closed() {
        let path = test_database_path("collision");
        let event_id = Uuid::new_v4();
        let original = message_event(event_id, "original");
        let mut collision = original.clone();
        collision.message.content = "substituted".to_string();
        let ledger = DurableEventLedger::open(&path).expect("open ledger");
        let _first = ledger.admit(original).await.expect("admit original");

        let error = ledger
            .admit(collision)
            .await
            .expect_err("payload collision must fail closed");
        assert!(error.to_string().contains("different payload"));
    }

    /// Invalid SQLite bytes abort opening instead of silently replacing consumer history.
    #[test]
    fn corrupt_state_fails_closed() {
        let path = test_database_path("corrupt");
        std::fs::create_dir_all(path.parent().expect("database parent"))
            .expect("create database parent");
        make_directory_private(path.parent().expect("database parent"));
        std::fs::write(&path, b"not a sqlite database").expect("write corrupt database");
        make_database_private(&path);

        assert!(DurableEventLedger::open(path).is_err());
    }

    /// A same-column substitute missing canonical constraints and STRICT mode is rejected.
    #[test]
    fn schema_substitution_fails_closed() {
        let path = test_database_path("schema-substitution");
        std::fs::create_dir_all(path.parent().expect("database parent"))
            .expect("create database parent");
        make_directory_private(path.parent().expect("database parent"));
        {
            let database = rusqlite::Connection::open(&path).expect("open substitute database");
            database
                .execute_batch(
                    "CREATE TABLE event_ledger_metadata (\
                         singleton INTEGER PRIMARY KEY, retained_events INTEGER NOT NULL\
                     );\
                     INSERT INTO event_ledger_metadata VALUES (1, 0);\
                     CREATE TABLE processed_rift_events (\
                         event_id BLOB PRIMARY KEY NOT NULL, message_id BLOB NOT NULL,\
                         payload_fingerprint BLOB NOT NULL, status TEXT NOT NULL,\
                         claimed_at INTEGER NOT NULL, completed_at INTEGER\
                     );\
                     PRAGMA user_version = 1;",
                )
                .expect("create substitute schema");
        }
        make_database_private(&path);

        let error = match DurableEventLedger::open(path) {
            Ok(_) => panic!("substitute schema must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("schema objects differ"));
    }

    /// The hardened opener refuses a symlink in place of the dedicated database leaf.
    #[cfg(unix)]
    #[test]
    fn symlink_database_leaf_fails_closed() {
        use std::os::unix::fs::symlink;

        let target = test_database_path("symlink-target");
        let link = test_database_path("symlink-link");
        drop(DurableEventLedger::open(&target).expect("create target ledger"));
        std::fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");
        make_directory_private(link.parent().expect("link parent"));
        symlink(&target, &link).expect("create database symlink");

        assert!(DurableEventLedger::open(link).is_err());
    }

    /// The hardened opener refuses an existing ledger readable by another account class.
    #[cfg(unix)]
    #[test]
    fn group_readable_database_leaf_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_database_path("permissions");
        drop(DurableEventLedger::open(&path).expect("create private ledger"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("weaken test database mode");

        assert!(DurableEventLedger::open(path).is_err());
    }

    /// A missing ledger parent is created with private service-only Unix permissions.
    #[cfg(unix)]
    #[test]
    fn fresh_ledger_parent_is_created_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_database_path("private-parent");
        let parent = path.parent().expect("database parent");
        assert!(!parent.exists());

        drop(DurableEventLedger::open(&path).expect("create fresh ledger hierarchy"));

        let mode = std::fs::metadata(parent)
            .expect("inspect created parent")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
}
