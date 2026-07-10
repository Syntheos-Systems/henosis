//! The Stripe billing webhook surface: `POST /billing/stripe/webhook`.
//!
//! Additive and env-gated, exactly like the operator surface: the route mounts only when
//! [`BillingState`] is constructed (which `main.rs` does only when
//! `SYNTHEOS_STRIPE_WEBHOOK_SECRET` is set). Absent that variable the kernel router is
//! byte-for-byte unchanged and the path 404s.
//!
//! ORDERING INVARIANT: the handler takes the request body as raw [`Bytes`] and verifies the
//! `Stripe-Signature` HMAC over those exact bytes **before** anything parses them as JSON.
//! Stripe signs the literal payload it sent, so a re-serialized body would not verify, and
//! parsing attacker-supplied JSON before authenticating it would hand unverified input to the
//! deserializer. In axum this is enforced by extractor order: `Bytes` is the last argument, so
//! no body-consuming extractor (`Json`, `Form`) can run ahead of it.
//!
//! Status mapping (matches the 6.4a design's fail-closed table):
//! - missing / malformed / expired signature -> `400`, nothing recorded, nothing applied.
//! - body that is not JSON, or an event missing required fields -> `400`.
//! - recognized event -> `200`, with the applied outcome recorded in `billing_event`.
//!   This includes the outcomes that deliberately change nothing (`unmapped_price`,
//!   `unknown_customer`, `unknown_subscription`, `ignored`, `replayed`): Stripe must not retry
//!   them, and the operator needs the row to see why a tier did not move.
//! - store failure -> `500`, so Stripe retries. The pipeline's idempotency gate makes the
//!   retry safe.
//!
//! Error bodies are deliberately terse. The detail goes to the log, not to the caller: this
//! endpoint is public by necessity, and a verifier that explains *why* a signature failed is a
//! verifier that helps forge one.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use henosis_plutus::{apply_decision, decide, parse_event, verify_stripe_signature, PlutusStore};
use serde::Serialize;

/// The header Stripe carries its signature in. Compared case-insensitively by `HeaderMap`.
const STRIPE_SIGNATURE_HEADER: &str = "stripe-signature";

/// The generic rejection body returned for every 400. Intentionally uninformative.
const REJECTED: &str = "webhook rejected";

/// Everything the Stripe webhook route needs: the policy store it writes entitlements into,
/// and the endpoint signing secret it authenticates deliveries against.
///
/// Cheap to clone (all shared handles), as the axum `State` extractor requires.
#[derive(Clone)]
pub struct BillingState {
    /// The Plutus store: entitlements, price map, org tiers, quota, and the event log.
    plutus: Arc<PlutusStore>,
    /// The Stripe endpoint signing secret (`whsec_...`), used as the HMAC key.
    ///
    /// Held as `Arc<str>` so cloning the state per request does not copy the secret, and so
    /// it is never accidentally logged through a `Debug` derive (this struct has none).
    webhook_secret: Arc<str>,
}

/// `BillingState` constructor.
impl BillingState {
    /// Wire the billing surface over a Plutus store and an endpoint signing secret.
    ///
    /// The secret is used verbatim as the HMAC key: it is never trimmed, normalized, or
    /// logged. Callers are responsible for rejecting an empty secret before calling this
    /// (see `billing_state_from_env` in `main.rs`, which makes that a hard boot error).
    pub fn new(plutus: Arc<PlutusStore>, webhook_secret: impl Into<Arc<str>>) -> Self {
        Self {
            plutus,
            webhook_secret: webhook_secret.into(),
        }
    }
}

/// The JSON acknowledgement returned on a successfully processed delivery.
///
/// Stripe ignores the response body, but naming the outcome makes the route directly
/// assertable in tests and legible in an operator's `curl`.
#[derive(Serialize)]
struct WebhookAck {
    /// The `BillingOutcome` that was applied and recorded, in its stable text form.
    outcome: &'static str,
}

/// Build the billing router: the single Stripe webhook route, bound to `state`.
///
/// Merged into the kernel router by [`crate::app::router`] only when the app state carries a
/// [`BillingState`]. No CORS layer: Stripe is a server-to-server caller, not a browser.
pub fn billing_router(state: BillingState) -> Router {
    Router::new()
        .route("/billing/stripe/webhook", post(stripe_webhook))
        .with_state(state)
}

/// Handle one Stripe webhook delivery: authenticate it, decide it, apply it, record it.
///
/// `body` is declared last so axum runs it as the body-consuming extractor, guaranteeing the
/// signature is checked against the raw bytes before any JSON parsing occurs.
async fn stripe_webhook(
    State(state): State<BillingState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. The signature header must be present and ASCII. A header we cannot read is a
    //    rejection, never a bypass.
    let Some(signature) = headers
        .get(STRIPE_SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::warn!("stripe webhook: missing or non-ascii Stripe-Signature header");
        return (StatusCode::BAD_REQUEST, REJECTED).into_response();
    };

    // 2. Authenticate the RAW bytes before anything interprets them. Nothing below this line
    //    runs for an unsigned, tampered, or replayed-outside-the-window delivery.
    if let Err(e) = verify_stripe_signature(
        &state.webhook_secret,
        signature,
        &body,
        chrono::Utc::now(),
    ) {
        tracing::warn!(error = %e, "stripe webhook: signature verification failed");
        return (StatusCode::BAD_REQUEST, REJECTED).into_response();
    }

    // 3. Now that the payload is authentic, parse it.
    let event = match parse_event(&body) {
        Ok(event) => event,
        Err(e) => {
            tracing::warn!(error = %e, "stripe webhook: unparseable event body");
            return (StatusCode::BAD_REQUEST, REJECTED).into_response();
        }
    };

    // 4. Decide what the event asks for. A malformed-but-signed event is still a 400: Stripe
    //    retrying it would not help, and we will not guess at its intent.
    let decision = match decide(&event.value) {
        Ok(decision) => decision,
        Err(e) => {
            tracing::warn!(event_id = %event.id, error = %e, "stripe webhook: undecidable event");
            return (StatusCode::BAD_REQUEST, REJECTED).into_response();
        }
    };

    // 5. The raw body text is what lands in `billing_event.payload` (a JSONB column), so the
    //    operator audits exactly the bytes Stripe signed rather than a re-serialization.
    //    `parse_event` already proved these bytes are valid UTF-8 JSON.
    let raw_payload = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => {
            tracing::warn!(event_id = %event.id, "stripe webhook: body is not utf-8");
            return (StatusCode::BAD_REQUEST, REJECTED).into_response();
        }
    };

    // 6. Apply. A store failure is a 500 so Stripe retries; the idempotency gate in
    //    `apply_decision` makes that retry a no-op if the first attempt in fact landed.
    match apply_decision(
        &state.plutus,
        &event.id,
        &event.event_type,
        raw_payload,
        decision,
    )
    .await
    {
        Ok(outcome) => {
            tracing::info!(
                event_id = %event.id,
                event_type = %event.event_type,
                outcome = outcome.as_str(),
                "stripe webhook processed"
            );
            (
                StatusCode::OK,
                Json(WebhookAck {
                    outcome: outcome.as_str(),
                }),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(event_id = %event.id, error = %e, "stripe webhook: store failure");
            (StatusCode::INTERNAL_SERVER_ERROR, "billing store error").into_response()
        }
    }
}

/// Tests for the Stripe webhook route: signature enforcement, status mapping, and replay.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use henosis_plutus::{sign_stripe_payload, QuotaTier};
    use serde_json::json;
    use syntheos_contracts::{PrincipalId, TenantId};
    use tower::ServiceExt;

    /// Fixture endpoint secret. Fake -- never a real `whsec_` value.
    const TEST_SECRET: &str = "whsec_route_test_secret";

    /// Connect to the live test database, or return `None` so the offline build stays green.
    async fn live_store() -> Option<Arc<PlutusStore>> {
        let url = match std::env::var("SYNTHEOS_PLUTUS_TEST_PG_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!(
                    "[billing route test] SYNTHEOS_PLUTUS_TEST_PG_URL not set -- skipping live webhook tests"
                );
                return None;
            }
        };
        Some(Arc::new(PlutusStore::open(&url).await.expect("open store")))
    }

    /// Build the billing router over a live store and the fixture secret.
    async fn live_app() -> Option<(Router, Arc<PlutusStore>)> {
        let store = live_store().await?;
        let router = billing_router(BillingState::new(store.clone(), TEST_SECRET));
        Some((router, store))
    }

    /// POST `body` to the webhook with the given signature header, returning status + body.
    async fn post_webhook(
        app: Router,
        signature: Option<&str>,
        body: &[u8],
    ) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/billing/stripe/webhook");
        if let Some(sig) = signature {
            req = req.header("Stripe-Signature", sig);
        }
        let response = app
            .oneshot(req.body(Body::from(body.to_vec())).expect("request"))
            .await
            .expect("router responds");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Sign `body` at the current time so it lands inside the tolerance window.
    fn sign_now(body: &[u8]) -> String {
        sign_stripe_payload(TEST_SECRET, chrono::Utc::now().timestamp(), body)
    }

    /// Build a `customer.subscription.created` event body.
    fn created_event(event_id: &str, customer: &str, subscription: &str, price: &str) -> Vec<u8> {
        json!({
            "id": event_id,
            "type": "customer.subscription.created",
            "data": { "object": {
                "id": subscription,
                "customer": customer,
                "status": "active",
                "items": { "data": [ { "price": { "id": price } } ] }
            }}
        })
        .to_string()
        .into_bytes()
    }

    /// A missing `Stripe-Signature` header is a 400, not a bypass.
    #[tokio::test]
    async fn missing_signature_header_is_rejected() {
        let Some((app, _)) = live_app().await else {
            return;
        };
        let (status, _) = post_webhook(app, None, b"{}").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// A signature computed over a different body is a 400.
    #[tokio::test]
    async fn tampered_body_is_rejected() {
        let Some((app, _)) = live_app().await else {
            return;
        };
        let signature = sign_now(b"{\"id\":\"evt_a\"}");
        let (status, _) = post_webhook(app, Some(&signature), b"{\"id\":\"evt_b\"}").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// A signature from outside the tolerance window is a 400 (replay defense).
    #[tokio::test]
    async fn expired_signature_is_rejected() {
        let Some((app, _)) = live_app().await else {
            return;
        };
        let body = b"{\"id\":\"evt_old\"}";
        let stale = chrono::Utc::now().timestamp() - 10_000;
        let signature = sign_stripe_payload(TEST_SECRET, stale, body);
        let (status, _) = post_webhook(app, Some(&signature), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// A correctly signed body that is not JSON is a 400, and never panics the handler.
    #[tokio::test]
    async fn signed_but_non_json_body_is_rejected() {
        let Some((app, _)) = live_app().await else {
            return;
        };
        let body = b"this is signed but it is not json";
        let signature = sign_now(body);
        let (status, _) = post_webhook(app, Some(&signature), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// A signed event missing `data.object` is a 400 (undecidable), not a 500.
    #[tokio::test]
    async fn signed_event_missing_fields_is_rejected() {
        let Some((app, _)) = live_app().await else {
            return;
        };
        let body = json!({ "id": "evt_bad", "type": "customer.subscription.created" })
            .to_string()
            .into_bytes();
        let signature = sign_now(&body);
        let (status, _) = post_webhook(app, Some(&signature), &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// A valid, signed subscription event applies its tier and returns 200 with the outcome.
    #[tokio::test]
    async fn valid_event_applies_tier_and_returns_outcome() {
        let Some((app, store)) = live_app().await else {
            return;
        };
        let tenant = TenantId::new();
        store
            .create_org(tenant, "webhook-route", PrincipalId::new(), QuotaTier::Free)
            .await
            .expect("create_org");
        let customer = format!("cus_{}", tenant.as_uuid());
        let price = format!("price_{}", tenant.as_uuid());
        store
            .upsert_billing_customer(tenant, &customer)
            .await
            .expect("upsert_billing_customer");
        store
            .insert_price_mapping(&price, QuotaTier::Pro)
            .await
            .expect("insert_price_mapping");

        let body = created_event(
            &format!("evt_route_{}", tenant.as_uuid()),
            &customer,
            &format!("sub_{}", tenant.as_uuid()),
            &price,
        );
        let signature = sign_now(&body);
        let (status, text) = post_webhook(app, Some(&signature), &body).await;

        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("applied"), "response body: {text}");
        assert_eq!(store.org_tier(tenant).await.unwrap(), Some(QuotaTier::Pro));
        assert_eq!(
            store.quota_config(tenant).await.unwrap(),
            Some(QuotaTier::Pro.defaults())
        );
    }

    /// Redelivering the same event id is a 200 no-op reporting `replayed`, not a double-apply.
    #[tokio::test]
    async fn replayed_event_is_ok_and_applies_nothing() {
        let Some((app, store)) = live_app().await else {
            return;
        };
        let tenant = TenantId::new();
        store
            .create_org(tenant, "webhook-replay", PrincipalId::new(), QuotaTier::Free)
            .await
            .expect("create_org");
        let customer = format!("cus_{}", tenant.as_uuid());
        let price = format!("price_{}", tenant.as_uuid());
        store
            .upsert_billing_customer(tenant, &customer)
            .await
            .expect("upsert_billing_customer");
        store
            .insert_price_mapping(&price, QuotaTier::Pro)
            .await
            .expect("insert_price_mapping");

        let body = created_event(
            &format!("evt_replay_route_{}", tenant.as_uuid()),
            &customer,
            &format!("sub_{}", tenant.as_uuid()),
            &price,
        );
        let signature = sign_now(&body);

        let first = billing_router(BillingState::new(store.clone(), TEST_SECRET));
        let (status, text) = post_webhook(first, Some(&signature), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("applied"), "first delivery: {text}");

        let (status, text) = post_webhook(app, Some(&signature), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("replayed"), "second delivery: {text}");
        assert_eq!(store.org_tier(tenant).await.unwrap(), Some(QuotaTier::Pro));
    }

    /// An unmapped price is a 200 (Stripe must not retry) that changes no tier.
    #[tokio::test]
    async fn unmapped_price_is_ok_and_changes_nothing() {
        let Some((app, store)) = live_app().await else {
            return;
        };
        let tenant = TenantId::new();
        store
            .create_org(tenant, "webhook-unmapped", PrincipalId::new(), QuotaTier::Free)
            .await
            .expect("create_org");
        let customer = format!("cus_{}", tenant.as_uuid());
        store
            .upsert_billing_customer(tenant, &customer)
            .await
            .expect("upsert_billing_customer");

        let body = created_event(
            &format!("evt_unmapped_{}", tenant.as_uuid()),
            &customer,
            &format!("sub_{}", tenant.as_uuid()),
            "price_never_mapped_route",
        );
        let signature = sign_now(&body);
        let (status, text) = post_webhook(app, Some(&signature), &body).await;

        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("unmapped_price"), "response body: {text}");
        assert_eq!(store.org_tier(tenant).await.unwrap(), Some(QuotaTier::Free));
    }
}
