//! The output-filter seam: redaction/transform applied to an action result AFTER it has cleared
//! the gate chain and executed. The input-authorization path is [`crate::Gate`] /
//! [`crate::GateDecision`] (see `gate.rs`); this is its deliberate counterpart for the OUTPUT
//! direction, kept as a separate interface so input authorization and output redaction never
//! collapse into one another.
//!
//! The dispatcher holds an optional filter and, when none is set, passes results through
//! unchanged. A policy filter can implement [`OutputFilter`] and be wired through
//! `Dispatcher::with_output_filter` without changing the dispatcher's construction API. The trait is `async`
//! (matching [`crate::Gate`]) so a real filter may consult an external policy or redaction
//! service without a later breaking change to this signature.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::action::RequestContext;

/// What an [`OutputFilter`] decides to do with an action result.
///
/// Marked `#[non_exhaustive]`: filter outcomes may grow (e.g. partial-field redaction policies),
/// and downstream matches must keep a wildcard arm. The dispatcher treats an unrecognized
/// decision as a withhold -- fail-closed, so an output is never leaked unfiltered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FilterDecision {
    /// Emit the result as-is. A filter may scrub fields in place via its `&mut` access and then
    /// return `Pass`, meaning "use whatever the result now holds".
    Pass,
    /// Withhold the result entirely; the dispatcher substitutes a redaction notice carrying this
    /// reason. Use when no part of the output may reach the caller.
    Redact {
        /// Why the result was withheld.
        reason: String,
    },
    /// Replace the result wholesale with this value (e.g. a transformed or minimised projection).
    Replace(serde_json::Value),
}

/// Filters or transforms an action result after successful execution.
///
/// Object-safe via `async_trait` so the dispatcher can hold `Option<Box<dyn OutputFilter>>`. An
/// absent filter is equivalent to a filter that always returns [`FilterDecision::Pass`] unchanged.
#[async_trait]
pub trait OutputFilter: Send + Sync {
    /// A short, stable name for this filter (used in logs and audit trails so a redaction is
    /// attributable to the filter that performed it).
    fn name(&self) -> &str;

    /// Inspect and optionally scrub `result` in place, then decide how it is emitted via the
    /// returned [`FilterDecision`]. `ctx` is the same request context the gate chain saw, so a
    /// filter can redact per principal, tenant, or persona.
    async fn filter(&self, result: &mut serde_json::Value, ctx: &RequestContext) -> FilterDecision;
}

/// Tests for output-filter object-safety and decision wire contracts.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{PrincipalId, TenantId};

    /// A filter that scrubs a `secret` field in place and passes the rest through; exists only to
    /// prove the trait is usable as a trait object.
    struct ScrubSecretFilter;

    /// Implement `OutputFilter` for the test scrub filter.
    #[async_trait]
    impl OutputFilter for ScrubSecretFilter {
        /// Return the stable test filter name.
        fn name(&self) -> &str {
            "scrub-secret"
        }

        /// Remove a top-level `secret` field, then pass the scrubbed result through.
        async fn filter(
            &self,
            result: &mut serde_json::Value,
            _ctx: &RequestContext,
        ) -> FilterDecision {
            if let Some(obj) = result.as_object_mut() {
                obj.remove("secret");
            }
            FilterDecision::Pass
        }
    }

    /// Build a minimal request context for tests.
    fn sample_ctx() -> RequestContext {
        RequestContext {
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            persona: None,
            session: None,
            room: None,
            task: None,
            workflow: None,
            authority: None,
        }
    }

    /// A boxed filter remains object-safe, callable, and can scrub in place.
    #[tokio::test]
    async fn boxed_filter_is_object_safe_and_scrubs() {
        let filter: Box<dyn OutputFilter> = Box::new(ScrubSecretFilter);
        assert_eq!(filter.name(), "scrub-secret");
        let mut result = serde_json::json!({ "ok": true, "secret": "hunter2" });
        let decision = filter.filter(&mut result, &sample_ctx()).await;
        assert_eq!(decision, FilterDecision::Pass);
        assert_eq!(result, serde_json::json!({ "ok": true }));
    }

    /// Filter decision variants roundtrip without adding or removing variants.
    #[test]
    fn filter_decision_roundtrip() {
        for d in [
            FilterDecision::Pass,
            FilterDecision::Redact {
                reason: "pii".to_string(),
            },
            FilterDecision::Replace(serde_json::json!({ "minimised": true })),
        ] {
            let json = serde_json::to_string(&d).expect("serialize");
            let back: FilterDecision = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(d, back);
        }
    }
}
