//! The eidolon output side: scrub credential-bearing fields from executor results before they
//! reach the caller (the G1 `OutputFilter` slot, wired via `Dispatcher::with_output_filter`).

use async_trait::async_trait;
use syntheos_contracts::{FilterDecision, OutputFilter, RequestContext};

use crate::policy::{EidolonError, EidolonPolicy};

/// Lowercase a field name for pattern matching. Field names are identifiers, not prose, so
/// (unlike the gate's prose normalizer) no whitespace collapsing is involved.
fn normalize_key(key: &str) -> String {
    key.to_lowercase()
}

/// What a redacted field's value is replaced with.
pub const REDACTED: &str = "[redacted]";

/// The policy authority for the dispatcher's output-filter slot.
///
/// Walks the executor result and replaces, in place, the value of every object field whose
/// normalized (lowercased) name contains a sensitive-field pattern from the shared
/// [`EidolonPolicy`], then passes the scrubbed result through. Clean output passes unchanged.
/// The whole subtree under a matching key is replaced -- a credential inside a `credentials`
/// object never survives by nesting.
pub struct EidolonOutputFilter {
    /// The patterns matched against normalized field names. Pre-normalized at construction.
    sensitive_fields: Vec<String>,
}

impl EidolonOutputFilter {
    /// Build the filter from the shared policy, validating the sensitive-field patterns: every
    /// pattern must survive normalization non-empty (an empty pattern is a substring of every
    /// field name and would redact the entire result -- a config error, not a policy).
    pub fn new(policy: &EidolonPolicy) -> Result<Self, EidolonError> {
        let mut normalized = Vec::with_capacity(policy.sensitive_fields.len());
        for pattern in &policy.sensitive_fields {
            let n = normalize_key(pattern.trim());
            if n.is_empty() {
                return Err(EidolonError::InvalidPolicy(format!(
                    "sensitive-field pattern {pattern:?} is empty after normalization and would redact every field"
                )));
            }
            normalized.push(n);
        }
        Ok(Self {
            sensitive_fields: normalized,
        })
    }

    /// True when a (normalized) field name matches the sensitive set.
    fn is_sensitive(&self, key: &str) -> bool {
        let key = normalize_key(key);
        self.sensitive_fields
            .iter()
            .any(|p| key.contains(p.as_str()))
    }
}

#[async_trait]
impl OutputFilter for EidolonOutputFilter {
    /// The canonical authority name, matching the gate slot this filter is the output side of.
    fn name(&self) -> &str {
        "eidolon"
    }

    /// Scrub sensitive fields in place (iteratively -- no recursion depth to exhaust), then pass
    /// the result through. The whole value under a matching key is replaced, so nothing nested
    /// beneath a credential-bearing field survives.
    async fn filter(
        &self,
        result: &mut serde_json::Value,
        _ctx: &RequestContext,
    ) -> FilterDecision {
        if self.sensitive_fields.is_empty() {
            return FilterDecision::Pass;
        }
        let mut stack: Vec<&mut serde_json::Value> = vec![result];
        while let Some(value) = stack.pop() {
            match value {
                serde_json::Value::Object(map) => {
                    for (k, v) in map.iter_mut() {
                        if self.is_sensitive(k) {
                            *v = serde_json::Value::String(REDACTED.to_string());
                        } else {
                            stack.push(v);
                        }
                    }
                }
                serde_json::Value::Array(items) => stack.extend(items.iter_mut()),
                _ => {}
            }
        }
        FilterDecision::Pass
    }
}

/// Tests for output scrubbing and its invariants.
#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use syntheos_contracts::{PrincipalId, TenantId};

    use super::*;
    use crate::policy::default_sensitive_fields;

    /// Build a minimal request context for filter calls.
    fn ctx() -> RequestContext {
        RequestContext {
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            persona: None,
            session: None,
            room: None,
            task: None,
            workflow: None,
        }
    }

    /// Build the filter under test from the default policy.
    fn filter() -> EidolonOutputFilter {
        EidolonOutputFilter::new(&EidolonPolicy::default()).expect("valid default policy")
    }

    /// Run one filter call to completion on a fresh single-thread runtime.
    fn run_filter(f: &EidolonOutputFilter, value: &mut serde_json::Value) -> FilterDecision {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(f.filter(value, &ctx()))
    }

    /// The filter reports the eidolon authority name.
    #[test]
    fn filter_name_is_eidolon() {
        assert_eq!(filter().name(), "eidolon");
    }

    /// The default sensitive-field set covers the classic credential field names.
    #[test]
    fn default_sensitive_fields_cover_classics() {
        let fields = default_sensitive_fields();
        for classic in ["password", "secret", "token", "api_key"] {
            assert!(
                fields.iter().any(|f| f == classic),
                "missing {classic:?} in defaults"
            );
        }
    }

    /// Clean output passes through unchanged.
    #[tokio::test]
    async fn clean_output_passes_unchanged() {
        let mut result = serde_json::json!({ "ok": true, "items": [1, 2, 3], "name": "loom" });
        let before = result.clone();
        let decision = filter().filter(&mut result, &ctx()).await;
        assert_eq!(decision, FilterDecision::Pass);
        assert_eq!(result, before);
    }

    /// A sensitive top-level field is redacted in place; siblings survive.
    #[tokio::test]
    async fn sensitive_field_redacted_in_place() {
        let mut result = serde_json::json!({ "ok": true, "api_key": "kleos-key-123" });
        let decision = filter().filter(&mut result, &ctx()).await;
        assert_eq!(decision, FilterDecision::Pass);
        assert_eq!(result["api_key"], serde_json::json!(REDACTED));
        assert_eq!(result["ok"], serde_json::json!(true));
    }

    /// A sensitive field buried in nested arrays/objects is found, and the whole subtree under
    /// a matching key is replaced.
    #[tokio::test]
    async fn nested_sensitive_subtree_redacted() {
        let mut result = serde_json::json!({
            "data": [
                { "agent": "soma", "credentials": { "password": "hunter2", "user": "agent" } }
            ]
        });
        let decision = filter().filter(&mut result, &ctx()).await;
        assert_eq!(decision, FilterDecision::Pass);
        assert_eq!(
            result["data"][0]["credentials"],
            serde_json::json!(REDACTED),
            "the whole credentials subtree must be replaced"
        );
        assert_eq!(result["data"][0]["agent"], serde_json::json!("soma"));
    }

    /// Field-name matching is case-insensitive and substring-based.
    #[tokio::test]
    async fn key_match_is_case_insensitive_substring() {
        let mut result = serde_json::json!({ "Kleos_API_Key": "k", "Bearer_Header": "b" });
        let decision = filter().filter(&mut result, &ctx()).await;
        assert_eq!(decision, FilterDecision::Pass);
        assert_eq!(result["Kleos_API_Key"], serde_json::json!(REDACTED));
        assert_eq!(result["Bearer_Header"], serde_json::json!(REDACTED));
    }

    /// Non-object output (a bare scalar) passes unchanged: there is no field to match.
    #[tokio::test]
    async fn non_object_output_passes() {
        let mut result = serde_json::json!("a bare string result");
        let before = result.clone();
        let decision = filter().filter(&mut result, &ctx()).await;
        assert_eq!(decision, FilterDecision::Pass);
        assert_eq!(result, before);
    }

    /// An explicitly empty sensitive-field list disables output scrubbing.
    #[tokio::test]
    async fn empty_sensitive_list_disables_scrub() {
        let policy = EidolonPolicy {
            sensitive_fields: Vec::new(),
            ..EidolonPolicy::default()
        };
        let f = EidolonOutputFilter::new(&policy).expect("valid policy");
        let mut result = serde_json::json!({ "password": "hunter2" });
        let decision = f.filter(&mut result, &ctx()).await;
        assert_eq!(decision, FilterDecision::Pass);
        assert_eq!(result["password"], serde_json::json!("hunter2"));
    }

    /// An empty (or whitespace-only) sensitive-field pattern is a config error, rejected at
    /// build: it would match every field name and redact the entire result.
    #[test]
    fn empty_sensitive_pattern_rejected_at_construction() {
        let policy = EidolonPolicy {
            sensitive_fields: vec!["password".to_string(), " ".to_string()],
            ..EidolonPolicy::default()
        };
        let err = EidolonOutputFilter::new(&policy)
            .err()
            .expect("empty sensitive pattern must be rejected");
        assert!(matches!(err, EidolonError::InvalidPolicy(_)), "got {err:?}");
    }

    /// The real filter scrubs an executor result from inside the dispatcher.
    #[tokio::test]
    async fn dispatcher_redacts_through_eidolon_filter() {
        use std::sync::Arc;

        use syntheos_axon::AxonBus;
        use syntheos_contracts::{Gate, GateRequest, ToolInvocation};
        use syntheos_dispatch::stubs::{stub_gate_chain, EchoExecutor};
        use syntheos_dispatch::{DispatchOutcome, Dispatcher};

        /// An executor returning a result that carries a credential field.
        struct LeakyExecutor;

        /// The leaky executor returns a payload with a secret to scrub.
        #[async_trait]
        impl syntheos_dispatch::Executor for LeakyExecutor {
            async fn execute(
                &self,
                _ctx: &RequestContext,
                _inv: &ToolInvocation,
            ) -> Result<serde_json::Value, syntheos_dispatch::ExecutorError> {
                Ok(serde_json::json!({ "ok": true, "api_key": "leaked-key" }))
            }
        }

        // EchoExecutor unused here, but stub_gate_chain needs the stubs feature anyway.
        let _ = EchoExecutor;
        let _: Vec<Box<dyn Gate>> = stub_gate_chain();

        let bus = Arc::new(AxonBus::new());
        let dispatcher = Dispatcher::new(stub_gate_chain(), Box::new(LeakyExecutor), bus)
            .expect("canonical chain")
            .with_output_filter(Box::new(filter()));

        let outcome = dispatcher
            .dispatch(GateRequest {
                context: ctx(),
                invocation: ToolInvocation {
                    tool: "kleos".to_string(),
                    action: "whoami".to_string(),
                    args: serde_json::json!({}),
                },
            })
            .await
            .expect("dispatch");
        match outcome {
            DispatchOutcome::Executed { result } => {
                assert_eq!(result["api_key"], serde_json::json!(REDACTED));
                assert_eq!(result["ok"], serde_json::json!(true));
            }
            other => panic!("expected Executed, got {other:?}"),
        }
    }

    /// Strategy: arbitrary JSON, with keys that sometimes hit the sensitive set.
    fn arb_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|n| serde_json::json!(n)),
            "[a-zA-Z0-9 _.,-]{0,24}".prop_map(serde_json::Value::String),
        ];
        let key = prop_oneof![
            "[a-z_]{1,10}",
            Just("password".to_string()),
            Just("api_key".to_string()),
            Just("nested_token".to_string()),
        ];
        leaf.prop_recursive(4, 32, 6, move |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                prop::collection::btree_map(key.clone(), inner, 0..4)
                    .prop_map(|m| { serde_json::Value::Object(m.into_iter().collect()) }),
            ]
        })
    }

    /// True when any object key in `value` (normalized) contains a sensitive pattern.
    fn has_sensitive_key(value: &serde_json::Value, patterns: &[String]) -> bool {
        let mut stack = vec![value];
        while let Some(v) = stack.pop() {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, val) in map {
                        let key = k.to_lowercase();
                        if patterns.iter().any(|p| key.contains(p.as_str())) {
                            return true;
                        }
                        stack.push(val);
                    }
                }
                serde_json::Value::Array(items) => stack.extend(items.iter()),
                _ => {}
            }
        }
        false
    }

    proptest! {
        /// THE output invariant: after filtering, no sensitive key anywhere in the result maps
        /// to anything but the redaction marker, and the decision is always Pass.
        #[test]
        fn no_sensitive_value_survives(mut value in arb_json()) {
            let f = filter();
            let decision = run_filter(&f, &mut value);
            prop_assert_eq!(decision, FilterDecision::Pass);
            let patterns = default_sensitive_fields();
            let mut stack = vec![&value];
            while let Some(v) = stack.pop() {
                match v {
                    serde_json::Value::Object(map) => {
                        for (k, val) in map {
                            let key = k.to_lowercase();
                            if patterns.iter().any(|p| key.contains(p.as_str())) {
                                prop_assert_eq!(
                                    val,
                                    &serde_json::json!(REDACTED),
                                    "sensitive key {} survived",
                                    k
                                );
                            } else {
                                stack.push(val);
                            }
                        }
                    }
                    serde_json::Value::Array(items) => stack.extend(items.iter()),
                    _ => {}
                }
            }
        }

        /// Filtering is idempotent: a second pass changes nothing.
        #[test]
        fn filtering_is_idempotent(mut value in arb_json()) {
            let f = filter();
            run_filter(&f, &mut value);
            let once = value.clone();
            run_filter(&f, &mut value);
            prop_assert_eq!(value, once);
        }

        /// Output with no sensitive keys is never modified.
        #[test]
        fn clean_output_is_never_modified(mut value in arb_json()) {
            let patterns = default_sensitive_fields();
            prop_assume!(!has_sensitive_key(&value, &patterns));
            let before = value.clone();
            let f = filter();
            let decision = run_filter(&f, &mut value);
            prop_assert_eq!(decision, FilterDecision::Pass);
            prop_assert_eq!(value, before);
        }
    }
}
