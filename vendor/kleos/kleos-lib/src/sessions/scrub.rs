//! Secret-aware session content scrubbing with bounded fallback caching.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::cred::PhylaxdClient;
use crate::db::Database;
use crate::gate::scrub_output;
use crate::Result;
use tracing::warn;

#[derive(Debug, Clone)]
/// Stores a cached secret list and the time it was loaded.
struct CachedSecrets {
    secrets: Vec<String>,
    loaded_at: Instant,
}

/// Returns the process-wide cache of per-authority scrub secret lists.
fn scrub_cache() -> &'static Mutex<HashMap<String, CachedSecrets>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedSecrets>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Scrubs known secrets from a message according to configured failure policy.
#[tracing::instrument(skip(db, phylaxd, agent, message), fields(message_len = message.len()))]
pub async fn scrub_message(
    db: &Database,
    phylaxd: &PhylaxdClient,
    user_id: i64,
    agent: &str,
    message: &str,
) -> Result<String> {
    let config = Config::from_env();
    if !config.eidolon.sessions.scrub_secrets {
        return Ok(message.to_string());
    }

    let secrets = match load_scrub_secrets(
        db,
        user_id,
        agent,
        phylaxd,
        Duration::from_secs(config.eidolon.phylaxd.cache_ttl_secs.max(1)),
    )
    .await
    {
        Ok(secrets) => secrets,
        Err(err) => {
            // The secret list could not be loaded AND there was no cached list
            // to fall back on (load_scrub_secrets already tries the stale cache
            // on a phylaxd fault). This is the cold-cache case. Fail open or
            // closed per policy: open (default) keeps message writes working
            // without a hard phylaxd dependency; closed refuses the write so a
            // fault cannot persist an unscrubbed secret.
            if config.eidolon.sessions.scrub_fail_open {
                warn!(agent = %agent, user_id, error = %err, "session_secret_scrub_fail_open");
                return Ok(message.to_string());
            }
            warn!(agent = %agent, user_id, error = %err, "session_secret_scrub_fail_closed");
            return Err(crate::EngError::Internal(
                "secret scrubbing is enabled but the secret list could not be loaded \
                 and no cached list is available; message rejected to avoid persisting \
                 unscrubbed content (set EIDOLON_SESSIONS_SCRUB_FAIL_OPEN=1 to allow)"
                    .into(),
            ));
        }
    };
    Ok(apply_scrub(message, &secrets))
}

/// Replaces every known secret in a message with a redaction marker.
pub fn apply_scrub(message: &str, secrets: &[String]) -> String {
    scrub_output(message, secrets)
}

/// Loads scrub secrets from the authority or a valid cached fallback.
async fn load_scrub_secrets(
    db: &Database,
    user_id: i64,
    agent: &str,
    phylaxd: &PhylaxdClient,
    ttl: Duration,
) -> Result<Vec<String>> {
    // ROBUSTNESS: include the phylaxd base URL in the cache key. The cache
    // is a process-wide static, and tests (plus prod if phylaxd is ever
    // reconfigured) can create multiple PhylaxdClient instances pointed at
    // different upstreams. Without the URL in the key, the first client
    // to resolve a (user_id, agent) pair poisons the cache for every
    // other client.
    let cache_key = format!(
        "scrub:{}:{user_id}:{agent}:{}",
        phylaxd.base_url(),
        ttl.as_secs()
    );
    if let Some(cached) = cached_scrub_secrets(&cache_key, ttl) {
        return Ok(cached);
    }

    let secrets = match phylaxd.list_secret_values(db, user_id, agent).await {
        Ok(secrets) => secrets,
        Err(err) => {
            // phylaxd is unavailable: fall back to the last-known secret list,
            // ignoring its TTL, so a transient outage still scrubs known
            // secrets instead of leaking them. The caller only applies its
            // fail-open/closed policy when there is no cached list at all.
            if let Some(stale) = stale_cached_scrub_secrets(&cache_key) {
                warn!(
                    agent = %agent,
                    user_id,
                    error = %err,
                    "session_secret_scrub_stale_cache_fallback"
                );
                return Ok(stale);
            }
            return Err(err);
        }
    };
    let mut cache = scrub_cache().lock().expect("scrub cache mutex poisoned");
    cache.insert(
        cache_key,
        CachedSecrets {
            secrets: secrets.clone(),
            loaded_at: Instant::now(),
        },
    );
    Ok(secrets)
}

/// Last-known secret list for a key, ignoring TTL. Used as a fail-safe fallback
/// when phylaxd is unavailable so a transient outage does not drop scrubbing.
fn stale_cached_scrub_secrets(cache_key: &str) -> Option<Vec<String>> {
    let cache = scrub_cache().lock().expect("scrub cache mutex poisoned");
    cache.get(cache_key).map(|entry| entry.secrets.clone())
}

/// Returns a fresh cached scrub-secret list for the requested key.
fn cached_scrub_secrets(cache_key: &str, ttl: Duration) -> Option<Vec<String>> {
    let cache = scrub_cache().lock().expect("scrub cache mutex poisoned");
    let entry = cache.get(cache_key)?;
    if entry.loaded_at.elapsed() > ttl {
        return None;
    }
    Some(entry.secrets.clone())
}

#[cfg(test)]
/// Clears the scrub cache for deterministic tests.
pub(crate) fn reset_scrub_cache() {
    let mut cache = scrub_cache().lock().expect("scrub cache mutex poisoned");
    cache.clear();
}

#[cfg(test)]
/// Tests message scrubbing behavior.
mod tests {
    use super::{apply_scrub, reset_scrub_cache};

    #[test]
    /// Verifies known secret text is redacted.
    fn known_secret_is_redacted() {
        reset_scrub_cache();
        let result = apply_scrub(
            "The key is alpha-secret and should vanish.",
            &["alpha-secret".to_string()],
        );
        assert_eq!(result, "The key is [REDACTED] and should vanish.");
    }

    #[test]
    /// Verifies clean text remains unchanged.
    fn clean_message_is_preserved() {
        reset_scrub_cache();
        let input = "harmless session line";
        assert_eq!(apply_scrub(input, &["alpha-secret".to_string()]), input);
    }

    #[test]
    /// Verifies unrelated random text remains unchanged.
    fn unrelated_random_text_is_preserved() {
        reset_scrub_cache();
        let corpus = [
            "fj3k2l9 test payload",
            "http://example.com/path?x=1",
            "notes about blocked-domain.com but no secret",
            "uuid 123e4567-e89b-12d3-a456-426614174000",
        ];
        for sample in corpus {
            assert_eq!(apply_scrub(sample, &["alpha-secret".to_string()]), sample);
        }
    }

    // A TTL-expired cache entry is no longer served as a fresh hit, but the
    // stale fallback still returns it so a phylaxd outage keeps scrubbing known
    // secrets instead of leaking them.
    #[test]
    fn stale_cache_survives_ttl_expiry() {
        use super::{cached_scrub_secrets, scrub_cache, stale_cached_scrub_secrets, CachedSecrets};
        use std::time::{Duration, Instant};

        reset_scrub_cache();
        let key = "scrub:test:1:agent:60";
        scrub_cache().lock().unwrap().insert(
            key.to_string(),
            CachedSecrets {
                secrets: vec!["sekret".to_string()],
                loaded_at: Instant::now(),
            },
        );

        // Zero TTL => the fresh lookup treats the entry as expired...
        assert!(cached_scrub_secrets(key, Duration::ZERO).is_none());
        // ...but the stale fallback still returns the last-known list.
        assert_eq!(
            stale_cached_scrub_secrets(key),
            Some(vec!["sekret".to_string()])
        );
        reset_scrub_cache();
    }
}
