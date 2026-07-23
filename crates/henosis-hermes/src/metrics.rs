//! Per-provider and global invocation metrics.
//!
//! Counts are cumulative atomic counters since process start; latency
//! percentiles are computed over a bounded ring of the most recent samples
//! (capped per provider). Recording is best-effort and never blocks an invocation.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde_json::{Value, json};

use crate::tool::InvokeResponse;

/// How the dispatcher classified one invocation's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The tool returned success.
    Success,
    /// The tool returned an error after retries.
    Error,
    /// The call was rejected by the rate limiter before invocation.
    RateLimited,
    /// The call was rejected because the provider's circuit was open.
    CircuitOpen,
    /// The args failed schema validation before invocation.
    ValidationFailed,
}

/// Implements stable outcome labels.
impl Outcome {
    /// Stable lowercase label for audit records and `GET /audit` filtering.
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Error => "error",
            Outcome::RateLimited => "rate_limited",
            Outcome::CircuitOpen => "circuit_open",
            Outcome::ValidationFailed => "validation_failed",
        }
    }

    /// Classify a completed invocation from its response envelope. Shared by the
    /// HTTP and MCP dispatch paths so both record identical outcomes.
    pub fn classify(resp: &InvokeResponse) -> Self {
        if resp.success {
            return Outcome::Success;
        }
        match resp
            .error
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
        {
            Some("circuit_open") => Outcome::CircuitOpen,
            Some("validation_failed") => Outcome::ValidationFailed,
            Some("rate_limited") => Outcome::RateLimited,
            _ => Outcome::Error,
        }
    }
}

/// Maximum latency samples retained per provider for percentile estimation.
const LATENCY_CAP: usize = 1000;

/// Cumulative counters and a bounded latency ring for one provider.
#[derive(Debug, Default)]
struct ProviderMetrics {
    /// Total invocations attempted (every outcome).
    invocation_count: AtomicU64,
    /// Invocations that returned success.
    success_count: AtomicU64,
    /// Invocations that returned an error after retries.
    error_count: AtomicU64,
    /// Calls rejected by the rate limiter.
    rate_limited_count: AtomicU64,
    /// Calls rejected by an open circuit.
    circuit_open_count: AtomicU64,
    /// Total upstream retries across all invocations.
    retry_count: AtomicU64,
    /// Most-recent latency samples (ms), capped at [`LATENCY_CAP`].
    latency_ms: RwLock<VecDeque<u64>>,
}

/// Implements per-provider metric recording and snapshots.
impl ProviderMetrics {
    /// Fold one invocation's outcome, latency, and retry count into the totals.
    fn record(&self, outcome: Outcome, duration_ms: u64, retries: u32) {
        self.invocation_count.fetch_add(1, Ordering::Relaxed);
        match outcome {
            Outcome::Success => {
                self.success_count.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::Error => {
                self.error_count.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::RateLimited => {
                self.rate_limited_count.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::CircuitOpen => {
                self.circuit_open_count.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::ValidationFailed => {
                self.error_count.fetch_add(1, Ordering::Relaxed);
            }
        }
        if retries > 0 {
            self.retry_count
                .fetch_add(u64::from(retries), Ordering::Relaxed);
        }
        // Only invocations that actually ran upstream carry a meaningful
        // latency; rejected-before-invocation outcomes have duration 0 and are
        // not worth sampling.
        if matches!(outcome, Outcome::Success | Outcome::Error) {
            let mut ring = self.latency_ms.write().expect("latency ring poisoned");
            if ring.len() == LATENCY_CAP {
                ring.pop_front();
            }
            ring.push_back(duration_ms);
        }
    }

    /// Snapshot this provider's metrics as JSON.
    fn snapshot(&self) -> Value {
        let mut samples: Vec<u64> = self
            .latency_ms
            .read()
            .expect("latency ring poisoned")
            .iter()
            .copied()
            .collect();
        samples.sort_unstable();
        json!({
            "invocation_count": self.invocation_count.load(Ordering::Relaxed),
            "success_count": self.success_count.load(Ordering::Relaxed),
            "error_count": self.error_count.load(Ordering::Relaxed),
            "rate_limited_count": self.rate_limited_count.load(Ordering::Relaxed),
            "circuit_open_count": self.circuit_open_count.load(Ordering::Relaxed),
            "retry_count": self.retry_count.load(Ordering::Relaxed),
            "latency_p50_ms": percentile(&samples, 50),
            "latency_p95_ms": percentile(&samples, 95),
            "latency_p99_ms": percentile(&samples, 99),
        })
    }
}

/// Nearest-rank percentile over an ascending-sorted sample slice.
///
/// `p` is in `[0, 100]`. Returns 0 for an empty slice. Pure, so the percentile
/// math is unit-testable independent of the registry.
fn percentile(sorted: &[u64], p: u8) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.min(100) as usize;
    // Nearest-rank: rank = ceil(p/100 * n), 1-indexed, clamped to [1, n].
    let rank = (p * sorted.len()).div_ceil(100);
    let idx = rank.clamp(1, sorted.len()) - 1;
    sorted[idx]
}

/// Process-wide metrics registry: per-provider counters plus global totals.
pub struct MetricsRegistry {
    /// Per-provider metric buckets, created on first record.
    providers: RwLock<HashMap<String, Arc<ProviderMetrics>>>,
    /// Total invocations attempted across all providers.
    total_invocations: AtomicU64,
    /// When the registry (process) started, for uptime reporting.
    started: Instant,
}

/// Implements registry-wide metric recording and snapshots.
impl MetricsRegistry {
    /// Construct an empty registry, stamping the process start time.
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
            total_invocations: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    /// Record one invocation outcome against its provider.
    pub fn record(&self, provider: &str, outcome: Outcome, duration_ms: u64, retries: u32) {
        self.total_invocations.fetch_add(1, Ordering::Relaxed);
        // Fast path: provider bucket already exists.
        if let Some(metrics) = self
            .providers
            .read()
            .expect("metrics map poisoned")
            .get(provider)
            .cloned()
        {
            metrics.record(outcome, duration_ms, retries);
            return;
        }
        // Slow path: create the bucket, then record.
        let metrics = {
            let mut guard = self.providers.write().expect("metrics map poisoned");
            guard
                .entry(provider.to_string())
                .or_insert_with(|| Arc::new(ProviderMetrics::default()))
                .clone()
        };
        metrics.record(outcome, duration_ms, retries);
    }

    /// Snapshot the full metrics surface for `GET /metrics`. `active_circuits_open`
    /// is supplied by the caller (the circuit registry owns that count).
    pub fn snapshot(&self, active_circuits_open: usize) -> Value {
        let providers: serde_json::Map<String, Value> = self
            .providers
            .read()
            .expect("metrics map poisoned")
            .iter()
            .map(|(name, m)| (name.clone(), m.snapshot()))
            .collect();
        json!({
            "providers": providers,
            "global": {
                "total_invocations": self.total_invocations.load(Ordering::Relaxed),
                "active_circuits_open": active_circuits_open,
                "uptime_seconds": self.started.elapsed().as_secs(),
            },
        })
    }
}

/// Builds an empty metrics registry.
impl Default for MetricsRegistry {
    /// Delegates to [`MetricsRegistry::new`].
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
/// Tests metric aggregation and percentile calculation.
mod tests {
    use super::*;

    /// An empty sample set has no percentile.
    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[], 99), 0);
    }

    /// A single sample is every percentile.
    #[test]
    fn percentile_single_sample() {
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 99), 42);
    }

    /// Nearest-rank percentiles over 1..=100 land on the rank value.
    #[test]
    fn percentile_nearest_rank() {
        let s: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&s, 50), 50);
        assert_eq!(percentile(&s, 95), 95);
        assert_eq!(percentile(&s, 99), 99);
        assert_eq!(percentile(&s, 100), 100);
    }

    /// p0 and tiny percentiles clamp to the smallest sample, not index -1.
    #[test]
    fn percentile_low_end_clamps_to_first() {
        let s: Vec<u64> = (10..=20).collect();
        assert_eq!(percentile(&s, 0), 10);
        assert_eq!(percentile(&s, 1), 10);
    }

    /// Recording tallies per-outcome counters and global totals.
    #[test]
    fn records_outcomes_and_totals() {
        let reg = MetricsRegistry::new();
        reg.record("google", Outcome::Success, 100, 0);
        reg.record("google", Outcome::Error, 200, 2);
        reg.record("google", Outcome::RateLimited, 0, 0);
        reg.record("github", Outcome::CircuitOpen, 0, 0);

        let snap = reg.snapshot(1);
        let google = &snap["providers"]["google"];
        assert_eq!(google["invocation_count"], 3);
        assert_eq!(google["success_count"], 1);
        assert_eq!(google["error_count"], 1);
        assert_eq!(google["rate_limited_count"], 1);
        assert_eq!(google["retry_count"], 2);
        // Two ran upstream (100, 200) -> p50 nearest-rank is the lower sample.
        assert_eq!(google["latency_p50_ms"], 100);
        assert_eq!(google["latency_p99_ms"], 200);

        assert_eq!(snap["providers"]["github"]["circuit_open_count"], 1);
        assert_eq!(snap["global"]["total_invocations"], 4);
        assert_eq!(snap["global"]["active_circuits_open"], 1);
    }

    /// The latency ring never exceeds its cap.
    #[test]
    fn latency_ring_is_bounded() {
        let m = ProviderMetrics::default();
        for i in 0..(LATENCY_CAP as u64 + 500) {
            m.record(Outcome::Success, i, 0);
        }
        assert_eq!(
            m.latency_ms.read().unwrap().len(),
            LATENCY_CAP,
            "latency ring must stay capped"
        );
    }

    /// Rejected-before-invocation outcomes contribute no latency sample.
    #[test]
    fn rejected_outcomes_skip_latency() {
        let m = ProviderMetrics::default();
        m.record(Outcome::RateLimited, 0, 0);
        m.record(Outcome::CircuitOpen, 0, 0);
        m.record(Outcome::ValidationFailed, 0, 0);
        assert!(m.latency_ms.read().unwrap().is_empty());
    }
}
