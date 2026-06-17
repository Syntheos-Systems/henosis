//! Cost telemetry: pricing tables and per-call cost calculation.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bundled pricing defaults (OpenCode Zen GO tier, Anthropic, OpenAI, Ollama).
const BUNDLED_PRICING: &str = include_str!("pricing.json");

/// Per-model pricing in USD per million tokens.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelPricing {
    /// USD per 1M input tokens.
    pub input: f64,
    /// USD per 1M output tokens.
    pub output: f64,
    /// USD per 1M cache-read tokens. Defaults to `input * 0.1` when absent.
    #[serde(default)]
    pub cache_read: Option<f64>,
    /// USD per 1M cache-write (creation) tokens. Defaults to `input * 1.25` when absent.
    #[serde(default)]
    pub cache_write: Option<f64>,
}

/// Pricing table keyed by model ID.
#[derive(Debug, Clone)]
pub struct PricingTable {
    models: HashMap<String, ModelPricing>,
    /// Fallback pricing when model not found.
    default: ModelPricing,
}

impl Default for PricingTable {
    fn default() -> Self {
        Self::load()
    }
}

impl PricingTable {
    /// Load pricing from bundled defaults, then merge user overrides from
    /// `~/.synapse/pricing.json` if it exists.
    pub fn load() -> Self {
        let mut models: HashMap<String, ModelPricing> =
            serde_json::from_str(BUNDLED_PRICING).unwrap_or_default();

        // Merge user overrides
        if let Some(user_path) = Self::user_pricing_path()
            && let Ok(data) = std::fs::read_to_string(&user_path)
            && let Ok(user_models) = serde_json::from_str::<HashMap<String, ModelPricing>>(&data)
        {
            log::debug!(
                "loaded {} user pricing overrides from {}",
                user_models.len(),
                user_path.display()
            );
            models.extend(user_models);
        }

        Self {
            models,
            default: ModelPricing {
                input: 3.0,
                output: 15.0,
                cache_read: None,
                cache_write: None,
            }, // Conservative fallback
        }
    }

    fn user_pricing_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".synapse").join("pricing.json"))
    }

    /// Calculate cost in USD for a single call.
    ///
    /// `input_tokens` is the total input (including cache reads and writes);
    /// `cache_read_tokens` and `cache_write_tokens` are the subsets billed at the
    /// cache rates, and the remainder is billed at the full input rate.
    pub fn cost(
        &self,
        model: &str,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    ) -> f64 {
        let pricing = self.pricing_for(model);
        let read_rate = pricing.cache_read.unwrap_or(pricing.input * 0.1);
        let write_rate = pricing.cache_write.unwrap_or(pricing.input * 1.25);
        let full_input = input_tokens
            .saturating_sub(cache_read_tokens)
            .saturating_sub(cache_write_tokens);
        (full_input as f64 * pricing.input
            + cache_read_tokens as f64 * read_rate
            + cache_write_tokens as f64 * write_rate
            + output_tokens as f64 * pricing.output)
            / 1_000_000.0
    }

    /// Get pricing for a model, with prefix matching for versioned model IDs.
    pub fn pricing_for(&self, model: &str) -> &ModelPricing {
        // Exact match first
        if let Some(p) = self.models.get(model) {
            return p;
        }
        // Prefix match, longest (most specific) prefix wins so that
        // "claude-haiku-4-5-20250514" resolves to "claude-haiku-4-5" rather
        // than "claude-haiku-4" regardless of HashMap iteration order.
        self.models
            .iter()
            .filter(|(key, _)| model.starts_with(key.as_str()))
            .max_by_key(|(key, _)| key.len())
            .map(|(_, pricing)| pricing)
            .unwrap_or(&self.default)
    }

    /// Check if a model has explicit pricing (not fallback).
    pub fn has_pricing(&self, model: &str) -> bool {
        self.models.contains_key(model) || self.models.keys().any(|k| model.starts_with(k))
    }
}

/// Per-model accumulated usage and cost.
#[derive(Debug, Clone, Default)]
pub struct ModelCost {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub usd: f64,
}

/// Session cost accumulator. Thread-safe via interior mutability not needed
/// since ConversationContext already uses Mutex.
#[derive(Debug, Clone, Default)]
pub struct SessionCost {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_write_tokens: u64,
    pub total_usd: f64,
    /// Per-model breakdown.
    pub by_model: HashMap<String, ModelCost>,
}

impl SessionCost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a turn's usage and cost.
    pub fn record(
        &mut self,
        model: &str,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
        usd: f64,
    ) {
        self.total_input_tokens += input_tokens as u64;
        self.total_output_tokens += output_tokens as u64;
        self.total_cache_read_tokens += cache_read_tokens as u64;
        self.total_cache_write_tokens += cache_write_tokens as u64;
        self.total_usd += usd;

        let entry = self.by_model.entry(model.to_string()).or_default();
        entry.input_tokens += input_tokens as u64;
        entry.output_tokens += output_tokens as u64;
        entry.cache_read_tokens += cache_read_tokens as u64;
        entry.cache_write_tokens += cache_write_tokens as u64;
        entry.usd += usd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pricing_load() {
        let table = PricingTable::load();
        // Should have bundled models
        assert!(table.has_pricing("kimi-k2.5"));
    }

    #[test]
    fn test_cost_calculation() {
        let table = PricingTable::load();
        // kimi-k2.5: $0.14/M in, $0.28/M out
        let cost = table.cost("kimi-k2.5", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 0.42).abs() < 0.001);
    }

    #[test]
    fn cost_prices_cache_reads_and_writes() {
        let table = PricingTable::load();
        // claude-sonnet-4-6: input 3.00, output 15.00; cache_read 0.30, cache_write 3.75.
        // total input 1,000,000 = 100k full + 800k cache-read + 100k cache-write; output 100k.
        let cost = table.cost("claude-sonnet-4-6", 1_000_000, 100_000, 800_000, 100_000);
        // 0.1*3.00 + 0.8*0.30 + 0.1*3.75 + 0.1*15.00 = 0.30 + 0.24 + 0.375 + 1.50 = 2.415
        assert!((cost - 2.415).abs() < 0.001, "got {cost}");
    }

    #[test]
    fn cost_with_zero_cache_matches_legacy() {
        let table = PricingTable::load();
        // kimi-k2.5: 0.14 in, 0.28 out -> 0.42 for 1M/1M, unchanged from before.
        let cost = table.cost("kimi-k2.5", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 0.42).abs() < 0.001, "got {cost}");
    }

    #[test]
    fn test_prefix_matching() {
        let table = PricingTable::load();
        // "claude-sonnet-4-6-20260501" should match "claude-sonnet-4-6"
        assert!(table.has_pricing("claude-sonnet-4-6-20260501"));
    }

    #[test]
    fn pricing_for_prefers_longest_prefix() {
        let table = PricingTable::load();
        // claude-haiku-4-5 (cache_read 0.10) must win over claude-haiku-4 (cache_read 0.08)
        let p = table.pricing_for("claude-haiku-4-5-20250514");
        assert_eq!(p.cache_read, Some(0.10));
    }
}
