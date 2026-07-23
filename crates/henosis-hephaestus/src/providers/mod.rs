//! Provider factory. The orchestrator depends on `Arc<dyn Provider>` and
//! never knows which concrete type it has; this module is the only place
//! Hephaestus picks one based on configuration.
//!
//! Layout: each concrete provider lives in its own submodule. The factory
//! reads `Config::provider_kind` (set from `HEPHAESTUS_PROVIDER`) and
//! returns the right wrapper.

pub mod anthropic;
pub mod openai_compat;

use std::sync::Arc;

use anyhow::{Result, anyhow};
use reqwest::Client;
use tracing::warn;

use crate::anthropic_auth::ProviderChain;
use crate::config::{Config, ProviderKind};
use crate::provider::Provider;
use crate::services::Services;

pub use anthropic::HephaestusAnthropicProvider;
pub use openai_compat::HephaestusProxyProvider;

/// Construct the active provider for a given config + auth chain. Returns
/// `Arc<dyn Provider>` so the orchestrator can hold the provider behind a
/// trait object across .await points.
///
/// `tenant_id` is forwarded to the Anthropic path's auth chain. For
/// OpenAI-compatible providers the tenant id is currently ignored; per-tenant
/// keying is delegated to the configured credential authority.
///
/// The reqwest client passed in is the same shared instance held by `Services`
/// so connection pools stay unified across LLM, Hermes, and coordination
/// calls.
pub async fn build_provider(
    cfg: &Config,
    auth: ProviderChain,
    http: Client,
    services: &Services,
    tenant_id: Option<String>,
) -> Result<Arc<dyn Provider>> {
    match cfg.provider_kind {
        ProviderKind::Anthropic => {
            let url = cfg
                .provider_url
                .clone()
                .unwrap_or_else(|| cfg.anthropic_url.clone());
            Ok(Arc::new(HephaestusAnthropicProvider::new(
                http,
                auth,
                url,
                cfg.model.clone(),
                tenant_id,
            )))
        }
        ProviderKind::OpenAiCompat => {
            let base_url = cfg.provider_url.clone().ok_or_else(|| {
                anyhow!(
                    "HEPHAESTUS_PROVIDER=openai requires HEPHAESTUS_PROVIDER_URL to be set \
                     to the upstream base URL (e.g. https://api.openai.com/v1)"
                )
            })?;
            let api_key = resolve_openai_key(cfg, services).await?;
            Ok(Arc::new(HephaestusProxyProvider::new(
                http,
                base_url,
                api_key,
                "hephaestus-openai-compat",
            )))
        }
    }
}

/// Resolve the OpenAI-compatible API key from the configured sources. The env
/// var `HEPHAESTUS_PROVIDER_KEY` wins over the phylaxd slot, so tests and local
/// dev can override without touching phylaxd. If neither resolves, return a
/// clear error so server startup fails fast rather than at first request.
async fn resolve_openai_key(cfg: &Config, services: &Services) -> Result<String> {
    if let Some(k) = cfg.provider_api_key.as_deref()
        && !k.is_empty()
    {
        return Ok(k.to_string());
    }
    if let Some(slot) = cfg.provider_key_slot.as_deref()
        && let Some(k) = services.cred_get(slot).await
    {
        return Ok(k);
    }
    warn!(
        "HEPHAESTUS_PROVIDER=openai but no key resolved (HEPHAESTUS_PROVIDER_KEY \
         or HEPHAESTUS_PROVIDER_KEY_SLOT). Falling back to empty key -- requests \
         will likely 401."
    );
    // Returning an empty key (rather than erroring) preserves the pre-refactor
    // best-effort posture: the server still starts so other endpoints work,
    // and the first task that needs the provider will surface the auth error
    // through the normal failure path.
    Ok(String::new())
}
