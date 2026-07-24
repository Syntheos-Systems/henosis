#![deny(missing_docs)]
//! Independent persistence and HTTP handling for Henosis audit checkpoints.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use henosis_audit::{
    sign_witness_receipt, unix_timestamp_ms, verify_checkpoint_signature, WitnessCheckpoint,
    WitnessReceipt, GENESIS_HASH,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

/// A failure while validating or preserving a checkpoint.
#[derive(Debug, thiserror::Error)]
pub enum WitnessError {
    /// SQLite rejected an independent witness operation.
    #[error("witness storage error: {0}")]
    Storage(#[from] rusqlite::Error),
    /// JSON serialization or decoding failed.
    #[error("witness serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// The origin key identifier is unknown.
    #[error("origin key is not trusted")]
    UnknownOrigin,
    /// The trusted origin is not authorized to witness the submitted tenant.
    #[error("origin key is not authorized for tenant")]
    TenantNotAuthorized,
    /// An origin trust entry has an empty key identifier or tenant allowlist.
    #[error("origin trust configuration is invalid")]
    InvalidOriginTrust,
    /// The checkpoint signature is invalid.
    #[error("checkpoint signature is invalid")]
    InvalidSignature,
    /// The submitted checkpoint conflicts with an existing stream position.
    #[error("checkpoint conflicts with witnessed stream")]
    Conflict,
    /// The checkpoint does not extend the current stream head.
    #[error("checkpoint is out of sequence")]
    OutOfSequence,
    /// The witness state mutex was poisoned.
    #[error("witness storage lock was poisoned")]
    LockPoisoned,
    /// The system clock cannot provide a supported timestamp.
    #[error("witness clock is invalid")]
    Clock,
}

/// One immutable origin verification key bound to exact authorized tenant identifiers.
#[derive(Clone, Debug)]
pub struct TrustedOrigin {
    verifying_key: VerifyingKey,
    tenant_ids: BTreeSet<String>,
}

/// Constructs and queries a fail-closed origin trust entry.
impl TrustedOrigin {
    /// Creates an origin trust entry with a non-empty exact tenant allowlist.
    pub fn new(
        verifying_key: VerifyingKey,
        tenant_ids: impl IntoIterator<Item = String>,
    ) -> Result<Self, WitnessError> {
        let tenant_ids = tenant_ids.into_iter().collect::<Vec<_>>();
        let configured_tenant_count = tenant_ids.len();
        let tenant_ids = tenant_ids.into_iter().collect::<BTreeSet<_>>();
        if tenant_ids.is_empty()
            || tenant_ids.len() != configured_tenant_count
            || tenant_ids.iter().any(|tenant_id| {
                tenant_id.is_empty() || tenant_id == "*" || tenant_id.trim() != tenant_id
            })
        {
            return Err(WitnessError::InvalidOriginTrust);
        }
        Ok(Self {
            verifying_key,
            tenant_ids,
        })
    }

    /// Returns whether this key is explicitly authorized for the exact tenant identifier.
    fn authorizes(&self, tenant_id: &str) -> bool {
        self.tenant_ids.contains(tenant_id)
    }
}

/// Current independently preserved head for one tenant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WitnessHead {
    /// Tenant whose head is represented.
    pub tenant_id: String,
    /// Monotonic sequence at the preserved head.
    pub sequence: u64,
    /// Event hash at the preserved head.
    pub event_hash: String,
    /// Time the head was last advanced.
    pub witnessed_at_ms: i64,
}

/// Durable witness database with an independent Ed25519 signing identity.
#[derive(Clone)]
pub struct WitnessStore {
    connection: Arc<Mutex<Connection>>,
    trusted_origins: Arc<BTreeMap<String, TrustedOrigin>>,
    witness_key_id: Arc<str>,
    signing_key: Arc<SigningKey>,
}

/// Implements checkpoint validation, idempotency, and monotonic stream preservation.
impl WitnessStore {
    /// Opens or creates the independent witness database.
    pub fn open(
        path: impl AsRef<std::path::Path>,
        trusted_origins: BTreeMap<String, TrustedOrigin>,
        witness_key_id: impl Into<String>,
        signing_key: SigningKey,
    ) -> Result<Self, WitnessError> {
        Self::from_connection(
            Connection::open(path)?,
            trusted_origins,
            witness_key_id.into(),
            signing_key,
        )
    }

    /// Creates an isolated in-memory witness database.
    pub fn open_in_memory(
        trusted_origins: BTreeMap<String, TrustedOrigin>,
        witness_key_id: impl Into<String>,
        signing_key: SigningKey,
    ) -> Result<Self, WitnessError> {
        Self::from_connection(
            Connection::open_in_memory()?,
            trusted_origins,
            witness_key_id.into(),
            signing_key,
        )
    }

    /// Initializes the schema around an already-open connection.
    fn from_connection(
        connection: Connection,
        trusted_origins: BTreeMap<String, TrustedOrigin>,
        witness_key_id: String,
        signing_key: SigningKey,
    ) -> Result<Self, WitnessError> {
        if trusted_origins.is_empty()
            || trusted_origins
                .keys()
                .any(|key_id| key_id.is_empty() || key_id.trim() != key_id)
            || witness_key_id.is_empty()
            || witness_key_id.trim() != witness_key_id
        {
            return Err(WitnessError::InvalidOriginTrust);
        }
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS witness_heads (
                tenant_id TEXT PRIMARY KEY,
                sequence INTEGER NOT NULL,
                event_hash TEXT NOT NULL,
                witnessed_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS witness_receipts (
                tenant_id TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                event_hash TEXT NOT NULL,
                receipt_json TEXT NOT NULL,
                PRIMARY KEY (tenant_id, sequence)
            );
            ",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            trusted_origins: Arc::new(trusted_origins),
            witness_key_id: witness_key_id.into(),
            signing_key: Arc::new(signing_key),
        })
    }

    /// Verifies and preserves a checkpoint, returning the original receipt on exact retry.
    pub fn accept(&self, checkpoint: &WitnessCheckpoint) -> Result<WitnessReceipt, WitnessError> {
        let origin = self
            .trusted_origins
            .get(&checkpoint.origin_key_id)
            .ok_or(WitnessError::UnknownOrigin)?;
        if !origin.authorizes(&checkpoint.tenant_id) {
            return Err(WitnessError::TenantNotAuthorized);
        }
        verify_checkpoint_signature(checkpoint, &origin.verifying_key)
            .map_err(|_| WitnessError::InvalidSignature)?;

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing = transaction
            .query_row(
                "SELECT event_hash, receipt_json FROM witness_receipts
                 WHERE tenant_id = ?1 AND sequence = ?2",
                params![checkpoint.tenant_id, checkpoint.sequence],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((event_hash, receipt_json)) = existing {
            if event_hash != checkpoint.event_hash {
                return Err(WitnessError::Conflict);
            }
            let receipt = serde_json::from_str(&receipt_json)?;
            transaction.commit()?;
            return Ok(receipt);
        }

        let head = load_head_from_connection(&transaction, &checkpoint.tenant_id)?;
        let (expected_sequence, expected_previous) = match head {
            Some(head) => (head.sequence + 1, head.event_hash),
            None => (1, GENESIS_HASH.to_owned()),
        };
        if checkpoint.sequence != expected_sequence || checkpoint.previous_hash != expected_previous
        {
            return Err(WitnessError::OutOfSequence);
        }

        let witnessed_at_ms = unix_timestamp_ms().map_err(|_| WitnessError::Clock)?;
        let receipt = sign_witness_receipt(
            checkpoint.tenant_id.clone(),
            checkpoint.sequence,
            checkpoint.event_hash.clone(),
            self.witness_key_id.to_string(),
            witnessed_at_ms,
            &self.signing_key,
        );
        let receipt_json = serde_json::to_string(&receipt)?;
        transaction.execute(
            "INSERT INTO witness_receipts
                (tenant_id, sequence, event_hash, receipt_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                checkpoint.tenant_id,
                checkpoint.sequence,
                checkpoint.event_hash,
                receipt_json
            ],
        )?;
        transaction.execute(
            "INSERT INTO witness_heads (tenant_id, sequence, event_hash, witnessed_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(tenant_id) DO UPDATE SET
                sequence = excluded.sequence,
                event_hash = excluded.event_hash,
                witnessed_at_ms = excluded.witnessed_at_ms",
            params![
                checkpoint.tenant_id,
                checkpoint.sequence,
                checkpoint.event_hash,
                witnessed_at_ms
            ],
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    /// Loads the preserved head for one tenant.
    pub fn head(&self, tenant_id: &str) -> Result<Option<WitnessHead>, WitnessError> {
        let connection = self.lock_connection()?;
        load_head_from_connection(&connection, tenant_id)
    }

    /// Acquires the witness connection without panicking on poison.
    fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>, WitnessError> {
        self.connection
            .lock()
            .map_err(|_| WitnessError::LockPoisoned)
    }
}

/// Shared HTTP state for the witness service.
#[derive(Clone)]
struct AppState {
    store: WitnessStore,
}

/// Constructs the witness HTTP API.
pub fn router(store: WitnessStore) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/checkpoints", post(checkpoint))
        .with_state(AppState { store })
}

/// Reports witness liveness without exposing key or stream data.
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

/// Verifies and preserves one submitted audit checkpoint.
async fn checkpoint(
    State(state): State<AppState>,
    Json(checkpoint): Json<WitnessCheckpoint>,
) -> Result<(StatusCode, Json<WitnessReceipt>), ApiError> {
    let receipt = state.store.accept(&checkpoint)?;
    Ok((StatusCode::OK, Json(receipt)))
}

/// Sanitized HTTP wrapper around internal witness failures.
struct ApiError(WitnessError);

/// Wraps internal witness failures for centralized public response sanitization.
impl From<WitnessError> for ApiError {
    /// Preserve the internal error until [`IntoResponse`] maps it to a stable public code.
    fn from(error: WitnessError) -> Self {
        Self(error)
    }
}

/// Converts internal failures to stable, non-sensitive HTTP errors.
impl IntoResponse for ApiError {
    /// Map one internal failure to a stable status and public error code.
    fn into_response(self) -> Response {
        let (status, code) = match self.0 {
            WitnessError::UnknownOrigin
            | WitnessError::TenantNotAuthorized
            | WitnessError::InvalidOriginTrust
            | WitnessError::InvalidSignature => (StatusCode::UNAUTHORIZED, "invalid_checkpoint"),
            WitnessError::Conflict | WitnessError::OutOfSequence => {
                (StatusCode::CONFLICT, "checkpoint_conflict")
            }
            WitnessError::Storage(_)
            | WitnessError::Serialization(_)
            | WitnessError::LockPoisoned
            | WitnessError::Clock => (StatusCode::INTERNAL_SERVER_ERROR, "witness_unavailable"),
        };
        (status, Json(serde_json::json!({"error": code}))).into_response()
    }
}

/// Loads a tenant head through either a connection or transaction.
fn load_head_from_connection(
    connection: &Connection,
    tenant_id: &str,
) -> Result<Option<WitnessHead>, WitnessError> {
    connection
        .query_row(
            "SELECT tenant_id, sequence, event_hash, witnessed_at_ms
             FROM witness_heads WHERE tenant_id = ?1",
            [tenant_id],
            |row| {
                Ok(WitnessHead {
                    tenant_id: row.get(0)?,
                    sequence: row.get(1)?,
                    event_hash: row.get(2)?,
                    witnessed_at_ms: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(WitnessError::from)
}

#[cfg(test)]
/// Exercises checkpoint authentication, ordering, and tenant isolation.
mod tests {
    use std::collections::BTreeMap;

    use ed25519_dalek::SigningKey;
    use henosis_audit::{AuditEventInput, AuditPhase, AuditStore, OriginSigner};
    use serde_json::json;

    use super::*;

    /// Builds a witness and matching origin signer for isolated tests.
    fn fixture() -> (WitnessStore, OriginSigner, VerifyingKey) {
        let origin = OriginSigner::new("origin-a", SigningKey::from_bytes(&[3_u8; 32])).unwrap();
        let witness_signer = SigningKey::from_bytes(&[4_u8; 32]);
        let store = WitnessStore::open_in_memory(
            BTreeMap::from([(
                "origin-a".into(),
                TrustedOrigin::new(origin.verifying_key(), ["tenant-a".into()]).unwrap(),
            )]),
            "witness-a",
            witness_signer.clone(),
        )
        .unwrap();
        (store, origin, witness_signer.verifying_key())
    }

    /// Appends a valid event and signs its checkpoint.
    fn checkpoint(
        origin: &OriginSigner,
        tenant_id: &str,
        idempotency_key: &str,
    ) -> WitnessCheckpoint {
        let audit = AuditStore::open_in_memory().unwrap();
        let record = audit
            .append(AuditEventInput {
                tenant_id: tenant_id.into(),
                principal_id: "machine:test".into(),
                action: "tool.invoke".into(),
                phase: AuditPhase::Intent,
                request: json!({"action": "ping"}),
                payload: json!({"tool": "demo"}),
                idempotency_key: Some(idempotency_key.into()),
            })
            .unwrap();
        origin.checkpoint(&record)
    }

    /// Proves that a valid checkpoint advances a tenant head and yields a verifiable receipt.
    #[test]
    fn accepts_valid_checkpoint() {
        let (store, origin, witness_key) = fixture();
        let checkpoint = checkpoint(&origin, "tenant-a", "accept");
        let receipt = store.accept(&checkpoint).unwrap();

        henosis_audit::verify_witness_receipt(&receipt, &witness_key).unwrap();
        assert_eq!(store.head("tenant-a").unwrap().unwrap().sequence, 1);
    }

    /// Proves that exact retries return the original signed receipt.
    #[test]
    fn exact_retry_is_idempotent() {
        let (store, origin, _) = fixture();
        let checkpoint = checkpoint(&origin, "tenant-a", "retry");

        let first = store.accept(&checkpoint).unwrap();
        let second = store.accept(&checkpoint).unwrap();
        assert_eq!(first, second);
    }

    /// Proves that a checkpoint altered after origin signing is rejected.
    #[test]
    fn rejects_tampered_checkpoint() {
        let (store, origin, _) = fixture();
        let mut checkpoint = checkpoint(&origin, "tenant-a", "tamper");
        checkpoint.event_hash = "f".repeat(64);

        assert!(matches!(
            store.accept(&checkpoint),
            Err(WitnessError::InvalidSignature)
        ));
    }

    /// Proves that a tenant stream cannot skip a sequence.
    #[test]
    fn rejects_sequence_gap() {
        let (store, origin, _) = fixture();
        let mut checkpoint = checkpoint(&origin, "tenant-a", "gap");
        checkpoint.sequence = 2;

        assert!(matches!(
            store.accept(&checkpoint),
            Err(WitnessError::InvalidSignature) | Err(WitnessError::OutOfSequence)
        ));
    }

    /// Rejects a valid signature when its trusted key lacks authorization for that tenant.
    #[test]
    fn rejects_valid_key_for_unlisted_tenant() {
        let (store, origin, _) = fixture();
        let checkpoint = checkpoint(&origin, "tenant-b", "wrong-tenant");

        assert!(matches!(
            store.accept(&checkpoint),
            Err(WitnessError::TenantNotAuthorized)
        ));
        assert!(store.head("tenant-b").unwrap().is_none());
    }

    /// Rejects an origin trust entry without at least one exact tenant identifier.
    #[test]
    fn rejects_empty_origin_tenant_allowlist() {
        let key = SigningKey::from_bytes(&[8_u8; 32]).verifying_key();

        assert!(matches!(
            TrustedOrigin::new(key, Vec::new()),
            Err(WitnessError::InvalidOriginTrust)
        ));
    }
}
