//! Per-(tenant, tool) token-bucket rate limiter.
//!
//! Default policy: 60 requests per 60 seconds (1 rps sustained, burst 60).
//! `HERMES_RATE_LIMIT_PER_MIN` overrides the per-bucket capacity. Setting it
//! to 0 disables rate limiting.
//!
//! Kleos counter mirroring: each accept/reject decision optionally fires a
//! best-effort POST to `{AXON_URL}/axon/publish` with channel
//! `thymus.metric` so usage shows up in the activity hub.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::Mutex;
use tracing::warn;

/// Token bucket state for a single (tenant, tool) pair.
#[derive(Debug, Clone)]
struct Bucket {
    /// Current token count (fractional allowed).
    tokens: f64,
    /// When the bucket was last refilled.
    last_refill: Instant,
}

/// Configuration for the token-bucket rate limiter.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Bucket capacity (also the max burst).
    pub capacity: u32,
    /// Tokens replenished per second. Default = capacity / 60 so a bucket
    /// fully refills in one minute.
    pub refill_per_sec: f64,
}

impl Default for RateLimitConfig {
    /// Read `HERMES_RATE_LIMIT_PER_MIN` from the environment; default 60.
    fn default() -> Self {
        let capacity = std::env::var("HERMES_RATE_LIMIT_PER_MIN")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(60);
        let refill_per_sec = if capacity == 0 {
            0.0
        } else {
            capacity as f64 / 60.0
        };
        Self {
            capacity,
            refill_per_sec,
        }
    }
}

/// Per-(tenant, tool) token-bucket rate limiter with optional Axon metric
/// mirroring.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    /// Configured bucket capacity and refill rate.
    cfg: RateLimitConfig,
    /// Per-(tenant, tool) bucket map, protected by an async mutex.
    buckets: Arc<Mutex<HashMap<(String, String), Bucket>>>,
    /// Axon URL for best-effort metric mirroring; `None` disables mirroring.
    axon_url: Option<String>,
    /// Shared HTTP client for metric mirroring calls.
    http: reqwest::Client,
}

/// Result of a rate-limit check for one invocation.
#[derive(Debug, Clone)]
pub enum CheckOutcome {
    /// The invocation is allowed.
    Allowed,
    /// The invocation is throttled; caller should retry after `retry_after_secs`.
    Throttled {
        /// Suggested retry delay in seconds.
        retry_after_secs: u64,
    },
    /// Rate limiting is disabled (capacity = 0); always allowed.
    Disabled,
}

impl RateLimiter {
    /// Construct a rate limiter from the given config, reading `AXON_URL` for
    /// metric mirroring.
    pub fn new(cfg: RateLimitConfig) -> Self {
        Self {
            cfg,
            buckets: Arc::new(Mutex::new(HashMap::new())),
            axon_url: std::env::var("AXON_URL").ok(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client build"),
        }
    }

    /// Configured per-bucket capacity (requests per minute). 0 = disabled.
    pub fn capacity(&self) -> u32 {
        self.cfg.capacity
    }

    /// Try to consume one token for a (tenant, tool) pair, using the limiter's
    /// configured capacity. Disabled buckets always return Allowed.
    pub async fn check(&self, tenant_id: &str, tool_id: &str) -> CheckOutcome {
        self.check_with_capacity(tenant_id, tool_id, None).await
    }

    /// Try to consume one token, optionally overriding the per-minute capacity
    /// for this (tenant, tool) pair. A per-tenant `rate_limit_override` of 0
    /// disables limiting for that pair; `None` uses the global configured
    /// capacity.
    pub async fn check_with_capacity(
        &self,
        tenant_id: &str,
        tool_id: &str,
        capacity_override: Option<u32>,
    ) -> CheckOutcome {
        let capacity = capacity_override.unwrap_or(self.cfg.capacity);
        if capacity == 0 {
            return CheckOutcome::Disabled;
        }
        let refill_per_sec = capacity as f64 / 60.0;

        let key = (tenant_id.to_string(), tool_id.to_string());
        let outcome = {
            let mut guard = self.buckets.lock().await;
            let now = Instant::now();
            let bucket = guard.entry(key.clone()).or_insert(Bucket {
                tokens: capacity as f64,
                last_refill: now,
            });

            // Refill based on elapsed wall time, capping at the effective
            // capacity (which may be the per-tenant override).
            let elapsed = now
                .saturating_duration_since(bucket.last_refill)
                .as_secs_f64();
            let added = elapsed * refill_per_sec;
            bucket.tokens = (bucket.tokens + added).min(capacity as f64);
            bucket.last_refill = now;

            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                CheckOutcome::Allowed
            } else {
                let need = 1.0 - bucket.tokens;
                let wait = (need / refill_per_sec).ceil() as u64;
                CheckOutcome::Throttled {
                    retry_after_secs: wait.max(1),
                }
            }
        };

        let action_str = match &outcome {
            CheckOutcome::Allowed => "tool.allowed",
            CheckOutcome::Throttled { .. } => "tool.throttled",
            CheckOutcome::Disabled => return outcome,
        };
        self.publish_metric(tenant_id, tool_id, action_str).await;
        outcome
    }

    /// Fire a best-effort `thymus.metric` event to Axon. Spawned and forgotten;
    /// never blocks the hot path.
    async fn publish_metric(&self, tenant_id: &str, tool_id: &str, action: &str) {
        let Some(axon_url) = &self.axon_url else {
            return;
        };
        let url = format!("{}/axon/publish", axon_url.trim_end_matches('/'));
        let body = json!({
            "channel": "thymus.metric",
            "action": action,
            "payload": {
                "tenant_id": tenant_id,
                "tool_id": tool_id,
            },
            "source": "hermes",
        });
        let req = self.http.post(&url).json(&body).send();
        // Don't await -- mirroring is best-effort and must not slow up the
        // hot path. Spawn and forget.
        tokio::spawn(async move {
            match req.await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => warn!(status = %r.status(), "thymus metric mirror non-2xx"),
                Err(e) => warn!(error = %e, "thymus metric mirror failed"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allows_under_capacity_then_throttles() {
        let cfg = RateLimitConfig {
            capacity: 3,
            refill_per_sec: 0.0001, // effectively zero refill in test window
        };
        let rl = RateLimiter::new(cfg);
        for _ in 0..3 {
            assert!(matches!(
                rl.check("t1", "tool.x").await,
                CheckOutcome::Allowed
            ));
        }
        assert!(matches!(
            rl.check("t1", "tool.x").await,
            CheckOutcome::Throttled { .. }
        ));
    }

    #[tokio::test]
    async fn separate_tenants_do_not_share_buckets() {
        let cfg = RateLimitConfig {
            capacity: 1,
            refill_per_sec: 0.0001,
        };
        let rl = RateLimiter::new(cfg);
        assert!(matches!(
            rl.check("t1", "tool.x").await,
            CheckOutcome::Allowed
        ));
        // Different tenant should still get its own bucket.
        assert!(matches!(
            rl.check("t2", "tool.x").await,
            CheckOutcome::Allowed
        ));
        // Same tenant + tool should now throttle.
        assert!(matches!(
            rl.check("t1", "tool.x").await,
            CheckOutcome::Throttled { .. }
        ));
    }

    #[tokio::test]
    async fn capacity_zero_means_disabled() {
        let cfg = RateLimitConfig {
            capacity: 0,
            refill_per_sec: 0.0,
        };
        let rl = RateLimiter::new(cfg);
        for _ in 0..1000 {
            assert!(matches!(
                rl.check("t1", "tool.x").await,
                CheckOutcome::Disabled
            ));
        }
    }

    /// A per-call capacity override throttles below the global capacity.
    #[tokio::test]
    async fn capacity_override_limits_per_call() {
        // Global capacity is high; the per-tenant override of 2 is what bites.
        let rl = RateLimiter::new(RateLimitConfig {
            capacity: 100,
            refill_per_sec: 0.0001,
        });
        assert!(matches!(
            rl.check_with_capacity("t", "x", Some(2)).await,
            CheckOutcome::Allowed
        ));
        assert!(matches!(
            rl.check_with_capacity("t", "x", Some(2)).await,
            CheckOutcome::Allowed
        ));
        assert!(matches!(
            rl.check_with_capacity("t", "x", Some(2)).await,
            CheckOutcome::Throttled { .. }
        ));
    }

    /// A per-tenant override of 0 disables limiting for that pair.
    #[tokio::test]
    async fn capacity_override_zero_disables() {
        let rl = RateLimiter::new(RateLimitConfig {
            capacity: 5,
            refill_per_sec: 0.0001,
        });
        assert!(matches!(
            rl.check_with_capacity("t", "x", Some(0)).await,
            CheckOutcome::Disabled
        ));
    }
}
