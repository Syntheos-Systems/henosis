//! Quota tiers, dimension taxonomy, and outcome types for the Plutus usage engine.
//!
//! `QuotaTier` maps to a `QuotaConfig` of daily limits and an RPM cap. The store
//! enforces them atomically; this module owns only the pure data types and defaults.

use std::fmt;

/// The billing/quota tier for an org.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    /// Free tier: minimal resource allowances.
    Free,
    /// Pro tier: single-user or small team.
    Pro,
    /// Team tier: larger org with elevated limits.
    Team,
    /// Enterprise tier: high-sentinel limits; practically unlimited.
    Enterprise,
}

/// Display the tier as its canonical lowercase text (matches the DB column value).
impl fmt::Display for QuotaTier {
    /// Write the lowercase tier name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse errors for `QuotaTier`.
#[derive(Debug)]
pub struct TierParseError(String);

/// Display the unrecognized tier string.
impl fmt::Display for TierParseError {
    /// Write the error.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown quota tier: {:?}", self.0)
    }
}

/// Parse a `QuotaTier` from its canonical text form.
impl std::str::FromStr for QuotaTier {
    /// Tier parse error.
    type Err = TierParseError;

    /// Parse the canonical lowercase tier name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "free" => Ok(QuotaTier::Free),
            "pro" => Ok(QuotaTier::Pro),
            "team" => Ok(QuotaTier::Team),
            "enterprise" => Ok(QuotaTier::Enterprise),
            other => Err(TierParseError(other.to_string())),
        }
    }
}

/// `QuotaTier` methods.
impl QuotaTier {
    /// Return the canonical text representation stored in the DB `plan_tier` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            QuotaTier::Free => "free",
            QuotaTier::Pro => "pro",
            QuotaTier::Team => "team",
            QuotaTier::Enterprise => "enterprise",
        }
    }

    /// Return the spec-defined default quota configuration for this tier.
    ///
    /// Free:       10 tasks, 100_000 tokens, 50 tool_calls, 100 memory_stores, 10 rpm.
    /// Pro:        100 tasks, 1_000_000 tokens, 500 tool_calls, 1_000 memory_stores, 60 rpm.
    /// Team:       1_000 tasks, 10_000_000 tokens, 5_000 tool_calls, 10_000 memory_stores, 300 rpm.
    /// Enterprise: i64::MAX sentinel (effectively unlimited).
    pub fn defaults(self) -> QuotaConfig {
        match self {
            QuotaTier::Free => QuotaConfig {
                max_tasks_per_day: 10,
                max_tokens_per_day: 100_000,
                max_tool_calls_per_day: 50,
                max_memory_stores_per_day: 100,
                rate_limit_rpm: 10,
            },
            QuotaTier::Pro => QuotaConfig {
                max_tasks_per_day: 100,
                max_tokens_per_day: 1_000_000,
                max_tool_calls_per_day: 500,
                max_memory_stores_per_day: 1_000,
                rate_limit_rpm: 60,
            },
            QuotaTier::Team => QuotaConfig {
                max_tasks_per_day: 1_000,
                max_tokens_per_day: 10_000_000,
                max_tool_calls_per_day: 5_000,
                max_memory_stores_per_day: 10_000,
                rate_limit_rpm: 300,
            },
            QuotaTier::Enterprise => QuotaConfig {
                max_tasks_per_day: i64::MAX,
                max_tokens_per_day: i64::MAX,
                max_tool_calls_per_day: i64::MAX,
                max_memory_stores_per_day: i64::MAX,
                rate_limit_rpm: 3_600,
            },
        }
    }
}

/// Daily quota limits and rate-limit configuration for one org.
///
/// Stored in the `quota_config` table; populated from `QuotaTier::defaults` on org creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaConfig {
    /// Maximum Chiasm tasks the org may submit per day.
    pub max_tasks_per_day: i64,
    /// Maximum LLM tokens consumed per day across all agents.
    pub max_tokens_per_day: i64,
    /// Maximum tool invocations per day.
    pub max_tool_calls_per_day: i64,
    /// Maximum memory store operations per day.
    pub max_memory_stores_per_day: i64,
    /// Token-bucket refill rate in requests per minute.
    pub rate_limit_rpm: i64,
}

/// A countable resource dimension tracked in `usage_counter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDimension {
    /// Chiasm task submissions.
    Tasks,
    /// LLM token consumption.
    Tokens,
    /// Tool invocations.
    ToolCalls,
    /// Memory store operations.
    MemoryStores,
}

/// `QuotaDimension` methods.
impl QuotaDimension {
    /// Return the stable string stored in `usage_counter.dimension`.
    pub fn as_str(&self) -> &'static str {
        match self {
            QuotaDimension::Tasks => "tasks",
            QuotaDimension::Tokens => "tokens",
            QuotaDimension::ToolCalls => "tool_calls",
            QuotaDimension::MemoryStores => "memory_stores",
        }
    }

    /// Return the `quota_config` column limit for this dimension.
    ///
    /// Used by `PlutusStore::check_and_increment` to resolve the applicable cap.
    pub fn limit_from_config(&self, cfg: &QuotaConfig) -> i64 {
        match self {
            QuotaDimension::Tasks => cfg.max_tasks_per_day,
            QuotaDimension::Tokens => cfg.max_tokens_per_day,
            QuotaDimension::ToolCalls => cfg.max_tool_calls_per_day,
            QuotaDimension::MemoryStores => cfg.max_memory_stores_per_day,
        }
    }
}

/// Display the dimension as its stable string.
impl fmt::Display for QuotaDimension {
    /// Write the dimension string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of a `check_and_increment` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaOutcome {
    /// Whether the request is within quota (false means the caller should deny).
    pub allowed: bool,
    /// The new cumulative usage count after this increment (even if denied).
    pub used: i64,
    /// The configured daily limit for this dimension.
    pub limit: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Free tier tokens-per-day default matches the spec value.
    #[test]
    fn free_tier_tokens_default() {
        assert_eq!(QuotaTier::Free.defaults().max_tokens_per_day, 100_000);
    }

    /// Free tier task and rpm defaults are non-zero and reasonable.
    #[test]
    fn free_tier_task_and_rpm_defaults() {
        let cfg = QuotaTier::Free.defaults();
        assert_eq!(cfg.max_tasks_per_day, 10);
        assert_eq!(cfg.rate_limit_rpm, 10);
    }

    /// Enterprise sentinel values are i64::MAX (effectively unlimited).
    #[test]
    fn enterprise_tier_is_sentinel() {
        let cfg = QuotaTier::Enterprise.defaults();
        assert_eq!(cfg.max_tasks_per_day, i64::MAX);
        assert_eq!(cfg.max_tokens_per_day, i64::MAX);
    }

    /// Dimension as_str values are stable.
    #[test]
    fn dimension_strings_stable() {
        assert_eq!(QuotaDimension::Tasks.as_str(), "tasks");
        assert_eq!(QuotaDimension::Tokens.as_str(), "tokens");
        assert_eq!(QuotaDimension::ToolCalls.as_str(), "tool_calls");
        assert_eq!(QuotaDimension::MemoryStores.as_str(), "memory_stores");
    }

    /// Tier round-trips through its text form.
    #[test]
    fn tier_roundtrip() {
        for tier in [QuotaTier::Free, QuotaTier::Pro, QuotaTier::Team, QuotaTier::Enterprise] {
            let s = tier.to_string();
            let back: QuotaTier = s.parse().expect("valid tier");
            assert_eq!(tier, back);
        }
    }
}
