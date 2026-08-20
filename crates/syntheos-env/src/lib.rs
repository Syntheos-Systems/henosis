//! # syntheos-env
//!
//! Environment resolution for the repository rename boundary.
//!
//! `SYNTHEOS_` is the permanent technical environment prefix. The legacy
//! product-brand prefix is kept as a read-only compatibility alias so existing
//! deployments keep working through a documented window. This crate is the
//! single place that implements that rule:
//!
//! - [`resolve`] returns the canonical `SYNTHEOS_<SUFFIX>` value when set,
//!   otherwise the legacy alias value, and fails closed when both are set with
//!   different values.
//! - Legacy usage is recorded process-wide so each binary can emit one
//!   structured deprecation event at configuration load via
//!   [`emit_deprecations`], never per request.
//!
//! Stage 2 of the rename-readiness plan owns this contract. New code must not
//! read the legacy prefix directly and must not introduce new legacy-prefixed
//! keys.

#![deny(missing_docs)]
#![warn(clippy::all)]

use std::collections::BTreeSet;
use std::sync::Mutex;

/// Canonical technical environment prefix. This never changes with the brand.
pub const CANONICAL_PREFIX: &str = "SYNTHEOS_";

/// Legacy brand environment prefix retained as read-only compatibility aliases.
pub const LEGACY_PREFIX: &str = "HENOSIS_";

/// Recorded legacy keys plus the emission latch, shared across the process.
static COLLECTOR: Collector = Collector::new();

/// Errors produced by rename-boundary environment resolution.
///
/// The error never carries environment values, only key names, because
/// resolved keys can hold secrets such as JWT material.
#[derive(Debug, PartialEq, Eq)]
pub enum EnvError {
    /// Canonical and legacy keys are both set with different values.
    ///
    /// Resolution fails closed instead of silently choosing one side. The
    /// operator must unset or align one of the two keys.
    Conflict {
        /// The canonical `SYNTHEOS_*` key name.
        canonical_key: String,
        /// The legacy brand alias key name.
        legacy_key: String,
    },
    /// The named key is present but its value is not valid Unicode.
    NonUnicode {
        /// The offending key name.
        key: String,
    },
}

/// Render the conflict and non-Unicode failures as actionable operator text.
impl std::fmt::Display for EnvError {
    /// Format one resolution failure, naming keys but never values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvError::Conflict {
                canonical_key,
                legacy_key,
            } => write!(
                f,
                "{canonical_key} and {legacy_key} are both set with different values; \
                 unset one of them ({canonical_key} is canonical, {legacy_key} is a \
                 read-only compatibility alias)"
            ),
            EnvError::NonUnicode { key } => {
                write!(f, "{key} is set but its value is not valid Unicode")
            }
        }
    }
}

/// Mark the resolution failures as standard errors for `?` propagation.
impl std::error::Error for EnvError {}

/// Resolve one environment suffix against the rename boundary using a
/// caller-supplied lookup.
///
/// This is the single source of truth for the stage-2 rule: `SYNTHEOS_<suffix>`
/// is canonical, `HENOSIS_<suffix>` is a read-only compatibility alias, identical
/// values on both sides resolve, and differing values fail closed with
/// [`EnvError::Conflict`]. Legacy usage is recorded so [`emit_deprecations`] can
/// report it. The `lookup` closure returns the value for one full key name, or an
/// error if the value cannot be read.
pub fn resolve_with<F>(suffix: &str, mut lookup: F) -> Result<Option<String>, EnvError>
where
    F: FnMut(&str) -> Result<Option<String>, EnvError>,
{
    let canonical = format!("{CANONICAL_PREFIX}{suffix}");
    let legacy = format!("{LEGACY_PREFIX}{suffix}");
    match (lookup(&canonical)?, lookup(&legacy)?) {
        (Some(canonical_value), Some(legacy_value)) => {
            if canonical_value == legacy_value {
                COLLECTOR.record(&legacy);
                Ok(Some(canonical_value))
            } else {
                Err(EnvError::Conflict {
                    canonical_key: canonical,
                    legacy_key: legacy,
                })
            }
        }
        (Some(canonical_value), None) => Ok(Some(canonical_value)),
        (None, Some(legacy_value)) => {
            COLLECTOR.record(&legacy);
            Ok(Some(legacy_value))
        }
        (None, None) => Ok(None),
    }
}

/// Resolve one environment suffix against the process environment.
///
/// `SYNTHEOS_<suffix>` is canonical. `HENOSIS_<suffix>` is a read-only
/// compatibility alias. When both are set, identical values resolve normally
/// and differing values fail closed with [`EnvError::Conflict`]. Non-Unicode
/// values fail closed rather than disappearing. Legacy usage is recorded so
/// [`emit_deprecations`] can report it once.
pub fn resolve(suffix: &str) -> Result<Option<String>, EnvError> {
    resolve_with(suffix, read_os)
}

/// Resolve one full canonical `SYNTHEOS_*` key name through the rename
/// boundary.
///
/// The canonical prefix is stripped to recover the suffix before delegating to
/// [`resolve`], so the legacy brand alias for the same suffix is honored and
/// conflicts fail closed. Names without the canonical prefix are treated as
/// bare suffixes.
pub fn resolve_name(canonical_key: &str) -> Result<Option<String>, EnvError> {
    let suffix = canonical_key
        .strip_prefix(CANONICAL_PREFIX)
        .unwrap_or(canonical_key);
    resolve(suffix)
}

/// Resolve one suffix or fall back to a default when neither key is set.
pub fn resolve_or(suffix: &str, default: impl Into<String>) -> Result<String, EnvError> {
    Ok(resolve(suffix)?.unwrap_or_else(|| default.into()))
}

/// Emit structured deprecation events for legacy keys recorded so far.
///
/// Returns the legacy key names covered by this emission, sorted and
/// deduplicated. Call this once after configuration load completes. Keys first
/// resolved after an emission are reported by a later call, so configuration
/// loaded in stages still produces one event per load boundary and never one
/// per request.
pub fn emit_deprecations() -> Vec<String> {
    let keys = COLLECTOR.drain();
    if !keys.is_empty() {
        tracing::warn!(
            legacy_keys = ?keys,
            "legacy HENOSIS_* environment keys in use; SYNTHEOS_* is canonical \
             and the legacy aliases will be retired after the documented window"
        );
    }
    keys
}

/// Read one environment variable as UTF-8, failing closed on non-Unicode.
fn read_os(key: &str) -> Result<Option<String>, EnvError> {
    match std::env::var_os(key) {
        None => Ok(None),
        Some(value) => value
            .into_string()
            .map(Some)
            .map_err(|_| EnvError::NonUnicode {
                key: key.to_string(),
            }),
    }
}

/// Process-wide record of legacy keys resolved through this crate.
struct Collector {
    /// Legacy key names recorded by [`resolve`], sorted by the BTreeSet.
    keys: Mutex<BTreeSet<String>>,
}

/// Construction, recording, and draining for the process-wide collector.
impl Collector {
    /// Build the empty process-wide collector.
    const fn new() -> Self {
        Self {
            keys: Mutex::new(BTreeSet::new()),
        }
    }

    /// Record one legacy key name as used.
    fn record(&self, legacy_key: &str) {
        let mut keys = self.keys.lock().expect("env collector poisoned");
        keys.insert(legacy_key.to_string());
    }

    /// Take every recorded legacy key name, leaving the collector empty so a
    /// later emission reports only newly recorded keys.
    fn drain(&self) -> Vec<String> {
        let mut keys = self.keys.lock().expect("env collector poisoned");
        let drained: Vec<String> = keys.iter().cloned().collect();
        keys.clear();
        drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Remove both prefixed forms of one suffix from the test environment.
    fn clear(suffix: &str) {
        std::env::remove_var(format!("{CANONICAL_PREFIX}{suffix}"));
        std::env::remove_var(format!("{LEGACY_PREFIX}{suffix}"));
    }

    /// Canonical-only values resolve without recording legacy usage.
    #[test]
    fn canonical_only_resolves() {
        let suffix = "ENV_TEST_CANONICAL_ONLY";
        clear(suffix);
        std::env::set_var(format!("{CANONICAL_PREFIX}{suffix}"), "canonical");
        assert_eq!(resolve(suffix).unwrap().as_deref(), Some("canonical"));
        clear(suffix);
    }

    /// Legacy-only values resolve and are recorded for deprecation reporting.
    #[test]
    fn legacy_only_resolves_and_records() {
        let suffix = "ENV_TEST_LEGACY_ONLY";
        clear(suffix);
        std::env::set_var(format!("{LEGACY_PREFIX}{suffix}"), "legacy");
        assert_eq!(resolve(suffix).unwrap().as_deref(), Some("legacy"));
        let emitted = emit_deprecations();
        assert!(emitted
            .iter()
            .any(|key| key == &format!("{LEGACY_PREFIX}{suffix}")));
        clear(suffix);
    }

    /// Identical values on both prefixes resolve and record legacy usage.
    #[test]
    fn identical_values_resolve() {
        let suffix = "ENV_TEST_IDENTICAL";
        clear(suffix);
        std::env::set_var(format!("{CANONICAL_PREFIX}{suffix}"), "same");
        std::env::set_var(format!("{LEGACY_PREFIX}{suffix}"), "same");
        assert_eq!(resolve(suffix).unwrap().as_deref(), Some("same"));
        clear(suffix);
    }

    /// Differing values on both prefixes fail closed and name both keys.
    #[test]
    fn conflicting_values_fail_closed() {
        let suffix = "ENV_TEST_CONFLICT";
        clear(suffix);
        std::env::set_var(format!("{CANONICAL_PREFIX}{suffix}"), "one");
        std::env::set_var(format!("{LEGACY_PREFIX}{suffix}"), "two");
        let error = resolve(suffix).unwrap_err();
        assert_eq!(
            error,
            EnvError::Conflict {
                canonical_key: format!("{CANONICAL_PREFIX}{suffix}"),
                legacy_key: format!("{LEGACY_PREFIX}{suffix}"),
            }
        );
        assert!(error
            .to_string()
            .contains(&format!("{CANONICAL_PREFIX}{suffix}")));
        assert!(error
            .to_string()
            .contains(&format!("{LEGACY_PREFIX}{suffix}")));
        clear(suffix);
    }

    /// Absent keys resolve to None and resolve_or applies the default.
    #[test]
    fn absent_keys_resolve_to_default() {
        let suffix = "ENV_TEST_ABSENT";
        clear(suffix);
        assert_eq!(resolve(suffix).unwrap(), None);
        assert_eq!(resolve_or(suffix, "fallback").unwrap(), "fallback");
    }

    /// Empty strings are treated as set values and participate in conflicts.
    #[test]
    fn empty_values_are_set_values() {
        let suffix = "ENV_TEST_EMPTY";
        clear(suffix);
        std::env::set_var(format!("{CANONICAL_PREFIX}{suffix}"), "");
        std::env::set_var(format!("{LEGACY_PREFIX}{suffix}"), "value");
        assert!(matches!(
            resolve(suffix).unwrap_err(),
            EnvError::Conflict { .. }
        ));
        clear(suffix);
        std::env::set_var(format!("{CANONICAL_PREFIX}{suffix}"), "");
        assert_eq!(resolve(suffix).unwrap().as_deref(), Some(""));
        clear(suffix);
    }

    /// Non-Unicode values fail closed instead of silently disappearing.
    #[cfg(unix)]
    #[test]
    fn non_unicode_values_fail_closed() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let suffix = "ENV_TEST_NON_UNICODE";
        clear(suffix);
        let key = format!("{CANONICAL_PREFIX}{suffix}");
        std::env::set_var(&key, OsString::from_vec(vec![0xff, 0xfe]));
        assert_eq!(resolve(suffix).unwrap_err(), EnvError::NonUnicode { key });
        clear(suffix);
    }

    /// Emission drains recorded keys; a second emission without new legacy
    /// usage reports nothing.
    #[test]
    fn emission_drains_recorded_keys() {
        let suffix = "ENV_TEST_EMISSION";
        clear(suffix);
        std::env::set_var(format!("{LEGACY_PREFIX}{suffix}"), "legacy");
        resolve(suffix).unwrap();
        let first = emit_deprecations();
        assert!(first
            .iter()
            .any(|key| key == &format!("{LEGACY_PREFIX}{suffix}")));
        let second = emit_deprecations();
        assert!(!second
            .iter()
            .any(|key| key == &format!("{LEGACY_PREFIX}{suffix}")));
        clear(suffix);
    }
}
