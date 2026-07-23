//! Per-tenant adapter configuration.
//!
//! Operators can enable/disable a provider per tenant, override its rate limit,
//! and inject default args merged into every invocation. Configuration lives in
//! memory keyed by `(tenant, provider)`; a default (enabled, no overrides)
//! applies to any pair without an explicit entry.
//!
//! Durable persistence is a local JSON file, loaded on startup and rewritten on
//! every mutation. A file store supports exact configuration retrieval without
//! depending on a semantic memory index.
//! `enabled`, `default_args`, and `rate_limit_override` are all enforced today.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

/// Per-(tenant, provider) adapter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantAdapterConfig {
    /// Whether this provider is enabled for the tenant. Default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Enforced per-minute rate-limit override.
    #[serde(default)]
    pub rate_limit_override: Option<u32>,
    /// Args merged (as defaults) into every invocation for this tenant+provider.
    #[serde(default)]
    pub default_args: Option<Value>,
}

/// Serde default for `enabled`.
fn default_true() -> bool {
    true
}

/// Builds the default tenant adapter configuration.
impl Default for TenantAdapterConfig {
    /// Returns the permissive default: enabled, no rate-limit override, no
    /// default args.
    fn default() -> Self {
        Self {
            enabled: true,
            rate_limit_override: None,
            default_args: None,
        }
    }
}

/// File-backed store of per-tenant adapter configuration.
#[derive(Default)]
pub struct TenantConfigStore {
    /// Keyed by `"{tenant}:{provider}"`.
    configs: RwLock<HashMap<String, TenantAdapterConfig>>,
    /// Optional JSON file the map is persisted to; `None` = in-memory only
    /// (tests).
    path: Option<PathBuf>,
}

/// Build the composite map key from a tenant ID and provider name.
fn key(tenant: &str, provider: &str) -> String {
    format!("{tenant}:{provider}")
}

/// Implements tenant configuration lookup and persistence.
impl TenantConfigStore {
    /// Construct an in-memory store (no persistence). Used by tests.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a file-backed store, loading any existing config from `path`.
    /// A missing or unreadable file starts empty (logged).
    pub fn with_path(path: PathBuf) -> Self {
        let configs = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "tenant config unreadable; starting empty");
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        Self {
            configs: RwLock::new(configs),
            path: Some(path),
        }
    }

    /// Resolve the effective config for `(tenant, provider)`, falling back to
    /// the permissive default when no entry exists.
    pub fn get(&self, tenant: &str, provider: &str) -> TenantAdapterConfig {
        self.configs
            .read()
            .expect("tenant config poisoned")
            .get(&key(tenant, provider))
            .cloned()
            .unwrap_or_default()
    }

    /// Set the full config for `(tenant, provider)`, then persist.
    pub fn set(&self, tenant: &str, provider: &str, config: TenantAdapterConfig) {
        self.configs
            .write()
            .expect("tenant config poisoned")
            .insert(key(tenant, provider), config);
        self.persist();
    }

    /// Disable a provider for a tenant, preserving any other fields, then
    /// persist.
    pub fn disable(&self, tenant: &str, provider: &str) {
        {
            let mut guard = self.configs.write().expect("tenant config poisoned");
            guard.entry(key(tenant, provider)).or_default().enabled = false;
        }
        self.persist();
    }

    /// Best-effort write of the full config map to the backing file. A failure
    /// is logged and swallowed -- a persistence fault must not fail an admin
    /// request or corrupt the in-memory state.
    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let snapshot = self.configs.read().expect("tenant config poisoned");
        let json = match serde_json::to_vec_pretty(&*snapshot) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "tenant config serialize failed");
                return;
            }
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(path, json) {
            warn!(path = %path.display(), error = %e, "tenant config persist failed");
        }
    }

    /// List every configured provider for a tenant, as `provider -> config`.
    pub fn list(&self, tenant: &str) -> HashMap<String, TenantAdapterConfig> {
        let prefix = format!("{tenant}:");
        self.configs
            .read()
            .expect("tenant config poisoned")
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix(&prefix)
                    .map(|provider| (provider.to_string(), v.clone()))
            })
            .collect()
    }
}

/// Merge `default_args` (defaults) under `request_args` (overrides), returning a
/// new object. Request values win on key collisions. Non-object inputs degrade
/// gracefully: the request args pass through unchanged when there are no object
/// defaults to merge.
pub fn merge_default_args(default_args: Option<&Value>, request_args: Value) -> Value {
    let Some(Value::Object(defaults)) = default_args else {
        return request_args;
    };
    let Value::Object(req) = &request_args else {
        return request_args;
    };
    let mut merged = defaults.clone();
    for (k, v) in req {
        merged.insert(k.clone(), v.clone());
    }
    Value::Object(merged)
}

#[cfg(test)]
/// Tests tenant configuration defaults and durable updates.
mod tests {
    use super::*;
    use serde_json::json;

    /// An unconfigured pair resolves to the permissive default.
    #[test]
    fn default_is_enabled() {
        let store = TenantConfigStore::new();
        let cfg = store.get("acme", "github");
        assert!(cfg.enabled);
        assert!(cfg.rate_limit_override.is_none());
    }

    /// Disable then re-read reflects the change and is tenant+provider scoped.
    #[test]
    fn disable_scopes_to_tenant_provider() {
        let store = TenantConfigStore::new();
        store.disable("acme", "github");
        assert!(!store.get("acme", "github").enabled);
        // Other tenant and other provider are unaffected.
        assert!(store.get("globex", "github").enabled);
        assert!(store.get("acme", "slack").enabled);
    }

    /// Set then list returns the configured providers for the tenant only.
    #[test]
    fn set_and_list() {
        let store = TenantConfigStore::new();
        store.set(
            "acme",
            "github",
            TenantAdapterConfig {
                enabled: true,
                rate_limit_override: Some(120),
                default_args: Some(json!({"org": "acme-inc"})),
            },
        );
        store.set("globex", "slack", TenantAdapterConfig::default());

        let acme = store.list("acme");
        assert_eq!(acme.len(), 1);
        assert_eq!(acme["github"].rate_limit_override, Some(120));
        assert!(!store.list("globex").contains_key("github"));
    }

    /// Request args override defaults; defaults fill the gaps.
    #[test]
    fn default_args_merge() {
        let defaults = json!({"org": "acme-inc", "visibility": "private"});
        let req = json!({"visibility": "public", "title": "bug"});
        let merged = merge_default_args(Some(&defaults), req);
        assert_eq!(merged["org"], "acme-inc"); // filled from default
        assert_eq!(merged["visibility"], "public"); // request wins
        assert_eq!(merged["title"], "bug"); // request-only
    }

    /// No object defaults -> request passes through unchanged.
    #[test]
    fn merge_passthrough_without_defaults() {
        let req = json!({"a": 1});
        assert_eq!(merge_default_args(None, req.clone()), req);
    }

    /// Config written by one store instance is reloaded by the next from the
    /// same file (durable across "restarts").
    #[test]
    fn persists_and_reloads_from_disk() {
        let dir = std::env::temp_dir();
        // A unique-ish path without relying on rng: nanos since epoch.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = dir.join(format!("hermes-tc-test-{nanos}.json"));

        {
            let store = TenantConfigStore::with_path(path.clone());
            store.set(
                "acme",
                "github",
                TenantAdapterConfig {
                    enabled: false,
                    rate_limit_override: Some(30),
                    default_args: Some(json!({"org": "acme-inc"})),
                },
            );
        }
        // A fresh store over the same file sees the persisted config.
        let reloaded = TenantConfigStore::with_path(path.clone());
        let cfg = reloaded.get("acme", "github");
        assert!(!cfg.enabled);
        assert_eq!(cfg.rate_limit_override, Some(30));
        assert_eq!(cfg.default_args, Some(json!({"org": "acme-inc"})));

        let _ = std::fs::remove_file(&path);
    }
}
