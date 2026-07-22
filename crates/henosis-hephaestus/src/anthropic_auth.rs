//! Anthropic OAuth credential providers. The `ProviderChain` tries Plutus
//! (for multi-tenant prod) and falls back to a local credentials file (for
//! dev). The chain is cloneable so the provider factory can hand an owned
//! copy into each provider instance.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use tracing::debug;

use crate::config::{Config, DeployEnv};

/// First system block required by the Anthropic OAuth contract. Missing it
/// returns 429 with a "claude code" identity complaint. Not negotiable.
pub const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Errors that can occur while resolving an Anthropic bearer token.
#[derive(Debug, Error)]
pub enum AuthError {
    /// No configured provider matched the given tenant and environment.
    #[error("no token provider matched (tenant_id={tenant_id:?}, env={env:?})")]
    NoProvider {
        /// Tenant that was requested.
        tenant_id: Option<String>,
        /// Deployment environment at the time of failure.
        env: DeployEnv,
    },
    /// Plutus is configured but could not produce a token.
    #[error("plutus not available: {0}")]
    PlutusUnavailable(String),
    /// The dev credentials file could not be read.
    #[error("credentials file unreadable ({path}): {source}")]
    CredentialsFileUnreadable {
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// The dev credentials file exists but its JSON is not the expected shape.
    #[error("credentials file malformed ({path}): {reason}")]
    CredentialsFileMalformed {
        /// Path that was attempted.
        path: PathBuf,
        /// Human-readable description of the parse failure.
        reason: String,
    },
}

/// Trait for anything that can resolve an Anthropic bearer token for a given
/// (optional) tenant. Implementations are composited into a `ProviderChain`.
#[async_trait]
pub trait AnthropicTokenProvider: Send + Sync {
    /// Resolve a bearer token, optionally scoped to `tenant_id`.
    async fn token(&self, tenant_id: Option<&str>) -> Result<String, AuthError>;
}

/// Stub: will talk to Plutus once Phase 4 ships. For now always returns
/// PlutusUnavailable so the chain falls through to dev.
pub struct PlutusTokenProvider {
    /// Base URL of the Plutus credential service.
    pub base_url: String,
}

#[async_trait]
/// Provides the placeholder Plutus token lookup used by the provider chain.
impl AnthropicTokenProvider for PlutusTokenProvider {
    /// Always returns `PlutusUnavailable` -- Phase 4 stub not yet implemented.
    async fn token(&self, _tenant_id: Option<&str>) -> Result<String, AuthError> {
        Err(AuthError::PlutusUnavailable(format!(
            "plutus stub at {} (Phase 4 not implemented)",
            self.base_url
        )))
    }
}

/// Reads ~/.claude/.credentials.json and returns claudeAiOauth.accessToken.
/// Only used when HEPHAESTUS_ENV=dev.
pub struct CredentialsFileProvider {
    /// Absolute path to the local Claude credentials JSON file.
    pub path: PathBuf,
}

/// Raw deserialization target for the Claude credentials file. Only the
/// `claudeAiOauth.accessToken` field is needed.
#[derive(Debug, Deserialize)]
struct CredentialsFile {
    /// Outer wrapper object present in the Claude credentials file.
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeAiOauth,
}

/// Inner OAuth block within the credentials file.
#[derive(Debug, Deserialize)]
struct ClaudeAiOauth {
    /// Bearer token for Anthropic Messages API requests.
    #[serde(rename = "accessToken")]
    access_token: String,
}

#[async_trait]
/// Loads Anthropic OAuth tokens from the configured local credentials file.
impl AnthropicTokenProvider for CredentialsFileProvider {
    /// Load and parse the credentials file, returning the embedded access
    /// token. Returns an `AuthError` if the file is missing or malformed.
    async fn token(&self, _tenant_id: Option<&str>) -> Result<String, AuthError> {
        let bytes = tokio::fs::read(&self.path).await.map_err(|e| {
            AuthError::CredentialsFileUnreadable {
                path: self.path.clone(),
                source: e,
            }
        })?;
        let parsed: CredentialsFile =
            serde_json::from_slice(&bytes).map_err(|e| AuthError::CredentialsFileMalformed {
                path: self.path.clone(),
                reason: e.to_string(),
            })?;
        debug!(path = %self.path.display(), "loaded dev oauth token");
        Ok(parsed.claude_ai_oauth.access_token)
    }
}

/// Provider chain: try Plutus first when tenant_id is set and PLUTUS_URL is
/// configured; otherwise fall back to the dev credentials file if env=dev.
///
/// Cloneable so the provider factory can pass an owned chain into each
/// provider instance without forcing every caller to thread an `Arc`. The
/// underlying providers are already `Arc`-wrapped, so cloning is cheap.
#[derive(Clone)]
pub struct ProviderChain {
    /// Optional Plutus provider, present when `PLUTUS_URL` is configured.
    plutus: Option<Arc<PlutusTokenProvider>>,
    /// Optional dev-credentials provider, present when `env == Dev`.
    dev: Option<Arc<CredentialsFileProvider>>,
    /// Deployment environment, used for error reporting.
    env: DeployEnv,
}

/// Builds and resolves the ordered Anthropic token provider chain.
impl ProviderChain {
    /// Construct a chain from a loaded `Config`. Enables Plutus when
    /// `PLUTUS_URL` is set; enables the credentials-file path when
    /// `HEPHAESTUS_ENV=dev`.
    pub fn from_config(cfg: &Config) -> Self {
        let plutus = cfg.plutus_url.as_ref().map(|u| {
            Arc::new(PlutusTokenProvider {
                base_url: u.clone(),
            })
        });
        let dev = matches!(cfg.env, DeployEnv::Dev).then(|| {
            Arc::new(CredentialsFileProvider {
                path: cfg.dev_credentials_path.clone(),
            })
        });
        Self {
            plutus,
            dev,
            env: cfg.env,
        }
    }

    /// Resolve a token. Tries Plutus first for tenant-scoped requests when
    /// configured; falls back to the dev credentials file; returns
    /// `AuthError::NoProvider` if no provider matched.
    pub async fn token(&self, tenant_id: Option<&str>) -> Result<String, AuthError> {
        if tenant_id.is_some() {
            if let Some(p) = &self.plutus {
                match p.token(tenant_id).await {
                    Ok(t) => return Ok(t),
                    Err(e) if self.dev.is_none() => return Err(e),
                    Err(_) => {}
                }
            } else if matches!(self.env, DeployEnv::Prod) {
                return Err(AuthError::PlutusUnavailable(
                    "PLUTUS_URL not configured".into(),
                ));
            }
        }

        if let Some(d) = &self.dev {
            return d.token(tenant_id).await;
        }

        Err(AuthError::NoProvider {
            tenant_id: tenant_id.map(String::from),
            env: self.env,
        })
    }
}
