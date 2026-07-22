//! Test-only support: a wiremock-backed `test_adapter!` macro and a context
//! builder that points phylaxd and provider base URLs at a mock server.
//!
//! The macro covers the common adapter shape -- one upstream call returning a
//! canned body. Multi-call or special-case adapters write manual wiremock
//! tests (see `notion::tests::get_page_merges_page_and_blocks`).
#![cfg(test)]

use std::sync::Arc;

use crate::phylaxd_client::PhylaxdClient;
use crate::tool::{InvokeContext, ProviderBases};

/// Build an `InvokeContext` whose phylaxd client and provider base URLs all
/// point at a single mock server.
pub fn test_ctx(base: &str) -> InvokeContext {
    InvokeContext {
        phylaxd: Arc::new(PhylaxdClient::new(
            base.to_string(),
            Some("test-phylaxd-token".to_string()),
        )),
        bases: ProviderBases {
            linear: base.to_string(),
            notion: base.to_string(),
        },
        hermes_public_url: Some("https://hermes.test".to_string()),
    }
}

/// Generate a `#[tokio::test]` that mounts a phylaxd token mock plus one
/// upstream mock, invokes the tool, and asserts top-level result fields.
macro_rules! test_adapter {
    (
        $name:ident,
        tool: $tool:expr,
        method: $method:expr,
        path: $path:expr,
        respond: $respond:expr,
        args: $args:expr,
        expect: { $($key:expr => $val:tt),* $(,)? }
    ) => {
        #[tokio::test]
        /// Verifies $name.
        async fn $name() {
            let server = ::wiremock::MockServer::start().await;
            // phylaxd token resolution mock.
            ::wiremock::Mock::given(::wiremock::matchers::method("POST"))
                .and(::wiremock::matchers::path("/resolve/raw"))
                .respond_with(::wiremock::ResponseTemplate::new(200).set_body_json(
                    ::serde_json::json!({ "category": "x", "name": "t", "value": { "access_token": "test-token" } }),
                ))
                .mount(&server)
                .await;
            // upstream mock.
            ::wiremock::Mock::given(::wiremock::matchers::method($method))
                .and(::wiremock::matchers::path($path))
                .respond_with(::wiremock::ResponseTemplate::new(200).set_body_json($respond))
                .mount(&server)
                .await;

            let ctx = $crate::adapters::test_support::test_ctx(&server.uri());
            let resp = $tool
                .invoke(&ctx, $crate::tool::InvokeRequest { tenant_id: Some("t".into()), args: $args })
                .await;
            assert!(resp.success, "expected success, got error: {:?}", resp.error);
            let result = resp.result.unwrap_or_default();
            $(
                assert_eq!(
                    result.get($key),
                    Some(&::serde_json::json!($val)),
                    "field `{}` mismatch in {:?}", $key, result
                );
            )*
        }
    };
}
