//! Structured audit trail for tool invocations.
//!
//! Every tool invocation produces an [`AuditRecord`]: who (tenant), what (tool,
//! provider), how it went (outcome, retries, duration), and a SHA-256 hash of
//! the arguments -- never the arguments themselves, which may carry sensitive
//! values. Records are retained in a bounded in-memory ring (last
//! [`RING_CAP`]) for `GET /audit`, and published to Axon on channel
//! `hermes.audit` in batches every 10 seconds (or sooner once a batch threshold
//! is reached). Publishing is best-effort and never blocks an invocation.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::axon::AxonPublisher;
use crate::metrics::Outcome;

/// Maximum audit records retained in memory for `GET /audit`.
const RING_CAP: usize = 10_000;

/// Pending-record count that triggers an immediate batch publish ahead of the
/// 10-second timer.
const BATCH_THRESHOLD: usize = 500;

/// Interval between scheduled audit batch publishes.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(10);

/// One audit record for a single tool invocation.
#[derive(Debug, Clone, Serialize)]
pub struct AuditRecord {
    /// When the invocation completed (serialized RFC3339).
    pub timestamp: DateTime<Utc>,
    /// Tenant on whose behalf the call ran, if any.
    pub tenant_id: Option<String>,
    /// The invoked tool id.
    pub tool_id: String,
    /// The upstream provider the tool talks to.
    pub provider: String,
    /// Wall-clock duration of the invocation in milliseconds.
    pub duration_ms: u64,
    /// Outcome label (`success`/`error`/`rate_limited`/`circuit_open`/`validation_failed`).
    pub outcome: String,
    /// Structured error code when `outcome == "error"` (or another failure with
    /// a code), else `None`.
    pub error_code: Option<String>,
    /// Total upstream retries for this invocation.
    pub retries: u32,
    /// SHA-256 hex of the serialized args -- NOT the args themselves.
    pub args_hash: String,
}

/// SHA-256 (hex) of the serialized arguments. Hashing, not storing, keeps secret
/// argument values out of the audit trail while still allowing two invocations
/// with identical args to be correlated.
pub fn args_hash(args: &Value) -> String {
    let bytes = serde_json::to_vec(args).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

/// Filters for a `GET /audit` query. Every field is optional; `None` matches
/// everything.
#[derive(Debug, Default)]
pub struct AuditQuery {
    /// Restrict to one tenant.
    pub tenant_id: Option<String>,
    /// Restrict to one tool id.
    pub tool_id: Option<String>,
    /// Restrict to one outcome label.
    pub outcome: Option<String>,
    /// Lower time bound (inclusive).
    pub since: Option<DateTime<Utc>>,
    /// Upper time bound (inclusive).
    pub until: Option<DateTime<Utc>>,
    /// Maximum records returned (most recent first).
    pub limit: Option<usize>,
}

/// The in-memory audit trail: a bounded ring for queries plus a pending buffer
/// drained to Axon.
pub struct AuditTrail {
    /// Recent records, newest at the back, capped at [`RING_CAP`].
    ring: Mutex<VecDeque<AuditRecord>>,
    /// Records awaiting the next Axon batch publish.
    pending: Mutex<Vec<AuditRecord>>,
    /// Best-effort Axon publisher.
    axon: AxonPublisher,
}

/// Implements bounded audit recording and publication.
impl AuditTrail {
    /// Construct an audit trail over an Axon publisher.
    pub fn new(axon: AxonPublisher) -> Self {
        Self {
            ring: Mutex::new(VecDeque::with_capacity(1024)),
            pending: Mutex::new(Vec::new()),
            axon,
        }
    }

    /// Build a record from invocation facts and append it to the ring and the
    /// pending batch. Triggers an early publish once the batch threshold is hit.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        tenant_id: Option<String>,
        tool_id: &str,
        provider: &str,
        duration_ms: u64,
        outcome: Outcome,
        error_code: Option<String>,
        retries: u32,
        args_hash: String,
    ) {
        let rec = AuditRecord {
            timestamp: Utc::now(),
            tenant_id,
            tool_id: tool_id.to_string(),
            provider: provider.to_string(),
            duration_ms,
            outcome: outcome.label().to_string(),
            error_code,
            retries,
            args_hash,
        };

        {
            let mut ring = self.ring.lock().expect("audit ring poisoned");
            if ring.len() == RING_CAP {
                ring.pop_front();
            }
            ring.push_back(rec.clone());
        }

        let batch = {
            let mut pending = self.pending.lock().expect("audit pending poisoned");
            pending.push(rec);
            if pending.len() >= BATCH_THRESHOLD {
                Some(std::mem::take(&mut *pending))
            } else {
                None
            }
        };
        if let Some(batch) = batch {
            self.publish_batch(batch);
        }
    }

    /// Query the ring, newest first, applying the filters.
    pub fn query(&self, q: &AuditQuery) -> Vec<AuditRecord> {
        let ring = self.ring.lock().expect("audit ring poisoned");
        let mut out: Vec<AuditRecord> = ring
            .iter()
            .rev()
            .filter(|r| {
                q.tenant_id
                    .as_ref()
                    .is_none_or(|t| r.tenant_id.as_deref() == Some(t.as_str()))
                    && q.tool_id.as_ref().is_none_or(|t| &r.tool_id == t)
                    && q.outcome.as_ref().is_none_or(|o| &r.outcome == o)
                    && q.since.is_none_or(|s| r.timestamp >= s)
                    && q.until.is_none_or(|u| r.timestamp <= u)
            })
            .cloned()
            .collect();
        if let Some(limit) = q.limit {
            out.truncate(limit);
        }
        out
    }

    /// Drain the pending buffer and publish it as one Axon batch.
    fn publish_batch(&self, batch: Vec<AuditRecord>) {
        if batch.is_empty() || !self.axon.enabled() {
            return;
        }
        let records = serde_json::to_value(&batch).unwrap_or_else(|_| json!([]));
        self.axon.publish(
            "hermes.audit",
            "hermes.audit.batch",
            json!({ "count": batch.len(), "records": records }),
        );
    }

    /// Spawn the periodic publisher: every [`PUBLISH_INTERVAL`], drain and
    /// publish whatever has accumulated.
    pub fn spawn_publisher(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PUBLISH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let batch = {
                    let mut pending = self.pending.lock().expect("audit pending poisoned");
                    std::mem::take(&mut *pending)
                };
                self.publish_batch(batch);
            }
        });
    }
}

#[cfg(test)]
/// Tests audit record retention and publication behavior.
mod tests {
    use super::*;

    /// Build a trail with publishing disabled (no AXON_URL needed for tests).
    fn trail() -> AuditTrail {
        // AxonPublisher::from_env with AXON_URL unset yields a disabled publisher.
        AuditTrail::new(AxonPublisher::from_env())
    }

    /// Records one compact fixture entry in an audit trail.
    fn rec(trail: &AuditTrail, tenant: Option<&str>, tool: &str, outcome: Outcome) {
        trail.record(
            tenant.map(String::from),
            tool,
            "github",
            12,
            outcome,
            None,
            0,
            args_hash(&json!({"a": 1})),
        );
    }

    /// The args hash is the SHA-256 hex of the serialized args, deterministic
    /// and not the args themselves.
    #[test]
    fn args_hash_is_sha256_hex() {
        let h = args_hash(&json!({"token": "secret"}));
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, args_hash(&json!({"token": "secret"})));
        assert_ne!(h, args_hash(&json!({"token": "other"})));
    }

    /// Records come back newest-first and respect tenant/tool/outcome filters.
    #[test]
    fn query_filters_and_orders() {
        let t = trail();
        rec(&t, Some("acme"), "github.create_issue", Outcome::Success);
        rec(&t, Some("acme"), "github.list_issues", Outcome::Error);
        rec(&t, Some("globex"), "github.create_issue", Outcome::Success);

        let all = t.query(&AuditQuery::default());
        assert_eq!(all.len(), 3);
        // Newest first.
        assert_eq!(all[0].tenant_id.as_deref(), Some("globex"));

        let acme = t.query(&AuditQuery {
            tenant_id: Some("acme".into()),
            ..Default::default()
        });
        assert_eq!(acme.len(), 2);

        let errors = t.query(&AuditQuery {
            outcome: Some("error".into()),
            ..Default::default()
        });
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool_id, "github.list_issues");

        let by_tool = t.query(&AuditQuery {
            tool_id: Some("github.create_issue".into()),
            ..Default::default()
        });
        assert_eq!(by_tool.len(), 2);
    }

    /// The limit caps the result count.
    #[test]
    fn query_respects_limit() {
        let t = trail();
        for _ in 0..5 {
            rec(&t, None, "github.list_issues", Outcome::Success);
        }
        let limited = t.query(&AuditQuery {
            limit: Some(2),
            ..Default::default()
        });
        assert_eq!(limited.len(), 2);
    }

    /// The ring never exceeds its cap.
    #[test]
    fn ring_is_bounded() {
        let t = trail();
        for _ in 0..(RING_CAP + 100) {
            rec(&t, None, "github.list_issues", Outcome::Success);
        }
        assert_eq!(t.ring.lock().unwrap().len(), RING_CAP);
    }
}
