//! Per-provider circuit breakers.
//!
//! One breaker per upstream provider (google/github/slack/...). All adapters
//! for a provider share its breaker, so a Google outage trips Gmail, Drive and
//! Calendar together. State transitions are best-effort mirrored to Axon on
//! channel `hermes.circuit`.
//!
//! The breaker is consulted *before* an adapter's retry loop runs: if the
//! circuit is Open we fail fast with a structured `circuit_open` error rather
//! than hammering a known-bad upstream. Success/failure is recorded after the
//! adapter returns (i.e. after its retries are exhausted).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use tracing::{info, warn};

use crate::tool::{InvokeContext, InvokeRequest, InvokeResponse, Tool};

/// Atomic state constant: circuit is closed (healthy).
const STATE_CLOSED: u8 = 0;
/// Atomic state constant: circuit is open (tripped).
const STATE_OPEN: u8 = 1;
/// Atomic state constant: circuit is half-open (probing recovery).
const STATE_HALF_OPEN: u8 = 2;

/// Public, serializable view of a breaker's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Circuit is closed: all calls proceed normally.
    Closed,
    /// Circuit is open: calls are rejected fast to protect the upstream.
    Open,
    /// Circuit is half-open: a limited number of probe calls are allowed.
    HalfOpen,
}

impl CircuitState {
    /// Parse the atomic `u8` constant back into a `CircuitState`.
    fn from_u8(v: u8) -> Self {
        match v {
            STATE_OPEN => CircuitState::Open,
            STATE_HALF_OPEN => CircuitState::HalfOpen,
            _ => CircuitState::Closed,
        }
    }

    /// Stable lowercase string representation for JSON serialization.
    pub fn as_str(self) -> &'static str {
        match self {
            CircuitState::Closed => "closed",
            CircuitState::Open => "open",
            CircuitState::HalfOpen => "half_open",
        }
    }
}

/// Tunables for a breaker. Defaults come from the plan; env vars override.
#[derive(Debug, Clone, Copy)]
pub struct CircuitConfig {
    /// Consecutive failures required to trip Closed -> Open.
    pub failure_threshold: u32,
    /// How long a breaker stays Open before allowing a half-open probe.
    pub recovery_timeout_ms: u64,
    /// Max concurrent probes permitted while HalfOpen.
    pub half_open_max: u32,
}

impl Default for CircuitConfig {
    /// Read tunables from environment variables, falling back to plan defaults.
    fn default() -> Self {
        let env_u32 = |k: &str, d: u32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let env_u64 = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Self {
            failure_threshold: env_u32("HERMES_CIRCUIT_FAILURE_THRESHOLD", 5),
            recovery_timeout_ms: env_u64("HERMES_CIRCUIT_RECOVERY_MS", 60_000),
            half_open_max: env_u32("HERMES_CIRCUIT_HALF_OPEN_MAX", 1),
        }
    }
}

/// Outcome of consulting a breaker before an invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitDecision {
    /// The call is allowed to proceed.
    Allowed,
    /// The call is rejected; the breaker is open and will recover in roughly
    /// `recovery_eta_ms` milliseconds.
    Rejected {
        /// Estimated milliseconds until the breaker enters half-open.
        recovery_eta_ms: u64,
    },
}

/// A state transition worth mirroring to Axon.
#[derive(Debug, Clone, Copy)]
enum Transition {
    /// The circuit just tripped open.
    Opened,
    /// The circuit just closed (recovered).
    Closed,
    /// The circuit entered the half-open probe state.
    HalfOpen,
}

impl Transition {
    /// Axon action string for this transition.
    fn action(self) -> &'static str {
        match self {
            Transition::Opened => "hermes.circuit.opened",
            Transition::Closed => "hermes.circuit.closed",
            Transition::HalfOpen => "hermes.circuit.half_open",
        }
    }
}

/// Lock-free breaker state for a single provider.
#[derive(Debug)]
struct CircuitBreaker {
    /// Tunable configuration for this breaker.
    cfg: CircuitConfig,
    /// Current state (closed/open/half-open) stored atomically.
    state: AtomicU8,
    /// Consecutive failure count since the last success.
    consecutive_failures: AtomicU32,
    /// Number of half-open probe calls in flight.
    half_open_calls: AtomicU32,
    /// Timestamp of the last failure in milliseconds since the Unix epoch.
    last_failure_ms: AtomicI64,
    /// Timestamp of the last success in milliseconds since the Unix epoch.
    last_success_ms: AtomicI64,
}

/// Current Unix time in milliseconds.
fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl CircuitBreaker {
    /// Construct a new breaker in the Closed state with the given config.
    fn new(cfg: CircuitConfig) -> Self {
        Self {
            cfg,
            state: AtomicU8::new(STATE_CLOSED),
            consecutive_failures: AtomicU32::new(0),
            half_open_calls: AtomicU32::new(0),
            last_failure_ms: AtomicI64::new(0),
            last_success_ms: AtomicI64::new(0),
        }
    }

    /// Decide whether to allow a call, transitioning Open -> HalfOpen when the
    /// recovery window has elapsed.
    fn decide(&self) -> (CircuitDecision, Option<Transition>) {
        match CircuitState::from_u8(self.state.load(Ordering::SeqCst)) {
            CircuitState::Closed => (CircuitDecision::Allowed, None),
            CircuitState::HalfOpen => (self.probe(), None),
            CircuitState::Open => {
                let elapsed = now_ms() - self.last_failure_ms.load(Ordering::SeqCst);
                if elapsed >= self.cfg.recovery_timeout_ms as i64 {
                    // First caller past the window flips Open -> HalfOpen.
                    if self
                        .state
                        .compare_exchange(STATE_OPEN, STATE_HALF_OPEN, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        self.half_open_calls.store(0, Ordering::SeqCst);
                        (self.probe(), Some(Transition::HalfOpen))
                    } else {
                        (self.probe(), None)
                    }
                } else {
                    let eta = (self.cfg.recovery_timeout_ms as i64 - elapsed).max(0) as u64;
                    (CircuitDecision::Rejected { recovery_eta_ms: eta }, None)
                }
            }
        }
    }

    /// Allow up to `half_open_max` probes while HalfOpen.
    fn probe(&self) -> CircuitDecision {
        let used = self.half_open_calls.fetch_add(1, Ordering::SeqCst);
        if used < self.cfg.half_open_max {
            CircuitDecision::Allowed
        } else {
            CircuitDecision::Rejected { recovery_eta_ms: 0 }
        }
    }

    /// Record a successful invocation. Resets the failure count and closes the
    /// circuit if it was open.
    fn on_success(&self) -> Option<Transition> {
        self.last_success_ms.store(now_ms(), Ordering::SeqCst);
        self.consecutive_failures.store(0, Ordering::SeqCst);
        self.half_open_calls.store(0, Ordering::SeqCst);
        let prev = self.state.swap(STATE_CLOSED, Ordering::SeqCst);
        (prev != STATE_CLOSED).then_some(Transition::Closed)
    }

    /// Record a failed invocation. Increments the failure counter; trips the
    /// circuit when the threshold is reached or a half-open probe fails.
    fn on_failure(&self) -> Option<Transition> {
        self.last_failure_ms.store(now_ms(), Ordering::SeqCst);
        let fails = self.consecutive_failures.fetch_add(1, Ordering::SeqCst) + 1;
        match CircuitState::from_u8(self.state.load(Ordering::SeqCst)) {
            // A probe failed -> straight back to Open.
            CircuitState::HalfOpen => {
                self.state.store(STATE_OPEN, Ordering::SeqCst);
                Some(Transition::Opened)
            }
            _ => {
                if fails >= self.cfg.failure_threshold {
                    let was = self.state.swap(STATE_OPEN, Ordering::SeqCst);
                    (was != STATE_OPEN).then_some(Transition::Opened)
                } else {
                    None
                }
            }
        }
    }

    /// Snapshot the breaker's current health (state + timestamps + failure
    /// count) for the `/health/adapters` response.
    fn health(&self) -> CircuitHealth {
        CircuitHealth {
            circuit_state: CircuitState::from_u8(self.state.load(Ordering::SeqCst)).as_str(),
            last_success_at: ms_to_rfc3339(self.last_success_ms.load(Ordering::SeqCst)),
            last_failure_at: ms_to_rfc3339(self.last_failure_ms.load(Ordering::SeqCst)),
            consecutive_failures: self.consecutive_failures.load(Ordering::SeqCst),
        }
    }
}

/// Convert a millisecond timestamp to an RFC3339 string; `None` for epoch/zero.
fn ms_to_rfc3339(ms: i64) -> Option<String> {
    if ms <= 0 {
        None
    } else {
        DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339())
    }
}

/// Serializable per-provider health snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct CircuitHealth {
    /// Current circuit state label ("closed", "open", or "half_open").
    pub circuit_state: &'static str,
    /// RFC3339 timestamp of the last successful invocation, or `None`.
    pub last_success_at: Option<String>,
    /// RFC3339 timestamp of the last failed invocation, or `None`.
    pub last_failure_at: Option<String>,
    /// Number of consecutive failures since the last success.
    pub consecutive_failures: u32,
}

/// Registry of per-provider breakers plus best-effort Axon mirroring.
pub struct CircuitRegistry {
    /// Default configuration applied to newly-created breakers.
    cfg: CircuitConfig,
    /// Map from provider name to its breaker, created lazily.
    breakers: Mutex<HashMap<String, Arc<CircuitBreaker>>>,
    /// Axon base URL for state-transition events; `None` disables mirroring.
    axon_url: Option<String>,
    /// Shared HTTP client for Axon publish calls.
    http: reqwest::Client,
}

impl CircuitRegistry {
    /// Construct a new registry reading config and `AXON_URL` from the
    /// environment.
    pub fn new() -> Self {
        Self {
            cfg: CircuitConfig::default(),
            breakers: Mutex::new(HashMap::new()),
            axon_url: std::env::var("AXON_URL").ok(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client build"),
        }
    }

    /// Fetch (or lazily create) the breaker for a provider.
    fn breaker(&self, provider: &str) -> Arc<CircuitBreaker> {
        let mut guard = self.breakers.lock().expect("circuit map poisoned");
        guard
            .entry(provider.to_string())
            .or_insert_with(|| Arc::new(CircuitBreaker::new(self.cfg)))
            .clone()
    }

    /// Consult the breaker before invoking. Publishes a half-open transition
    /// if this call opened the recovery probe window.
    pub fn check(&self, provider: &str) -> CircuitDecision {
        let (decision, transition) = self.breaker(provider).decide();
        if let Some(t) = transition {
            self.publish(provider, t);
        }
        decision
    }

    /// Record a successful invocation for a provider's breaker.
    pub fn record_success(&self, provider: &str) {
        if let Some(t) = self.breaker(provider).on_success() {
            self.publish(provider, t);
        }
    }

    /// Record a failed invocation for a provider's breaker.
    pub fn record_failure(&self, provider: &str) {
        if let Some(t) = self.breaker(provider).on_failure() {
            self.publish(provider, t);
        }
    }

    /// Return a health snapshot for a provider's breaker.
    pub fn health(&self, provider: &str) -> CircuitHealth {
        self.breaker(provider).health()
    }

    /// Number of providers whose breaker is currently in the Open state. Feeds
    /// the `active_circuits_open` global metric.
    pub fn open_count(&self) -> usize {
        self.breakers
            .lock()
            .expect("circuit map poisoned")
            .values()
            .filter(|b| {
                CircuitState::from_u8(b.state.load(Ordering::SeqCst)) == CircuitState::Open
            })
            .count()
    }

    /// Best-effort Axon publish of a circuit state transition.
    fn publish(&self, provider: &str, transition: Transition) {
        let action = transition.action();
        match transition {
            Transition::Opened => warn!(provider, "circuit opened"),
            Transition::Closed => info!(provider, "circuit closed"),
            Transition::HalfOpen => info!(provider, "circuit half-open probe"),
        }
        let Some(axon_url) = &self.axon_url else {
            return;
        };
        let url = format!("{}/axon/publish", axon_url.trim_end_matches('/'));
        let body = json!({
            "channel": "hermes.circuit",
            "action": action,
            "payload": { "provider": provider },
            "source": "hermes",
        });
        let req = self.http.post(&url).json(&body).send();
        // Best-effort: spawn and forget so circuit bookkeeping never blocks.
        tokio::spawn(async move {
            match req.await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => warn!(status = %r.status(), "circuit axon mirror non-2xx"),
                Err(e) => warn!(error = %e, "circuit axon mirror failed"),
            }
        });
    }
}

impl Default for CircuitRegistry {
    /// Delegates to [`CircuitRegistry::new`].
    fn default() -> Self {
        Self::new()
    }
}

/// Run a tool through validation and its provider's circuit breaker: reject
/// invalid args up front, fail fast if the circuit is Open, otherwise invoke
/// and record the outcome. Shared by the HTTP and MCP paths.
///
/// Returns the response paired with the number of upstream retries the tool
/// performed (0 for the early-return validation/circuit-open paths). Retries are
/// counted via an ambient [`RETRY_COUNTER`](crate::adapters::common::RETRY_COUNTER)
/// scope so adapters need no plumbing.
pub async fn invoke_with_circuit(
    circuits: &CircuitRegistry,
    tool: &Arc<dyn Tool>,
    tool_id: &str,
    ctx: &InvokeContext,
    req: InvokeRequest,
) -> (InvokeResponse, u32) {
    let schema = tool.schema();
    // Validate args against the tool's input schema before doing any work.
    if let Err(errors) = crate::validation::validate(&schema.input_schema, &req.args) {
        return (validation_error_response(tool_id, errors), 0);
    }

    let provider = tool.provider();
    if let CircuitDecision::Rejected { recovery_eta_ms } = circuits.check(provider) {
        return (circuit_open_response(tool_id, provider, recovery_eta_ms), 0);
    }

    // Establish the per-invocation retry counter and read it back inside the
    // scope (task-local values are consumed by `scope`).
    let (resp, retries) = crate::adapters::common::RETRY_COUNTER
        .scope(std::sync::atomic::AtomicU32::new(0), async {
            let resp = tool.invoke(ctx, req).await;
            let retries = crate::adapters::common::RETRY_COUNTER
                .with(|c| c.load(std::sync::atomic::Ordering::Relaxed));
            (resp, retries)
        })
        .await;

    if resp.success {
        circuits.record_success(provider);
    } else if is_provider_failure(&resp) {
        circuits.record_failure(provider);
    }
    (resp, retries)
}

/// Whether an error response represents an upstream/provider fault (counts
/// against the circuit) versus a client-side error (does not). Provider faults
/// are the `*_unreachable` network errors and 5xx upstream statuses; 4xx,
/// validation, auth and rate-limit errors do not trip the breaker.
fn is_provider_failure(resp: &InvokeResponse) -> bool {
    let Some(err) = &resp.error else {
        return false;
    };
    let code = err.get("code").and_then(|v| v.as_str()).unwrap_or("");
    if code.ends_with("_unreachable") {
        return true;
    }
    err.get("status")
        .and_then(|v| v.as_u64())
        .map(|s| s >= 500)
        .unwrap_or(false)
}

/// Structured response for input that fails schema validation.
fn validation_error_response(
    tool_id: &str,
    errors: Vec<crate::validation::FieldError>,
) -> InvokeResponse {
    InvokeResponse {
        tool_id: tool_id.to_string(),
        success: false,
        result: None,
        error: Some(json!({
            "code": "validation_failed",
            "message": "input arguments failed schema validation",
            "errors": errors,
        })),
        duration_ms: 0,
    }
}

/// Structured fast-fail response when a provider's circuit is open.
pub fn circuit_open_response(tool_id: &str, provider: &str, recovery_eta_ms: u64) -> InvokeResponse {
    InvokeResponse {
        tool_id: tool_id.to_string(),
        success: false,
        result: None,
        error: Some(json!({
            "code": "circuit_open",
            "message": format!("circuit open for provider '{provider}'; upstream temporarily unavailable"),
            "provider": provider,
            "recovery_eta_ms": recovery_eta_ms,
        })),
        duration_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker(threshold: u32, recovery_ms: u64) -> CircuitBreaker {
        CircuitBreaker::new(CircuitConfig {
            failure_threshold: threshold,
            recovery_timeout_ms: recovery_ms,
            half_open_max: 1,
        })
    }

    #[test]
    fn closed_allows_until_threshold() {
        let b = breaker(3, 60_000);
        assert_eq!(b.decide().0, CircuitDecision::Allowed);
        assert!(b.on_failure().is_none()); // 1
        assert!(b.on_failure().is_none()); // 2
        let t = b.on_failure(); // 3 -> trips
        assert!(matches!(t, Some(Transition::Opened)));
        assert!(matches!(b.decide().0, CircuitDecision::Rejected { .. }));
    }

    #[test]
    fn success_resets_failures() {
        let b = breaker(2, 60_000);
        b.on_failure();
        b.on_success();
        // Failure count was reset, so one more failure should not trip.
        assert!(b.on_failure().is_none());
        assert_eq!(b.decide().0, CircuitDecision::Allowed);
    }

    #[test]
    fn open_transitions_to_half_open_after_recovery() {
        let b = breaker(1, 0); // trip on first failure, instant recovery window
        b.on_failure();
        // Recovery window is zero, so the next decide() flips to half-open and
        // allows a single probe.
        let (decision, transition) = b.decide();
        assert_eq!(decision, CircuitDecision::Allowed);
        assert!(matches!(transition, Some(Transition::HalfOpen)));
        // half_open_max = 1, so the next probe is rejected.
        assert!(matches!(b.decide().0, CircuitDecision::Rejected { .. }));
    }

    #[test]
    fn half_open_failure_reopens() {
        let b = breaker(1, 0);
        b.on_failure();
        b.decide(); // -> half-open probe
        let t = b.on_failure();
        assert!(matches!(t, Some(Transition::Opened)));
    }

    #[test]
    fn half_open_success_closes() {
        let b = breaker(1, 0);
        b.on_failure();
        b.decide(); // -> half-open probe
        let t = b.on_success();
        assert!(matches!(t, Some(Transition::Closed)));
        assert_eq!(b.decide().0, CircuitDecision::Allowed);
    }

    #[test]
    fn classifies_provider_failures() {
        let unreachable = InvokeResponse {
            tool_id: "x".into(),
            success: false,
            result: None,
            error: Some(json!({ "code": "github_unreachable", "message": "" })),
            duration_ms: 0,
        };
        assert!(is_provider_failure(&unreachable));

        let server_err = InvokeResponse {
            tool_id: "x".into(),
            success: false,
            result: None,
            error: Some(json!({ "code": "github_api_error", "status": 503 })),
            duration_ms: 0,
        };
        assert!(is_provider_failure(&server_err));

        let client_err = InvokeResponse {
            tool_id: "x".into(),
            success: false,
            result: None,
            error: Some(json!({ "code": "github_api_error", "status": 404 })),
            duration_ms: 0,
        };
        assert!(!is_provider_failure(&client_err));

        let bad_request = InvokeResponse {
            tool_id: "x".into(),
            success: false,
            result: None,
            error: Some(json!({ "code": "bad_request" })),
            duration_ms: 0,
        };
        assert!(!is_provider_failure(&bad_request));
    }
}
