#![deny(missing_docs)]
//! Synchronous, tenant-scoped audit chains and the Henosis witness protocol.
//!
//! An intent is committed to SQLite before a governed side effect may run. Production callers
//! then obtain and persist a receipt from an independently deployed witness. The witness has its
//! own signing key and storage, so compromising the Henosis process cannot silently rewrite both
//! copies of the audit head.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use reqwest::{Client, StatusCode, Url};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The fixed hash used before the first event in a tenant stream.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

const AUDIT_HASH_DOMAIN: &[u8] = b"henosis.audit.event.v1";
const CHECKPOINT_SIGNATURE_DOMAIN: &[u8] = b"henosis.audit.checkpoint.v1";
const RECEIPT_SIGNATURE_DOMAIN: &[u8] = b"henosis.audit.receipt.v1";
/// Maximum canonical JSON size retained for one replayable filtered result.
const MAX_SANITIZED_RESULT_BYTES: usize = 1024 * 1024;
/// Canonical schema for principal-scoped durable execution records.
const CREATE_EXECUTION_RECORDS_TABLE: &str = "
    CREATE TABLE execution_records (
        tenant_id TEXT NOT NULL,
        principal_id TEXT NOT NULL,
        idempotency_key TEXT NOT NULL,
        request_hash TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('claimed', 'completed', 'indeterminate')),
        sanitized_result_json TEXT,
        intent_sequence INTEGER NOT NULL,
        claimed_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        PRIMARY KEY (tenant_id, principal_id, idempotency_key),
        FOREIGN KEY (tenant_id, intent_sequence)
            REFERENCES audit_events (tenant_id, sequence),
        CHECK (
            (state = 'completed' AND sanitized_result_json IS NOT NULL)
            OR (state != 'completed' AND sanitized_result_json IS NULL)
        )
    );
";

/// A failure while appending, verifying, or witnessing an audit event.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// SQLite rejected a durable operation.
    #[error("audit storage error: {0}")]
    Storage(#[from] rusqlite::Error),
    /// JSON could not be canonicalized or decoded.
    #[error("audit serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// An audit input violated the metadata-only event contract.
    #[error("invalid audit input: {0}")]
    InvalidInput(String),
    /// An idempotency key was reused for different content.
    #[error("audit idempotency conflict")]
    IdempotencyConflict,
    /// No execution ledger row exists for the supplied tenant and retry key.
    #[error("audit execution record was not found")]
    ExecutionNotFound,
    /// An execution ledger transition conflicts with its durable terminal state.
    #[error("audit execution state does not permit this transition")]
    ExecutionStateConflict,
    /// A stored tenant chain failed cryptographic verification.
    #[error("audit chain verification failed at sequence {sequence}: {reason}")]
    ChainVerification {
        /// The sequence at which verification failed.
        sequence: u64,
        /// A non-sensitive explanation of the failed invariant.
        reason: String,
    },
    /// The in-process SQLite connection mutex was poisoned.
    #[error("audit storage lock was poisoned")]
    LockPoisoned,
    /// The remote witness transport failed.
    #[error("audit witness transport failed: {0}")]
    WitnessTransport(#[from] reqwest::Error),
    /// The remote witness URL was malformed.
    #[error("invalid witness URL")]
    WitnessUrl,
    /// The witness rejected a checkpoint.
    #[error("audit witness rejected checkpoint with status {0}")]
    WitnessRejected(StatusCode),
    /// A checkpoint or receipt signature was malformed or invalid.
    #[error("invalid audit signature")]
    InvalidSignature,
    /// A witness response did not match the submitted checkpoint.
    #[error("audit witness returned a mismatched receipt")]
    ReceiptMismatch,
    /// A witness key did not have the required Ed25519 length.
    #[error("invalid Ed25519 key material")]
    InvalidKey,
    /// A tenant audit stream is blocked after an ambiguous completion boundary.
    #[error("audit stream is blocked: {reason_code}")]
    StreamBlocked {
        /// Stable non-sensitive reason for the block.
        reason_code: String,
    },
}

/// Distinguishes pre-side-effect intent records from post-side-effect outcomes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditPhase {
    /// A record committed before the governed side effect.
    Intent,
    /// A record committed after the governed side effect returns.
    Outcome,
}

/// Supplies metadata for one append to a tenant audit stream.
#[derive(Clone, Debug)]
pub struct AuditEventInput {
    /// Tenant that owns the stream.
    pub tenant_id: String,
    /// Authenticated principal responsible for the action.
    pub principal_id: String,
    /// Stable action name such as `tool.invoke`.
    pub action: String,
    /// Whether this event records intent or outcome.
    pub phase: AuditPhase,
    /// Canonical governed request whose SHA-256 digest is derived inside the audit boundary.
    pub request: Value,
    /// Metadata-only event payload.
    pub payload: Value,
    /// Retry key scoped to tenant, principal, and phase.
    pub idempotency_key: Option<String>,
}

/// One immutable row in a tenant audit hash chain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditRecord {
    /// Tenant that owns the stream.
    pub tenant_id: String,
    /// Monotonic sequence within the tenant stream.
    pub sequence: u64,
    /// Globally unique event identifier.
    pub event_id: String,
    /// Authenticated principal responsible for the action.
    pub principal_id: String,
    /// Stable action name.
    pub action: String,
    /// Whether the event records intent or outcome.
    pub phase: AuditPhase,
    /// SHA-256 hash of the canonical governed request.
    pub request_hash: String,
    /// Canonical metadata-only JSON payload.
    pub payload: Value,
    /// Previous event hash, or [`GENESIS_HASH`] for the first row.
    pub previous_hash: String,
    /// SHA-256 hash over the complete immutable record.
    pub event_hash: String,
    /// Server-assigned Unix timestamp in milliseconds.
    pub created_at_ms: i64,
    /// Retry key scoped to tenant, principal, and phase.
    pub idempotency_key: Option<String>,
    /// Persisted witness receipt when one has been obtained.
    pub witness_receipt: Option<WitnessReceipt>,
}

/// Persistent fail-closed state for one tenant audit stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditStreamState {
    /// Tenant that owns the stream.
    pub tenant_id: String,
    /// Whether execution is blocked for the stream.
    pub blocked: bool,
    /// Stable non-sensitive reason for the current block.
    pub reason_code: Option<String>,
    /// Server-assigned Unix timestamp in milliseconds.
    pub updated_at_ms: i64,
}

/// Durable lifecycle state for one idempotent execution.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    /// Durable intent exists and the side effect may have started.
    Claimed,
    /// The side effect completed and its final filtered result is replayable.
    Completed,
    /// The side effect may have completed but no replayable result is known.
    Indeterminate,
}

/// One tenant-and-principal-scoped at-most-once execution ledger row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionRecord {
    /// Tenant that owns the execution.
    pub tenant_id: String,
    /// Authenticated principal that owns the execution.
    pub principal_id: String,
    /// Caller retry key scoped to the tenant and principal.
    pub idempotency_key: String,
    /// SHA-256 hash of the canonical governed request.
    pub request_hash: String,
    /// Current durable execution lifecycle.
    pub state: ExecutionState,
    /// Final output-filtered result available only for completed executions.
    pub sanitized_result: Option<Value>,
    /// Audit-stream sequence containing the atomically appended intent.
    pub intent_sequence: u64,
    /// Server-assigned claim timestamp in milliseconds.
    pub claimed_at_ms: i64,
    /// Server-assigned timestamp of the latest state transition.
    pub updated_at_ms: i64,
}

/// Result of atomically claiming an idempotent execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionClaim {
    /// This caller durably created the claim and may invoke the executor once.
    Acquired(ExecutionRecord),
    /// The principal-scoped key already existed with the same canonical request hash.
    Existing(ExecutionRecord),
}

/// A signed statement asking a witness to preserve an audit stream head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WitnessCheckpoint {
    /// Tenant whose stream is being witnessed.
    pub tenant_id: String,
    /// Monotonic sequence being witnessed.
    pub sequence: u64,
    /// Event identifier at the submitted head.
    pub event_id: String,
    /// Previous event hash at the submitted head.
    pub previous_hash: String,
    /// Event hash at the submitted head.
    pub event_hash: String,
    /// Configured identifier for the Henosis origin key.
    pub origin_key_id: String,
    /// Base64 Ed25519 signature over the checkpoint.
    pub origin_signature_b64: String,
}

/// A witness-signed acknowledgement of one preserved audit stream head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WitnessReceipt {
    /// Tenant whose stream head was preserved.
    pub tenant_id: String,
    /// Monotonic sequence preserved by the witness.
    pub sequence: u64,
    /// Event hash preserved by the witness.
    pub event_hash: String,
    /// Configured identifier for the witness signing key.
    pub witness_key_id: String,
    /// Witness-assigned Unix timestamp in milliseconds.
    pub witnessed_at_ms: i64,
    /// Base64 Ed25519 signature over the receipt.
    pub signature_b64: String,
}

/// Signs checkpoints with the Henosis origin key kept outside the audit database.
#[derive(Clone)]
pub struct OriginSigner {
    key_id: String,
    signing_key: SigningKey,
}

/// Implements origin signing for witness checkpoints.
impl OriginSigner {
    /// Constructs a signer from a configured key identifier and Ed25519 signing key.
    pub fn new(key_id: impl Into<String>, signing_key: SigningKey) -> Result<Self, AuditError> {
        let key_id = key_id.into();
        validate_identifier("origin key id", &key_id)?;
        Ok(Self {
            key_id,
            signing_key,
        })
    }

    /// Returns the public key corresponding to this origin signer.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Converts an immutable audit record into an origin-signed checkpoint.
    pub fn checkpoint(&self, record: &AuditRecord) -> WitnessCheckpoint {
        let mut checkpoint = WitnessCheckpoint {
            tenant_id: record.tenant_id.clone(),
            sequence: record.sequence,
            event_id: record.event_id.clone(),
            previous_hash: record.previous_hash.clone(),
            event_hash: record.event_hash.clone(),
            origin_key_id: self.key_id.clone(),
            origin_signature_b64: String::new(),
        };
        let signature = self
            .signing_key
            .sign(&checkpoint_signing_bytes(&checkpoint));
        checkpoint.origin_signature_b64 = BASE64.encode(signature.to_bytes());
        checkpoint
    }
}

/// Synchronous SQLite store for immutable, tenant-scoped audit chains.
#[derive(Clone)]
pub struct AuditStore {
    connection: Arc<Mutex<Connection>>,
}

/// Implements durable audit append and verification operations.
impl AuditStore {
    /// Opens or creates an audit database and applies its schema.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, AuditError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    /// Creates an in-memory audit database for isolated tests and local evaluation.
    pub fn open_in_memory() -> Result<Self, AuditError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Initializes a store around an already-open SQLite connection.
    fn from_connection(mut connection: Connection) -> Result<Self, AuditError> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS audit_events (
                tenant_id TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                event_id TEXT NOT NULL,
                principal_id TEXT NOT NULL,
                action TEXT NOT NULL,
                phase TEXT NOT NULL CHECK (phase IN ('intent', 'outcome')),
                request_hash TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                previous_hash TEXT NOT NULL,
                event_hash TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                idempotency_key TEXT,
                witness_receipt_json TEXT,
                PRIMARY KEY (tenant_id, sequence),
                UNIQUE (event_id)
            );
            CREATE TABLE IF NOT EXISTS audit_stream_state (
                tenant_id TEXT PRIMARY KEY,
                blocked INTEGER NOT NULL CHECK (blocked IN (0, 1)),
                reason_code TEXT,
                updated_at_ms INTEGER NOT NULL
            );
            ",
        )?;
        apply_principal_idempotency_schema(&mut connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Appends one event atomically and returns the existing identical row on safe retry.
    pub fn append(&self, input: AuditEventInput) -> Result<AuditRecord, AuditError> {
        validate_input(&input)?;
        let request_hash = hash_canonical_json(&input.request)?;
        let payload_json = canonical_json_string(&input.payload)?;
        validate_metadata_payload(&input.payload)?;

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_stream_writable_in(&transaction, &input.tenant_id)?;
        let record = append_in_transaction(&transaction, &input, &request_hash, &payload_json)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Atomically appends durable intent and claims one principal-scoped retry key.
    ///
    /// A new claim returns [`ExecutionClaim::Acquired`]. Reusing the key with the same canonical
    /// request returns [`ExecutionClaim::Existing`] in its durable state, while different
    /// canonical request content returns [`AuditError::IdempotencyConflict`].
    pub fn claim_execution(&self, intent: AuditEventInput) -> Result<ExecutionClaim, AuditError> {
        validate_execution_event(&intent, AuditPhase::Intent)?;
        let idempotency_key = intent
            .idempotency_key
            .as_deref()
            .ok_or_else(|| {
                AuditError::InvalidInput("execution intent requires an idempotency key".to_string())
            })?
            .to_string();
        let request_hash = hash_canonical_json(&intent.request)?;
        let payload_json = canonical_json_string(&intent.payload)?;
        validate_metadata_payload(&intent.payload)?;

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_execution_record(
            &transaction,
            &intent.tenant_id,
            &intent.principal_id,
            &idempotency_key,
        )? {
            ensure_execution_hash(&existing, &request_hash)?;
            transaction.commit()?;
            return Ok(ExecutionClaim::Existing(existing));
        }
        ensure_stream_writable_in(&transaction, &intent.tenant_id)?;

        let intent_record =
            append_in_transaction(&transaction, &intent, &request_hash, &payload_json)?;
        transaction.execute(
            "INSERT INTO execution_records (
                tenant_id, principal_id, idempotency_key, request_hash, state,
                sanitized_result_json, intent_sequence, claimed_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 'claimed', NULL, ?5, ?6, ?6)",
            params![
                intent.tenant_id,
                intent.principal_id,
                idempotency_key,
                request_hash,
                intent_record.sequence,
                intent_record.created_at_ms,
            ],
        )?;
        let record = load_execution_record(
            &transaction,
            &intent.tenant_id,
            &intent.principal_id,
            &idempotency_key,
        )?
        .ok_or(AuditError::ExecutionNotFound)?;
        transaction.commit()?;
        Ok(ExecutionClaim::Acquired(record))
    }

    /// Atomically appends a successful outcome and stores its final output-filtered result.
    ///
    /// Repeating an identical completion returns the existing row. A different result or a
    /// transition from an indeterminate execution is rejected without changing durable state.
    pub fn complete_execution(
        &self,
        outcome: AuditEventInput,
        sanitized_result: Value,
    ) -> Result<ExecutionRecord, AuditError> {
        validate_execution_event(&outcome, AuditPhase::Outcome)?;
        let idempotency_key = outcome
            .idempotency_key
            .as_deref()
            .ok_or_else(|| {
                AuditError::InvalidInput(
                    "execution outcome requires an idempotency key".to_string(),
                )
            })?
            .to_string();
        let request_hash = hash_canonical_json(&outcome.request)?;
        let payload_json = canonical_json_string(&outcome.payload)?;
        validate_metadata_payload(&outcome.payload)?;
        let result_json = canonical_json_string(&sanitized_result)?;
        if result_json.len() > MAX_SANITIZED_RESULT_BYTES {
            return Err(AuditError::InvalidInput(
                "sanitized execution result exceeds the storage limit".to_string(),
            ));
        }

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_stream_writable_in(&transaction, &outcome.tenant_id)?;
        let existing = load_execution_record(
            &transaction,
            &outcome.tenant_id,
            &outcome.principal_id,
            &idempotency_key,
        )?
        .ok_or(AuditError::ExecutionNotFound)?;
        ensure_execution_hash(&existing, &request_hash)?;
        match existing.state {
            ExecutionState::Completed => {
                let existing_result_json = existing
                    .sanitized_result
                    .as_ref()
                    .map(canonical_json_string)
                    .transpose()?;
                if existing_result_json.as_deref() != Some(result_json.as_str()) {
                    return Err(AuditError::ExecutionStateConflict);
                }
                transaction.commit()?;
                return Ok(existing);
            }
            ExecutionState::Indeterminate => {
                return Err(AuditError::ExecutionStateConflict);
            }
            ExecutionState::Claimed => {}
        }

        append_in_transaction(&transaction, &outcome, &request_hash, &payload_json)?;
        let updated_at_ms = unix_timestamp_ms()?;
        let changed = transaction.execute(
            "UPDATE execution_records
             SET state = 'completed', sanitized_result_json = ?1, updated_at_ms = ?2
             WHERE tenant_id = ?3 AND principal_id = ?4 AND idempotency_key = ?5
                 AND state = 'claimed'",
            params![
                result_json,
                updated_at_ms,
                outcome.tenant_id,
                outcome.principal_id,
                idempotency_key
            ],
        )?;
        if changed != 1 {
            return Err(AuditError::ExecutionStateConflict);
        }
        let record = load_execution_record(
            &transaction,
            &outcome.tenant_id,
            &outcome.principal_id,
            &idempotency_key,
        )?
        .ok_or(AuditError::ExecutionNotFound)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Marks a claimed execution indeterminate when its side-effect result cannot be persisted.
    ///
    /// Repeating the transition is idempotent. A completed execution is never downgraded.
    pub fn mark_execution_indeterminate(
        &self,
        tenant_id: &str,
        principal_id: &str,
        idempotency_key: &str,
        request: &Value,
    ) -> Result<ExecutionRecord, AuditError> {
        validate_identifier("tenant id", tenant_id)?;
        validate_identifier("principal id", principal_id)?;
        validate_identifier("idempotency key", idempotency_key)?;
        let request_hash = hash_canonical_json(request)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing =
            load_execution_record(&transaction, tenant_id, principal_id, idempotency_key)?
                .ok_or(AuditError::ExecutionNotFound)?;
        ensure_execution_hash(&existing, &request_hash)?;
        match existing.state {
            ExecutionState::Completed => {
                return Err(AuditError::ExecutionStateConflict);
            }
            ExecutionState::Indeterminate => {
                transaction.commit()?;
                return Ok(existing);
            }
            ExecutionState::Claimed => {}
        }

        let updated_at_ms = unix_timestamp_ms()?;
        let changed = transaction.execute(
            "UPDATE execution_records
             SET state = 'indeterminate', sanitized_result_json = NULL, updated_at_ms = ?1
             WHERE tenant_id = ?2 AND principal_id = ?3 AND idempotency_key = ?4
                 AND state = 'claimed'",
            params![updated_at_ms, tenant_id, principal_id, idempotency_key],
        )?;
        if changed != 1 {
            return Err(AuditError::ExecutionStateConflict);
        }
        let record = load_execution_record(&transaction, tenant_id, principal_id, idempotency_key)?
            .ok_or(AuditError::ExecutionNotFound)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Loads one execution ledger row by its tenant-and-principal-scoped retry key.
    pub fn execution_record(
        &self,
        tenant_id: &str,
        principal_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<ExecutionRecord>, AuditError> {
        validate_identifier("tenant id", tenant_id)?;
        validate_identifier("principal id", principal_id)?;
        validate_identifier("idempotency key", idempotency_key)?;
        let connection = self.lock_connection()?;
        load_execution_record(&connection, tenant_id, principal_id, idempotency_key)
    }

    /// Returns the current fail-closed state for one tenant stream.
    pub fn stream_state(&self, tenant_id: &str) -> Result<Option<AuditStreamState>, AuditError> {
        validate_identifier("tenant id", tenant_id)?;
        let connection = self.lock_connection()?;
        load_stream_state(&connection, tenant_id)
    }

    /// Rejects execution when a tenant stream has an unresolved ambiguous completion.
    pub fn ensure_stream_writable(&self, tenant_id: &str) -> Result<(), AuditError> {
        validate_identifier("tenant id", tenant_id)?;
        let connection = self.lock_connection()?;
        ensure_stream_writable_in(&connection, tenant_id)
    }

    /// Persistently blocks a tenant after an outcome cannot be durably witnessed.
    pub fn mark_stream_ambiguous(
        &self,
        tenant_id: &str,
        reason_code: &str,
    ) -> Result<(), AuditError> {
        validate_identifier("tenant id", tenant_id)?;
        validate_identifier("reason code", reason_code)?;
        let updated_at_ms = unix_timestamp_ms()?;
        let connection = self.lock_connection()?;
        connection.execute(
            "INSERT INTO audit_stream_state (tenant_id, blocked, reason_code, updated_at_ms)
             VALUES (?1, 1, ?2, ?3)
             ON CONFLICT(tenant_id) DO UPDATE SET
                blocked = 1,
                reason_code = excluded.reason_code,
                updated_at_ms = excluded.updated_at_ms",
            params![tenant_id, reason_code, updated_at_ms],
        )?;
        Ok(())
    }

    /// Persists a verified witness receipt alongside its immutable audit event.
    pub fn attach_witness_receipt(
        &self,
        receipt: &WitnessReceipt,
        witness_key: &VerifyingKey,
    ) -> Result<(), AuditError> {
        verify_witness_receipt(receipt, witness_key)?;
        let receipt_json = serde_json::to_string(receipt)?;
        let connection = self.lock_connection()?;
        let changed = connection.execute(
            "UPDATE audit_events
             SET witness_receipt_json = ?1
             WHERE tenant_id = ?2 AND sequence = ?3 AND event_hash = ?4
               AND (witness_receipt_json IS NULL OR witness_receipt_json = ?1)",
            params![
                receipt_json,
                receipt.tenant_id,
                receipt.sequence,
                receipt.event_hash
            ],
        )?;
        if changed != 1 {
            return Err(AuditError::ReceiptMismatch);
        }
        Ok(())
    }

    /// Loads one record from a tenant stream.
    pub fn get(&self, tenant_id: &str, sequence: u64) -> Result<Option<AuditRecord>, AuditError> {
        let connection = self.lock_connection()?;
        load_record(&connection, tenant_id, sequence)
    }

    /// Returns the current tenant stream head, if the stream is non-empty.
    pub fn head(&self, tenant_id: &str) -> Result<Option<AuditRecord>, AuditError> {
        let connection = self.lock_connection()?;
        connection
            .query_row(
                &format!("{SELECT_RECORD} WHERE tenant_id = ?1 ORDER BY sequence DESC LIMIT 1"),
                [tenant_id],
                record_from_row,
            )
            .optional()
            .map_err(AuditError::from)
    }

    /// Verifies every link and event hash in a tenant stream, returning the verified row count.
    pub fn verify_tenant(&self, tenant_id: &str) -> Result<u64, AuditError> {
        validate_identifier("tenant id", tenant_id)?;
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(&format!(
            "{SELECT_RECORD} WHERE tenant_id = ?1 ORDER BY sequence ASC"
        ))?;
        let records = statement
            .query_map([tenant_id], record_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut expected_sequence = 1_u64;
        let mut expected_previous = GENESIS_HASH.to_owned();
        for record in &records {
            if record.sequence != expected_sequence {
                return Err(verification_error(record.sequence, "sequence gap"));
            }
            if record.previous_hash != expected_previous {
                return Err(verification_error(
                    record.sequence,
                    "previous hash mismatch",
                ));
            }
            let payload_json = canonical_json_string(&record.payload)?;
            let expected_hash = compute_event_hash(
                &record.tenant_id,
                record.sequence,
                &record.event_id,
                &record.principal_id,
                &record.action,
                record.phase,
                &record.request_hash,
                &payload_json,
                &record.previous_hash,
                record.created_at_ms,
                record.idempotency_key.as_deref(),
            );
            if record.event_hash != expected_hash {
                return Err(verification_error(record.sequence, "event hash mismatch"));
            }
            expected_previous.clone_from(&record.event_hash);
            expected_sequence += 1;
        }
        Ok(records.len() as u64)
    }

    /// Acquires the SQLite connection or reports poison without panicking.
    fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>, AuditError> {
        self.connection.lock().map_err(|_| AuditError::LockPoisoned)
    }
}

/// Client for an independently deployed Henosis witness.
#[derive(Clone)]
pub struct WitnessClient {
    client: Client,
    checkpoint_url: Url,
    expected_key_id: String,
    expected_key: VerifyingKey,
}

/// Couples local audit persistence with mandatory remote checkpoint receipt verification.
#[derive(Clone)]
pub struct WitnessedAudit {
    store: AuditStore,
    origin_signer: OriginSigner,
    witness_client: WitnessClient,
}

/// Implements the synchronous witness boundary used around governed execution.
impl WitnessedAudit {
    /// Constructs a witnessed audit boundary from its independently configured authorities.
    pub fn new(
        store: AuditStore,
        origin_signer: OriginSigner,
        witness_client: WitnessClient,
    ) -> Self {
        Self {
            store,
            origin_signer,
            witness_client,
        }
    }

    /// Appends locally, obtains a valid off-host receipt, and persists it before returning.
    pub async fn append(&self, input: AuditEventInput) -> Result<AuditRecord, AuditError> {
        let record = self.store.append(input)?;
        self.witness_record(record).await
    }

    /// Witnesses a durable intent, then atomically claims execution before allowing a side effect.
    pub async fn claim_execution(
        &self,
        intent: AuditEventInput,
    ) -> Result<ExecutionClaim, AuditError> {
        validate_execution_event(&intent, AuditPhase::Intent)?;
        let idempotency_key = intent
            .idempotency_key
            .as_deref()
            .ok_or_else(|| {
                AuditError::InvalidInput("execution intent requires an idempotency key".to_string())
            })?
            .to_string();
        if self
            .store
            .execution_record(&intent.tenant_id, &intent.principal_id, &idempotency_key)?
            .is_some()
        {
            return self.store.claim_execution(intent);
        }
        let intent_record = self.store.append(intent.clone())?;
        self.witness_record(intent_record).await?;
        self.store.claim_execution(intent)
    }

    /// Witnesses a successful outcome before making its filtered result replayable.
    pub async fn complete_execution(
        &self,
        outcome: AuditEventInput,
        sanitized_result: Value,
    ) -> Result<ExecutionRecord, AuditError> {
        validate_execution_event(&outcome, AuditPhase::Outcome)?;
        let idempotency_key = outcome
            .idempotency_key
            .as_deref()
            .ok_or_else(|| {
                AuditError::InvalidInput(
                    "execution outcome requires an idempotency key".to_string(),
                )
            })?
            .to_string();
        let request_hash = hash_canonical_json(&outcome.request)?;
        let existing = self
            .store
            .execution_record(&outcome.tenant_id, &outcome.principal_id, &idempotency_key)?
            .ok_or(AuditError::ExecutionNotFound)?;
        ensure_execution_hash(&existing, &request_hash)?;
        if existing.state != ExecutionState::Claimed {
            return self.store.complete_execution(outcome, sanitized_result);
        }
        let outcome_record = self.store.append(outcome.clone())?;
        self.witness_record(outcome_record).await?;
        self.store.complete_execution(outcome, sanitized_result)
    }

    /// Returns the underlying local store for verification and stream blocking.
    pub fn store(&self) -> &AuditStore {
        &self.store
    }

    /// Obtains and persists the independent receipt for one local audit row.
    async fn witness_record(&self, record: AuditRecord) -> Result<AuditRecord, AuditError> {
        if record.witness_receipt.is_some() {
            return Ok(record);
        }
        let checkpoint = self.origin_signer.checkpoint(&record);
        let receipt = self.witness_client.checkpoint(&checkpoint).await?;
        self.store
            .attach_witness_receipt(&receipt, &self.witness_client.expected_key)?;
        self.store
            .get(&record.tenant_id, record.sequence)?
            .ok_or(AuditError::ReceiptMismatch)
    }
}

/// Implements witness submission and receipt verification.
impl WitnessClient {
    /// Builds a witness client with a strict request timeout and disabled redirects.
    pub fn new(
        base_url: &str,
        expected_key_id: impl Into<String>,
        expected_key: VerifyingKey,
        timeout: Duration,
    ) -> Result<Self, AuditError> {
        let mut checkpoint_url = Url::parse(base_url).map_err(|_| AuditError::WitnessUrl)?;
        if checkpoint_url.scheme() != "https"
            || !checkpoint_url.username().is_empty()
            || checkpoint_url.password().is_some()
            || checkpoint_url.host_str().is_none()
        {
            return Err(AuditError::WitnessUrl);
        }
        checkpoint_url.set_path("/v1/checkpoints");
        checkpoint_url.set_query(None);
        checkpoint_url.set_fragment(None);
        let expected_key_id = expected_key_id.into();
        validate_identifier("witness key id", &expected_key_id)?;
        let client = Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            checkpoint_url,
            expected_key_id,
            expected_key,
        })
    }

    /// Submits a checkpoint and verifies that the receipt exactly matches it.
    pub async fn checkpoint(
        &self,
        checkpoint: &WitnessCheckpoint,
    ) -> Result<WitnessReceipt, AuditError> {
        let response = self
            .client
            .post(self.checkpoint_url.clone())
            .header(
                "X-Henosis-Idempotency-Key",
                format!("{}:{}", checkpoint.tenant_id, checkpoint.sequence),
            )
            .json(checkpoint)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(AuditError::WitnessRejected(response.status()));
        }
        let receipt: WitnessReceipt = response.json().await?;
        if receipt.tenant_id != checkpoint.tenant_id
            || receipt.sequence != checkpoint.sequence
            || receipt.event_hash != checkpoint.event_hash
            || receipt.witness_key_id != self.expected_key_id
        {
            return Err(AuditError::ReceiptMismatch);
        }
        verify_witness_receipt(&receipt, &self.expected_key)?;
        Ok(receipt)
    }
}

/// Verifies an origin signature on a witness checkpoint.
pub fn verify_checkpoint_signature(
    checkpoint: &WitnessCheckpoint,
    origin_key: &VerifyingKey,
) -> Result<(), AuditError> {
    let signature_bytes = BASE64
        .decode(&checkpoint.origin_signature_b64)
        .map_err(|_| AuditError::InvalidSignature)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| AuditError::InvalidSignature)?;
    origin_key
        .verify(&checkpoint_signing_bytes(checkpoint), &signature)
        .map_err(|_| AuditError::InvalidSignature)
}

/// Signs a witness receipt with the witness key.
pub fn sign_witness_receipt(
    tenant_id: String,
    sequence: u64,
    event_hash: String,
    witness_key_id: String,
    witnessed_at_ms: i64,
    signing_key: &SigningKey,
) -> WitnessReceipt {
    let mut receipt = WitnessReceipt {
        tenant_id,
        sequence,
        event_hash,
        witness_key_id,
        witnessed_at_ms,
        signature_b64: String::new(),
    };
    let signature = signing_key.sign(&receipt_signing_bytes(&receipt));
    receipt.signature_b64 = BASE64.encode(signature.to_bytes());
    receipt
}

/// Verifies the witness signature on a receipt.
pub fn verify_witness_receipt(
    receipt: &WitnessReceipt,
    witness_key: &VerifyingKey,
) -> Result<(), AuditError> {
    let signature_bytes = BASE64
        .decode(&receipt.signature_b64)
        .map_err(|_| AuditError::InvalidSignature)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| AuditError::InvalidSignature)?;
    witness_key
        .verify(&receipt_signing_bytes(receipt), &signature)
        .map_err(|_| AuditError::InvalidSignature)
}

/// Returns the SHA-256 digest of canonical JSON as lowercase hexadecimal.
pub fn hash_canonical_json(value: &Value) -> Result<String, AuditError> {
    let bytes = canonical_json_string(value)?;
    Ok(hex_digest(Sha256::digest(bytes.as_bytes())))
}

/// Returns the current Unix timestamp in milliseconds.
pub fn unix_timestamp_ms() -> Result<i64, AuditError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuditError::InvalidInput("system clock precedes Unix epoch".into()))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| AuditError::InvalidInput("system clock is outside supported range".into()))
}

/// Applies principal-scoped idempotency indexes and migrates legacy execution rows atomically.
fn apply_principal_idempotency_schema(connection: &mut Connection) -> Result<(), AuditError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "
        DROP INDEX IF EXISTS audit_events_idempotency;
        CREATE UNIQUE INDEX IF NOT EXISTS audit_events_principal_idempotency
            ON audit_events (tenant_id, principal_id, phase, idempotency_key)
            WHERE idempotency_key IS NOT NULL;
        ",
    )?;

    let execution_table_exists = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'execution_records'
        )",
        [],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if !execution_table_exists {
        transaction.execute_batch(CREATE_EXECUTION_RECORDS_TABLE)?;
    } else if !execution_records_have_principal_namespace(&transaction)? {
        transaction
            .execute_batch("ALTER TABLE execution_records RENAME TO execution_records_legacy;")?;
        transaction.execute_batch(CREATE_EXECUTION_RECORDS_TABLE)?;
        let legacy_count =
            transaction.query_row("SELECT COUNT(*) FROM execution_records_legacy", [], |row| {
                row.get::<_, i64>(0)
            })?;
        let migrated_count = transaction.execute(
            "INSERT INTO execution_records (
                tenant_id, principal_id, idempotency_key, request_hash, state,
                sanitized_result_json, intent_sequence, claimed_at_ms, updated_at_ms
             )
             SELECT legacy.tenant_id, intent.principal_id, legacy.idempotency_key,
                    legacy.request_hash, legacy.state, legacy.sanitized_result_json,
                    legacy.intent_sequence, legacy.claimed_at_ms, legacy.updated_at_ms
             FROM execution_records_legacy AS legacy
             JOIN audit_events AS intent
               ON intent.tenant_id = legacy.tenant_id
              AND intent.sequence = legacy.intent_sequence
              AND intent.phase = 'intent'
              AND intent.idempotency_key = legacy.idempotency_key
              AND intent.request_hash = legacy.request_hash",
            [],
        )?;
        if i64::try_from(migrated_count).ok() != Some(legacy_count) {
            return Err(AuditError::InvalidInput(
                "execution ledger migration could not bind every row to its principal".to_string(),
            ));
        }
        transaction.execute_batch("DROP TABLE execution_records_legacy;")?;
    }
    transaction.commit()?;
    Ok(())
}

/// Detects whether an execution ledger uses the exact tenant, principal, and key primary key.
fn execution_records_have_principal_namespace(connection: &Connection) -> Result<bool, AuditError> {
    let mut statement = connection.prepare("PRAGMA table_info(execution_records)")?;
    let mut rows = statement.query([])?;
    let mut tenant_position = 0;
    let mut principal_position = 0;
    let mut key_position = 0;
    while let Some(row) = rows.next()? {
        let name = row.get::<_, String>(1)?;
        let primary_key_position = row.get::<_, i64>(5)?;
        match name.as_str() {
            "tenant_id" => tenant_position = primary_key_position,
            "principal_id" => principal_position = primary_key_position,
            "idempotency_key" => key_position = primary_key_position,
            _ => {}
        }
    }
    Ok(tenant_position == 1 && principal_position == 2 && key_position == 3)
}

const SELECT_RECORD: &str = "SELECT tenant_id, sequence, event_id, principal_id, action, phase,
    request_hash, payload_json, previous_hash, event_hash, created_at_ms, idempotency_key,
    witness_receipt_json FROM audit_events";

const SELECT_EXECUTION_RECORD: &str = "SELECT tenant_id, principal_id, idempotency_key,
    request_hash, state, sanitized_result_json, intent_sequence, claimed_at_ms, updated_at_ms
    FROM execution_records";

/// Appends or reloads one idempotent audit event inside the caller's immediate transaction.
fn append_in_transaction(
    transaction: &Transaction<'_>,
    input: &AuditEventInput,
    request_hash: &str,
    payload_json: &str,
) -> Result<AuditRecord, AuditError> {
    if let Some(idempotency_key) = input.idempotency_key.as_deref() {
        if let Some(existing) = load_by_idempotency(
            transaction,
            &input.tenant_id,
            &input.principal_id,
            input.phase,
            idempotency_key,
        )? {
            ensure_idempotent_match(&existing, input, request_hash, payload_json)?;
            return Ok(existing);
        }
    }

    let (sequence, previous_hash) = load_next_head(transaction, &input.tenant_id)?;
    let event_id = Uuid::new_v4().to_string();
    let created_at_ms = unix_timestamp_ms()?;
    let event_hash = compute_event_hash(
        &input.tenant_id,
        sequence,
        &event_id,
        &input.principal_id,
        &input.action,
        input.phase,
        request_hash,
        payload_json,
        &previous_hash,
        created_at_ms,
        input.idempotency_key.as_deref(),
    );

    transaction.execute(
        "INSERT INTO audit_events (
            tenant_id, sequence, event_id, principal_id, action, phase, request_hash,
            payload_json, previous_hash, event_hash, created_at_ms, idempotency_key
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            input.tenant_id,
            sequence,
            event_id,
            input.principal_id,
            input.action,
            phase_name(input.phase),
            request_hash,
            payload_json,
            previous_hash,
            event_hash,
            created_at_ms,
            input.idempotency_key,
        ],
    )?;
    load_record(transaction, &input.tenant_id, sequence)?
        .ok_or_else(|| AuditError::InvalidInput("appended row could not be loaded".into()))
}

/// Loads the next sequence and previous hash while an immediate transaction holds the writer lock.
fn load_next_head(
    transaction: &Transaction<'_>,
    tenant_id: &str,
) -> Result<(u64, String), AuditError> {
    let head = transaction
        .query_row(
            "SELECT sequence, event_hash FROM audit_events
             WHERE tenant_id = ?1 ORDER BY sequence DESC LIMIT 1",
            [tenant_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(match head {
        Some((sequence, event_hash)) => (sequence + 1, event_hash),
        None => (1, GENESIS_HASH.to_owned()),
    })
}

/// Loads one audit record through either a connection or transaction.
fn load_record(
    connection: &Connection,
    tenant_id: &str,
    sequence: u64,
) -> Result<Option<AuditRecord>, AuditError> {
    connection
        .query_row(
            &format!("{SELECT_RECORD} WHERE tenant_id = ?1 AND sequence = ?2"),
            params![tenant_id, sequence],
            record_from_row,
        )
        .optional()
        .map_err(AuditError::from)
}

/// Loads an audit record by retry key.
fn load_by_idempotency(
    transaction: &Transaction<'_>,
    tenant_id: &str,
    principal_id: &str,
    phase: AuditPhase,
    idempotency_key: &str,
) -> Result<Option<AuditRecord>, AuditError> {
    transaction
        .query_row(
            &format!(
                "{SELECT_RECORD} WHERE tenant_id = ?1 AND principal_id = ?2 AND phase = ?3
                    AND idempotency_key = ?4"
            ),
            params![tenant_id, principal_id, phase_name(phase), idempotency_key],
            record_from_row,
        )
        .optional()
        .map_err(AuditError::from)
}

/// Loads an execution ledger row through either a connection or transaction.
fn load_execution_record(
    connection: &Connection,
    tenant_id: &str,
    principal_id: &str,
    idempotency_key: &str,
) -> Result<Option<ExecutionRecord>, AuditError> {
    connection
        .query_row(
            &format!(
                "{SELECT_EXECUTION_RECORD} WHERE tenant_id = ?1 AND principal_id = ?2 AND idempotency_key = ?3"
            ),
            params![tenant_id, principal_id, idempotency_key],
            execution_record_from_row,
        )
        .optional()
        .map_err(AuditError::from)
}

/// Loads persistent stream state through either a connection or transaction.
fn load_stream_state(
    connection: &Connection,
    tenant_id: &str,
) -> Result<Option<AuditStreamState>, AuditError> {
    connection
        .query_row(
            "SELECT tenant_id, blocked, reason_code, updated_at_ms
             FROM audit_stream_state WHERE tenant_id = ?1",
            [tenant_id],
            |row| {
                Ok(AuditStreamState {
                    tenant_id: row.get(0)?,
                    blocked: row.get::<_, i64>(1)? != 0,
                    reason_code: row.get(2)?,
                    updated_at_ms: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(AuditError::from)
}

/// Enforces persistent stream blocking inside the caller's transaction boundary.
fn ensure_stream_writable_in(connection: &Connection, tenant_id: &str) -> Result<(), AuditError> {
    if let Some(state) = load_stream_state(connection, tenant_id)? {
        if state.blocked {
            return Err(AuditError::StreamBlocked {
                reason_code: state
                    .reason_code
                    .unwrap_or_else(|| "ambiguous_completion".to_string()),
            });
        }
    }
    Ok(())
}

/// Decodes one SQLite row into its immutable audit representation.
fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRecord> {
    let phase: String = row.get(5)?;
    let payload_json: String = row.get(7)?;
    let witness_receipt_json: Option<String> = row.get(12)?;
    Ok(AuditRecord {
        tenant_id: row.get(0)?,
        sequence: row.get(1)?,
        event_id: row.get(2)?,
        principal_id: row.get(3)?,
        action: row.get(4)?,
        phase: parse_phase(&phase).map_err(to_sql_conversion_error)?,
        request_hash: row.get(6)?,
        payload: serde_json::from_str(&payload_json).map_err(to_sql_conversion_error)?,
        previous_hash: row.get(8)?,
        event_hash: row.get(9)?,
        created_at_ms: row.get(10)?,
        idempotency_key: row.get(11)?,
        witness_receipt: witness_receipt_json
            .map(|json| serde_json::from_str(&json).map_err(to_sql_conversion_error))
            .transpose()?,
    })
}

/// Decodes one SQLite row into its execution ledger representation.
fn execution_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExecutionRecord> {
    let state: String = row.get(4)?;
    let sanitized_result_json: Option<String> = row.get(5)?;
    Ok(ExecutionRecord {
        tenant_id: row.get(0)?,
        principal_id: row.get(1)?,
        idempotency_key: row.get(2)?,
        request_hash: row.get(3)?,
        state: parse_execution_state(&state).map_err(to_sql_conversion_error)?,
        sanitized_result: sanitized_result_json
            .map(|json| serde_json::from_str(&json).map_err(to_sql_conversion_error))
            .transpose()?,
        intent_sequence: row.get(6)?,
        claimed_at_ms: row.get(7)?,
        updated_at_ms: row.get(8)?,
    })
}

/// Adapts a decoding failure to rusqlite's row-conversion error.
fn to_sql_conversion_error(
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

/// Checks whether an idempotent retry is byte-for-byte equivalent to the original request.
fn ensure_idempotent_match(
    existing: &AuditRecord,
    input: &AuditEventInput,
    request_hash: &str,
    payload_json: &str,
) -> Result<(), AuditError> {
    let existing_payload = canonical_json_string(&existing.payload)?;
    if existing.principal_id == input.principal_id
        && existing.action == input.action
        && existing.phase == input.phase
        && existing.request_hash == request_hash
        && existing_payload == payload_json
    {
        Ok(())
    } else {
        Err(AuditError::IdempotencyConflict)
    }
}

/// Rejects reuse of an execution key with a different canonical request hash.
fn ensure_execution_hash(existing: &ExecutionRecord, request_hash: &str) -> Result<(), AuditError> {
    if existing.request_hash == request_hash {
        Ok(())
    } else {
        Err(AuditError::IdempotencyConflict)
    }
}

/// Validates that an execution transition carries the expected audit phase and retry key.
fn validate_execution_event(
    input: &AuditEventInput,
    expected_phase: AuditPhase,
) -> Result<(), AuditError> {
    validate_input(input)?;
    if input.phase != expected_phase {
        return Err(AuditError::InvalidInput(
            "execution event has the wrong audit phase".to_string(),
        ));
    }
    if input.idempotency_key.is_none() {
        return Err(AuditError::InvalidInput(
            "execution event requires an idempotency key".to_string(),
        ));
    }
    Ok(())
}

/// Validates the required audit metadata and digest fields.
fn validate_input(input: &AuditEventInput) -> Result<(), AuditError> {
    validate_identifier("tenant id", &input.tenant_id)?;
    validate_identifier("principal id", &input.principal_id)?;
    validate_identifier("action", &input.action)?;
    if let Some(key) = input.idempotency_key.as_deref() {
        validate_identifier("idempotency key", key)?;
    }
    Ok(())
}

/// Validates a bounded non-empty identifier.
fn validate_identifier(label: &str, value: &str) -> Result<(), AuditError> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(AuditError::InvalidInput(format!("{label} is invalid")));
    }
    Ok(())
}

/// Accepts only the fixed metadata schema that is safe to persist in plaintext.
fn validate_metadata_payload(value: &Value) -> Result<(), AuditError> {
    let Value::Object(entries) = value else {
        return Err(AuditError::InvalidInput(
            "audit payload must be a metadata object".to_string(),
        ));
    };
    if entries.len() > 24 {
        return Err(AuditError::InvalidInput(
            "audit payload has too many metadata fields".to_string(),
        ));
    }
    for (key, child) in entries {
        if !allowed_metadata_key(key) {
            return Err(AuditError::InvalidInput(format!(
                "audit metadata field is not allowed: {key}"
            )));
        }
        validate_metadata_value(key, child)?;
    }
    Ok(())
}

/// Identifies fields in the intentionally narrow persistent audit schema.
fn allowed_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "tool"
            | "operation"
            | "decision"
            | "outcome"
            | "gate"
            | "gates"
            | "approval_id"
            | "token_identity"
            | "reason_code"
            | "component_id"
            | "policy_version"
            | "witness_mode"
    )
}

/// Validates one scalar or bounded string-list metadata value.
fn validate_metadata_value(key: &str, value: &Value) -> Result<(), AuditError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        Value::String(value) if value.len() <= 512 && !value.chars().any(char::is_control) => {
            Ok(())
        }
        Value::Array(values)
            if key == "gates"
                && values.len() <= 16
                && values.iter().all(|entry| {
                    matches!(
                        entry,
                        Value::String(value)
                            if value.len() <= 128 && !value.chars().any(char::is_control)
                    )
                }) =>
        {
            Ok(())
        }
        _ => Err(AuditError::InvalidInput(format!(
            "audit metadata value is invalid: {key}"
        ))),
    }
}

/// Produces a deterministic JSON string by recursively ordering object keys.
fn canonical_json_string(value: &Value) -> Result<String, AuditError> {
    serde_json::to_string(&canonicalize(value)).map_err(AuditError::from)
}

/// Recursively reconstructs JSON objects with lexicographically ordered keys.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(entries) => {
            let ordered = entries
                .iter()
                .map(|(key, child)| (key.clone(), canonicalize(child)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(ordered.into_iter().collect())
        }
        Value::Array(entries) => Value::Array(entries.iter().map(canonicalize).collect()),
        scalar => scalar.clone(),
    }
}

/// Computes the immutable hash for one audit record.
#[allow(clippy::too_many_arguments)]
fn compute_event_hash(
    tenant_id: &str,
    sequence: u64,
    event_id: &str,
    principal_id: &str,
    action: &str,
    phase: AuditPhase,
    request_hash: &str,
    payload_json: &str,
    previous_hash: &str,
    created_at_ms: i64,
    idempotency_key: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(AUDIT_HASH_DOMAIN);
    hash_field(&mut hasher, tenant_id.as_bytes());
    hash_field(&mut hasher, &sequence.to_be_bytes());
    hash_field(&mut hasher, event_id.as_bytes());
    hash_field(&mut hasher, principal_id.as_bytes());
    hash_field(&mut hasher, action.as_bytes());
    hash_field(&mut hasher, phase_name(phase).as_bytes());
    hash_field(&mut hasher, request_hash.as_bytes());
    hash_field(&mut hasher, payload_json.as_bytes());
    hash_field(&mut hasher, previous_hash.as_bytes());
    hash_field(&mut hasher, &created_at_ms.to_be_bytes());
    hash_field(&mut hasher, idempotency_key.unwrap_or_default().as_bytes());
    hex_digest(hasher.finalize())
}

/// Produces the domain-separated bytes signed by the Henosis origin key.
fn checkpoint_signing_bytes(checkpoint: &WitnessCheckpoint) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(CHECKPOINT_SIGNATURE_DOMAIN);
    append_field(&mut bytes, checkpoint.tenant_id.as_bytes());
    append_field(&mut bytes, &checkpoint.sequence.to_be_bytes());
    append_field(&mut bytes, checkpoint.event_id.as_bytes());
    append_field(&mut bytes, checkpoint.previous_hash.as_bytes());
    append_field(&mut bytes, checkpoint.event_hash.as_bytes());
    append_field(&mut bytes, checkpoint.origin_key_id.as_bytes());
    bytes
}

/// Produces the domain-separated bytes signed by the witness key.
fn receipt_signing_bytes(receipt: &WitnessReceipt) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(192);
    bytes.extend_from_slice(RECEIPT_SIGNATURE_DOMAIN);
    append_field(&mut bytes, receipt.tenant_id.as_bytes());
    append_field(&mut bytes, &receipt.sequence.to_be_bytes());
    append_field(&mut bytes, receipt.event_hash.as_bytes());
    append_field(&mut bytes, receipt.witness_key_id.as_bytes());
    append_field(&mut bytes, &receipt.witnessed_at_ms.to_be_bytes());
    bytes
}

/// Adds a length-prefixed field to signature material.
fn append_field(output: &mut Vec<u8>, field: &[u8]) {
    output.extend_from_slice(&(field.len() as u64).to_be_bytes());
    output.extend_from_slice(field);
}

/// Adds a length-prefixed field to a streaming digest.
fn hash_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_be_bytes());
    hasher.update(field);
}

/// Encodes digest bytes as lowercase hexadecimal without another dependency.
fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// Maps an audit phase to its stable database representation.
fn phase_name(phase: AuditPhase) -> &'static str {
    match phase {
        AuditPhase::Intent => "intent",
        AuditPhase::Outcome => "outcome",
    }
}

/// Parses the stable database representation of an audit phase.
fn parse_phase(value: &str) -> Result<AuditPhase, AuditError> {
    match value {
        "intent" => Ok(AuditPhase::Intent),
        "outcome" => Ok(AuditPhase::Outcome),
        _ => Err(AuditError::InvalidInput(
            "stored audit phase is invalid".into(),
        )),
    }
}

/// Parses the stable database representation of an execution lifecycle.
fn parse_execution_state(value: &str) -> Result<ExecutionState, AuditError> {
    match value {
        "claimed" => Ok(ExecutionState::Claimed),
        "completed" => Ok(ExecutionState::Completed),
        "indeterminate" => Ok(ExecutionState::Indeterminate),
        _ => Err(AuditError::InvalidInput(
            "stored execution state is invalid".into(),
        )),
    }
}

/// Builds a chain verification failure without exposing event payloads.
fn verification_error(sequence: u64, reason: impl Into<String>) -> AuditError {
    AuditError::ChainVerification {
        sequence,
        reason: reason.into(),
    }
}

#[cfg(test)]
/// Exercises chain integrity, idempotency, blocking, and witness behavior.
mod tests {
    use super::*;
    use serde_json::json;

    /// Constructs a valid test input with deterministic request content.
    fn input(phase: AuditPhase, idempotency_key: &str) -> AuditEventInput {
        AuditEventInput {
            tenant_id: "tenant-a".into(),
            principal_id: "machine:test".into(),
            action: "tool.invoke".into(),
            phase,
            request: json!({"action": "ping"}),
            payload: json!({"tool": "demo", "outcome": "allowed"}),
            idempotency_key: Some(idempotency_key.into()),
        }
    }

    /// Constructs a valid test input for one authenticated principal.
    fn principal_input(
        phase: AuditPhase,
        idempotency_key: &str,
        principal_id: &str,
    ) -> AuditEventInput {
        let mut event = input(phase, idempotency_key);
        event.principal_id = principal_id.to_string();
        event
    }

    /// Proves that append builds a tenant-local verifiable chain.
    #[test]
    fn append_and_verify_chain() {
        let store = AuditStore::open_in_memory().unwrap();
        let first = store
            .append(input(AuditPhase::Intent, "request-1:intent"))
            .unwrap();
        let second = store
            .append(input(AuditPhase::Outcome, "request-1:outcome"))
            .unwrap();

        assert_eq!(first.previous_hash, GENESIS_HASH);
        assert_eq!(second.previous_hash, first.event_hash);
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 2);
    }

    /// Proves that identical retries return the original row without extending the chain.
    #[test]
    fn append_is_idempotent_for_identical_content() {
        let store = AuditStore::open_in_memory().unwrap();
        let first = store.append(input(AuditPhase::Intent, "same")).unwrap();
        let retried = store.append(input(AuditPhase::Intent, "same")).unwrap();

        assert_eq!(first, retried);
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 1);
    }

    /// Proves that retry keys cannot be reused for different governed content.
    #[test]
    fn append_rejects_idempotency_conflict() {
        let store = AuditStore::open_in_memory().unwrap();
        store.append(input(AuditPhase::Intent, "same")).unwrap();
        let mut changed = input(AuditPhase::Intent, "same");
        changed.action = "tool.delete".into();

        assert!(matches!(
            store.append(changed),
            Err(AuditError::IdempotencyConflict)
        ));
    }

    /// Proves claim and completion transitions are durable, atomic, and safely replayable.
    #[test]
    fn execution_ledger_claims_completes_and_replays() {
        let store = AuditStore::open_in_memory().unwrap();
        let key = "execution-1";
        let acquired = store
            .claim_execution(input(AuditPhase::Intent, key))
            .unwrap();
        let ExecutionClaim::Acquired(claimed) = acquired else {
            panic!("first claim must be acquired");
        };
        assert_eq!(claimed.state, ExecutionState::Claimed);
        assert_eq!(claimed.principal_id, "machine:test");
        assert_eq!(claimed.intent_sequence, 1);
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 1);

        let retried = store
            .claim_execution(input(AuditPhase::Intent, key))
            .unwrap();
        assert_eq!(retried, ExecutionClaim::Existing(claimed.clone()));
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 1);

        let sanitized_result = json!({"safe": true});
        let completed = store
            .complete_execution(input(AuditPhase::Outcome, key), sanitized_result.clone())
            .unwrap();
        assert_eq!(completed.state, ExecutionState::Completed);
        assert_eq!(completed.sanitized_result.as_ref(), Some(&sanitized_result));
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 2);
        assert_eq!(
            store
                .complete_execution(input(AuditPhase::Outcome, key), sanitized_result)
                .unwrap(),
            completed
        );
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 2);

        let replay = store
            .claim_execution(input(AuditPhase::Intent, key))
            .unwrap();
        assert_eq!(replay, ExecutionClaim::Existing(completed.clone()));
        assert_eq!(
            store
                .execution_record("tenant-a", "machine:test", key)
                .unwrap(),
            Some(completed)
        );
    }

    /// Proves two principals in one tenant can independently claim the same caller retry key.
    #[test]
    fn execution_ledger_namespaces_same_key_by_principal() {
        let store = AuditStore::open_in_memory().unwrap();
        let key = "shared-caller-key";
        let first = store
            .claim_execution(principal_input(AuditPhase::Intent, key, "machine:first"))
            .unwrap();
        let second = store
            .claim_execution(principal_input(AuditPhase::Intent, key, "machine:second"))
            .unwrap();
        let ExecutionClaim::Acquired(first) = first else {
            panic!("first principal must acquire an independent claim");
        };
        let ExecutionClaim::Acquired(second) = second else {
            panic!("second principal must acquire an independent claim");
        };

        assert_eq!(first.principal_id, "machine:first");
        assert_eq!(second.principal_id, "machine:second");
        assert_ne!(first.intent_sequence, second.intent_sequence);
        assert_eq!(
            store
                .claim_execution(principal_input(AuditPhase::Intent, key, "machine:first"))
                .unwrap(),
            ExecutionClaim::Existing(first.clone())
        );

        let completed = store
            .complete_execution(
                principal_input(AuditPhase::Outcome, key, "machine:first"),
                json!({"owner": "first"}),
            )
            .unwrap();
        assert_eq!(completed.state, ExecutionState::Completed);
        assert_eq!(
            store
                .execution_record("tenant-a", "machine:second", key)
                .unwrap(),
            Some(second)
        );
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 3);
    }

    /// Proves a legacy tenant-and-key ledger is migrated without losing its principal binding.
    #[test]
    fn execution_ledger_migrates_legacy_principal_namespace() {
        let database = tempfile::NamedTempFile::new().unwrap();
        let key = "legacy-key";
        {
            let store = AuditStore::open(database.path()).unwrap();
            store
                .claim_execution(principal_input(AuditPhase::Intent, key, "machine:legacy"))
                .unwrap();
            let connection = store.lock_connection().unwrap();
            connection
                .execute_batch(
                    "
                    DROP INDEX audit_events_principal_idempotency;
                    CREATE UNIQUE INDEX audit_events_idempotency
                        ON audit_events (tenant_id, phase, idempotency_key)
                        WHERE idempotency_key IS NOT NULL;
                    ALTER TABLE execution_records RENAME TO execution_records_principal;
                    CREATE TABLE execution_records (
                        tenant_id TEXT NOT NULL,
                        idempotency_key TEXT NOT NULL,
                        request_hash TEXT NOT NULL,
                        state TEXT NOT NULL
                            CHECK (state IN ('claimed', 'completed', 'indeterminate')),
                        sanitized_result_json TEXT,
                        intent_sequence INTEGER NOT NULL,
                        claimed_at_ms INTEGER NOT NULL,
                        updated_at_ms INTEGER NOT NULL,
                        PRIMARY KEY (tenant_id, idempotency_key),
                        FOREIGN KEY (tenant_id, intent_sequence)
                            REFERENCES audit_events (tenant_id, sequence),
                        CHECK (
                            (state = 'completed' AND sanitized_result_json IS NOT NULL)
                            OR (state != 'completed' AND sanitized_result_json IS NULL)
                        )
                    );
                    INSERT INTO execution_records (
                        tenant_id, idempotency_key, request_hash, state,
                        sanitized_result_json, intent_sequence, claimed_at_ms, updated_at_ms
                    )
                    SELECT tenant_id, idempotency_key, request_hash, state,
                           sanitized_result_json, intent_sequence, claimed_at_ms, updated_at_ms
                    FROM execution_records_principal;
                    DROP TABLE execution_records_principal;
                    ",
                )
                .unwrap();
        }

        let migrated = AuditStore::open(database.path()).unwrap();
        let legacy = migrated
            .execution_record("tenant-a", "machine:legacy", key)
            .unwrap()
            .unwrap();
        assert_eq!(legacy.principal_id, "machine:legacy");
        assert_eq!(legacy.state, ExecutionState::Claimed);
        assert!(matches!(
            migrated
                .claim_execution(principal_input(AuditPhase::Intent, key, "machine:new"))
                .unwrap(),
            ExecutionClaim::Acquired(_)
        ));
        assert_eq!(migrated.verify_tenant("tenant-a").unwrap(), 2);
    }

    /// Proves a tenant retry key cannot be rebound to changed canonical request content.
    #[test]
    fn execution_ledger_rejects_changed_request_hash() {
        let store = AuditStore::open_in_memory().unwrap();
        let key = "execution-conflict";
        store
            .claim_execution(input(AuditPhase::Intent, key))
            .unwrap();
        let mut changed = input(AuditPhase::Intent, key);
        changed.request = json!({"action": "different"});

        assert!(matches!(
            store.claim_execution(changed),
            Err(AuditError::IdempotencyConflict)
        ));
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 1);
    }

    /// Proves an indeterminate execution remains non-replayable and cannot become completed.
    #[test]
    fn execution_ledger_preserves_indeterminate_state() {
        let store = AuditStore::open_in_memory().unwrap();
        let key = "execution-indeterminate";
        let request = input(AuditPhase::Intent, key).request;
        store
            .claim_execution(input(AuditPhase::Intent, key))
            .unwrap();

        let indeterminate = store
            .mark_execution_indeterminate("tenant-a", "machine:test", key, &request)
            .unwrap();
        assert_eq!(indeterminate.state, ExecutionState::Indeterminate);
        assert!(indeterminate.sanitized_result.is_none());
        assert_eq!(
            store
                .mark_execution_indeterminate("tenant-a", "machine:test", key, &request)
                .unwrap(),
            indeterminate
        );
        assert_eq!(
            store
                .claim_execution(input(AuditPhase::Intent, key))
                .unwrap(),
            ExecutionClaim::Existing(indeterminate)
        );
        assert!(matches!(
            store.complete_execution(input(AuditPhase::Outcome, key), json!({"safe": true})),
            Err(AuditError::ExecutionStateConflict)
        ));
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 1);
    }

    /// Proves that credential-shaped fields are rejected before persistence.
    #[test]
    fn append_rejects_sensitive_payload_fields() {
        let store = AuditStore::open_in_memory().unwrap();
        let mut event = input(AuditPhase::Intent, "secret");
        event.payload = json!({"apiKey": "do-not-store"});

        assert!(matches!(
            store.append(event),
            Err(AuditError::InvalidInput(_))
        ));
        assert!(store.head("tenant-a").unwrap().is_none());
    }

    /// Proves common credential-key spelling bypasses are outside the metadata allowlist.
    #[test]
    fn append_rejects_credential_key_spelling_bypasses() {
        for (index, key) in [
            "apiKey",
            "privateKey",
            "authorizationHeader",
            "clientSecret",
        ]
        .into_iter()
        .enumerate()
        {
            let store = AuditStore::open_in_memory().unwrap();
            let mut event = input(AuditPhase::Intent, &format!("bypass-{index}"));
            event.payload = json!({key: "do-not-store"});
            assert!(matches!(
                store.append(event),
                Err(AuditError::InvalidInput(_))
            ));
        }
    }

    /// Proves an ambiguous completion persistently blocks later audit appends and outcomes.
    #[test]
    fn ambiguous_completion_blocks_stream() {
        let store = AuditStore::open_in_memory().unwrap();
        let key = "blocked-completion";
        let claim = store
            .claim_execution(input(AuditPhase::Intent, key))
            .unwrap();
        assert!(matches!(claim, ExecutionClaim::Acquired(_)));
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 1);

        store
            .mark_stream_ambiguous("tenant-a", "outcome_witness_failed")
            .unwrap();

        let state = store.stream_state("tenant-a").unwrap().unwrap();
        assert!(state.blocked);
        assert_eq!(state.reason_code.as_deref(), Some("outcome_witness_failed"));
        assert!(matches!(
            store.append(input(AuditPhase::Intent, "blocked")),
            Err(AuditError::StreamBlocked { .. })
        ));
        assert!(matches!(
            store.complete_execution(input(AuditPhase::Outcome, key), json!({"safe": true})),
            Err(AuditError::StreamBlocked { .. })
        ));

        let record = store
            .execution_record("tenant-a", "machine:test", key)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, ExecutionState::Claimed);
        assert!(record.sanitized_result.is_none());
        assert_eq!(store.verify_tenant("tenant-a").unwrap(), 1);
    }

    /// Proves production witness clients reject plaintext and credential-bearing URLs.
    #[test]
    fn witness_client_requires_authenticated_https_url() {
        let key = SigningKey::from_bytes(&[11_u8; 32]).verifying_key();
        assert!(matches!(
            WitnessClient::new(
                "http://witness.example",
                "witness-a",
                key,
                Duration::from_secs(1)
            ),
            Err(AuditError::WitnessUrl)
        ));
        assert!(matches!(
            WitnessClient::new(
                "https://user:pass@witness.example",
                "witness-a",
                key,
                Duration::from_secs(1)
            ),
            Err(AuditError::WitnessUrl)
        ));
    }

    /// Proves that a receipt is accepted only when its signature and row identity match.
    #[test]
    fn witness_receipt_round_trip() {
        let store = AuditStore::open_in_memory().unwrap();
        let record = store
            .append(input(AuditPhase::Intent, "witnessed"))
            .unwrap();
        let witness_signer = SigningKey::from_bytes(&[7_u8; 32]);
        let receipt = sign_witness_receipt(
            record.tenant_id.clone(),
            record.sequence,
            record.event_hash.clone(),
            "witness-test".into(),
            42,
            &witness_signer,
        );

        store
            .attach_witness_receipt(&receipt, &witness_signer.verifying_key())
            .unwrap();
        assert_eq!(
            store.get("tenant-a", 1).unwrap().unwrap().witness_receipt,
            Some(receipt)
        );
    }

    /// Proves that origin checkpoints cannot be altered after signing.
    #[test]
    fn checkpoint_signature_binds_stream_head() {
        let store = AuditStore::open_in_memory().unwrap();
        let record = store
            .append(input(AuditPhase::Intent, "checkpoint"))
            .unwrap();
        let signer = OriginSigner::new("origin-test", SigningKey::from_bytes(&[9_u8; 32])).unwrap();
        let mut checkpoint = signer.checkpoint(&record);

        verify_checkpoint_signature(&checkpoint, &signer.verifying_key()).unwrap();
        checkpoint.event_hash = "f".repeat(64);
        assert!(matches!(
            verify_checkpoint_signature(&checkpoint, &signer.verifying_key()),
            Err(AuditError::InvalidSignature)
        ));
    }

    /// Proves that direct database tampering is detected by full-stream verification.
    #[test]
    fn verification_detects_tampering() {
        let store = AuditStore::open_in_memory().unwrap();
        store.append(input(AuditPhase::Intent, "tamper")).unwrap();
        {
            let connection = store.lock_connection().unwrap();
            connection
                .execute(
                    "UPDATE audit_events SET payload_json = '{\"outcome\":\"changed\"}'",
                    [],
                )
                .unwrap();
        }

        assert!(matches!(
            store.verify_tenant("tenant-a"),
            Err(AuditError::ChainVerification { sequence: 1, .. })
        ));
    }
}
