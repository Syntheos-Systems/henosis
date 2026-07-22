//! Stripe webhook event pipeline: decide what an event means, then apply it.
//!
//! The pipeline is deliberately split in two halves:
//!
//! 1. [`decide`] is **pure**. Given a parsed Stripe event it returns a [`BillingDecision`]
//!    describing what the event asks for, using only the JSON in front of it -- no database,
//!    no clock, no I/O. That makes every branch testable against canned fixtures.
//! 2. [`apply_decision`] is a **thin applier** over [`PlutusStore`]. It runs the idempotency
//!    gate, resolves the Stripe identifiers against our own tables, executes the decision,
//!    and records the outcome in `billing_event`.
//!
//! FAIL-CLOSED RULES (each has a test):
//! - A subscription status we do not recognize is treated as past-due, never as an upgrade.
//!   A tier is only ever raised on a status we explicitly know to be good.
//! - An unmapped Stripe price id yields [`BillingOutcome::UnmappedPrice`] and writes nothing:
//!   the tier is undeterminable, so guessing one would hand out quota for free.
//! - A Stripe customer id we have never seen yields [`BillingOutcome::UnknownCustomer`] and
//!   writes nothing. Events name the payer by customer id, so an unknown customer means we
//!   cannot bind the event to an org.
//! - A cancel/past-due naming a subscription with no entitlement row yields
//!   [`BillingOutcome::UnknownSubscription`] and changes no tier.
//! - A payment failure never lowers quota (decision D3: Stripe runs its own dunning flow,
//!   and the org keeps its tier until the subscription is actually deleted).
//! - Any store error propagates as `Err`, which the HTTP layer maps to 500 so Stripe retries.
//!   The idempotency gate makes that retry safe. No database error is ever swallowed.
//!
//! Event JSON reaches this module already signature-verified (see [`super::signature`]), but
//! it is still attacker-adjacent: it arrives over HTTP and its shape is not ours. Nothing here
//! indexes a JSON array directly, unwraps, or panics on a malformed event.

use serde_json::Value;

use crate::billing::EntitlementStatus;
use crate::quota::QuotaTier;
use crate::store::PlutusStore;

/// What a verified Stripe event asks us to do, decided without touching the database.
///
/// The variants carry raw Stripe identifiers rather than resolved tenants and tiers, because
/// resolving those requires reading `billing_customer` and `billing_price_map`. Keeping the
/// decision free of that lookup is what makes [`decide`] pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BillingDecision {
    /// A subscription is in good standing: bind it to a tier and apply that tier's quota.
    UpsertEntitlement {
        /// The Stripe customer id that identifies the paying org.
        customer_id: String,
        /// The Stripe subscription id, our idempotency key for the entitlement row.
        subscription_id: String,
        /// The Stripe price id, which maps to a `QuotaTier` via `billing_price_map`.
        price_id: String,
        /// When the current billing period ends, as an RFC3339 string, when Stripe sent one.
        current_period_end: Option<String>,
    },
    /// A subscription is delinquent or in a state we do not recognize. Grace applies: the
    /// entitlement is marked past-due but the org keeps its quota.
    MarkPastDue {
        /// The Stripe customer id that identifies the paying org.
        customer_id: String,
        /// The Stripe subscription id whose entitlement goes past-due.
        subscription_id: String,
    },
    /// A subscription ended. The entitlement is canceled and the org falls back to Free.
    Cancel {
        /// The Stripe customer id that identifies the paying org.
        customer_id: String,
        /// The Stripe subscription id whose entitlement is canceled.
        subscription_id: String,
    },
    /// The event is well-formed but not one we act on. Recorded, then dropped.
    Ignore {
        /// Why the event was ignored, recorded for operator forensics.
        reason: String,
    },
}

/// What actually happened when a decision was applied. `as_str` is persisted to
/// `billing_event.outcome`, so these strings are a stable operator-facing contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BillingOutcome {
    /// An entitlement was written and the org's tier and quota were updated.
    Applied {
        /// The tier that was applied.
        tier: QuotaTier,
    },
    /// The entitlement was canceled and the org was downgraded to Free.
    Canceled,
    /// The entitlement was marked past-due. Quota deliberately unchanged (D3 grace).
    PastDue,
    /// The event was recognized but is not one we act on.
    Ignored,
    /// The event's Stripe price id has no row in `billing_price_map`. Nothing was changed.
    UnmappedPrice,
    /// The event's Stripe customer id has no row in `billing_customer`. Nothing was changed.
    UnknownCustomer,
    /// The event names a subscription with no entitlement row. Nothing was changed.
    UnknownSubscription,
    /// This event id was processed before. Nothing was re-applied.
    Replayed,
}

/// `BillingOutcome` accessors.
impl BillingOutcome {
    /// Return the stable text stored in `billing_event.outcome`.
    pub fn as_str(&self) -> &'static str {
        match self {
            BillingOutcome::Applied { .. } => "applied",
            BillingOutcome::Canceled => "canceled",
            BillingOutcome::PastDue => "past_due",
            BillingOutcome::Ignored => "ignored",
            BillingOutcome::UnmappedPrice => "unmapped_price",
            BillingOutcome::UnknownCustomer => "unknown_customer",
            BillingOutcome::UnknownSubscription => "unknown_subscription",
            BillingOutcome::Replayed => "replayed",
        }
    }
}

/// Errors raised while parsing or interpreting a Stripe event body.
///
/// Both variants mean "this event is not something we can act on safely". Neither is ever a
/// panic: a malformed body is a 400, not a crashed worker.
#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    /// The request body was not valid JSON.
    #[error("stripe event body is not valid json: {0}")]
    MalformedJson(String),
    /// A field the event's own type requires was absent or the wrong JSON type.
    #[error("stripe event is missing required field: {0}")]
    MissingField(&'static str),
}

/// A Stripe event body far enough parsed to be routed: its id, its type, and the raw JSON.
///
/// The webhook handler needs `id` (for the idempotency gate) and `event_type` (for the
/// `billing_event` log) before it can call [`decide`], so this bundles all three.
#[derive(Debug, Clone)]
pub struct StripeEvent {
    /// The Stripe event id (`evt_...`); the idempotency key for at-most-once processing.
    pub id: String,
    /// The Stripe event type, e.g. `customer.subscription.created`.
    pub event_type: String,
    /// The full parsed event, handed to [`decide`].
    pub value: Value,
}

/// Parse a raw webhook body into a [`StripeEvent`].
///
/// Call this only after the signature has been verified over the same raw bytes. Returns a
/// typed error (never a panic) when the body is not JSON or lacks `id`/`type`.
pub fn parse_event(body: &[u8]) -> Result<StripeEvent, DecideError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| DecideError::MalformedJson(e.to_string()))?;
    let id = required_str(&value, "id")?.to_owned();
    let event_type = required_str(&value, "type")?.to_owned();
    Ok(StripeEvent {
        id,
        event_type,
        value,
    })
}

/// Read a required string field from a JSON object, or fail with a named `MissingField`.
fn required_str<'a>(v: &'a Value, key: &'static str) -> Result<&'a str, DecideError> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or(DecideError::MissingField(key))
}

/// Return the `data.object` sub-document every Stripe event carries, or `MissingField`.
fn event_object(event: &Value) -> Result<&Value, DecideError> {
    event
        .get("data")
        .and_then(|d| d.get("object"))
        .ok_or(DecideError::MissingField("data.object"))
}

/// Extract the first line item's price id from a subscription object.
///
/// Uses `.get(0)` rather than indexing: an absent or empty `items.data` array is a typed
/// error, not a panic. Stripe sends one line item per subscription in the single-price model
/// this story supports (multi-price subscriptions are a documented non-goal of 6.4a).
fn first_price_id(obj: &Value) -> Result<String, DecideError> {
    obj.get("items")
        .and_then(|i| i.get("data"))
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("price"))
        .and_then(|price| price.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(DecideError::MissingField(
            "data.object.items.data[0].price.id",
        ))
}

/// Convert Stripe's `current_period_end` unix timestamp into an RFC3339 string.
///
/// Returns `None` when the field is absent, null, not an integer, or out of the range chrono
/// can represent. A missing period end is normal (manual and legacy subscriptions omit it),
/// so this is deliberately lenient rather than an error.
fn period_end_rfc3339(obj: &Value) -> Option<String> {
    let secs = obj.get("current_period_end")?.as_i64()?;
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.to_rfc3339())
}

/// Decide what a verified Stripe event asks us to do, using only the event itself.
///
/// Routing by `type`:
/// - `customer.subscription.created` / `.updated`: branch on `data.object.status`.
///   `active`/`trialing` upsert an entitlement; `canceled` cancels; **every other status,
///   including one Stripe invents after this code ships, marks past-due**. Never upgrade on a
///   status we do not understand.
/// - `customer.subscription.deleted`: cancel, whatever the status says.
/// - `invoice.payment_failed`: mark past-due. An invoice with no `subscription` is a one-off
///   charge, not a subscription event, so it is ignored rather than treated as an error.
/// - anything else: ignored, with the type recorded for forensics.
pub fn decide(event: &Value) -> Result<BillingDecision, DecideError> {
    let event_type = required_str(event, "type")?;

    match event_type {
        "customer.subscription.created" | "customer.subscription.updated" => {
            let obj = event_object(event)?;
            let customer_id = required_str(obj, "customer")?.to_owned();
            let subscription_id = required_str(obj, "id")?.to_owned();
            let status = required_str(obj, "status")?;

            match status {
                // The only two statuses that may raise an org's tier.
                "active" | "trialing" => Ok(BillingDecision::UpsertEntitlement {
                    customer_id,
                    subscription_id,
                    price_id: first_price_id(obj)?,
                    current_period_end: period_end_rfc3339(obj),
                }),
                "canceled" => Ok(BillingDecision::Cancel {
                    customer_id,
                    subscription_id,
                }),
                // Fail-closed default: past_due, unpaid, incomplete, incomplete_expired, and
                // any future status string all land here. Grace, never an upgrade.
                _ => Ok(BillingDecision::MarkPastDue {
                    customer_id,
                    subscription_id,
                }),
            }
        }
        "customer.subscription.deleted" => {
            let obj = event_object(event)?;
            Ok(BillingDecision::Cancel {
                customer_id: required_str(obj, "customer")?.to_owned(),
                subscription_id: required_str(obj, "id")?.to_owned(),
            })
        }
        "invoice.payment_failed" => {
            let obj = event_object(event)?;
            let customer_id = required_str(obj, "customer")?.to_owned();
            match obj.get("subscription").and_then(Value::as_str) {
                Some(subscription_id) => Ok(BillingDecision::MarkPastDue {
                    customer_id,
                    subscription_id: subscription_id.to_owned(),
                }),
                // A one-off invoice carries no subscription. Not an error, just not ours.
                None => Ok(BillingDecision::Ignore {
                    reason: "invoice.payment_failed without a subscription".to_owned(),
                }),
            }
        }
        other => Ok(BillingDecision::Ignore {
            reason: format!("unhandled event type: {other}"),
        }),
    }
}

/// Apply a [`BillingDecision`] against the store and record the outcome.
///
/// Runs the idempotency gate first: an event id already present in `billing_event` returns
/// [`BillingOutcome::Replayed`] having applied and recorded nothing. Every other outcome,
/// including the ones that change nothing, is written to `billing_event` so an operator can
/// see why a webhook did not move a tier.
///
/// `raw_payload` must be the event's JSON text: it is bound into a `JSONB` column.
///
/// Store errors propagate. The caller maps them to 500 so Stripe retries, which the
/// idempotency gate makes safe.
pub async fn apply_decision(
    store: &PlutusStore,
    event_id: &str,
    event_type: &str,
    raw_payload: &str,
    decision: BillingDecision,
) -> crate::Result<BillingOutcome> {
    // Idempotency gate. A redelivered event applies nothing and records nothing new.
    if store.billing_event_seen(event_id).await? {
        tracing::debug!(%event_id, "stripe event already processed; replay ignored");
        return Ok(BillingOutcome::Replayed);
    }

    let outcome = match decision {
        BillingDecision::Ignore { reason } => {
            tracing::debug!(%event_id, %event_type, %reason, "stripe event ignored");
            BillingOutcome::Ignored
        }

        BillingDecision::UpsertEntitlement {
            customer_id,
            subscription_id,
            price_id,
            current_period_end,
        } => match store.tenant_for_stripe_customer(&customer_id).await? {
            None => {
                tracing::warn!(%event_id, %customer_id, "stripe customer maps to no tenant");
                BillingOutcome::UnknownCustomer
            }
            Some(tenant) => match store.price_tier(&price_id).await? {
                // Never guess a tier. Record the gap loudly so the operator can add the
                // mapping, and leave the org exactly as it was.
                None => {
                    tracing::warn!(%event_id, %price_id, "stripe price has no tier mapping");
                    BillingOutcome::UnmappedPrice
                }
                Some(tier) => {
                    store
                        .upsert_stripe_entitlement(
                            tenant,
                            &subscription_id,
                            tier,
                            EntitlementStatus::Active,
                            current_period_end.as_deref(),
                        )
                        .await?;
                    store.apply_tier(tenant, tier).await?;
                    BillingOutcome::Applied { tier }
                }
            },
        },

        BillingDecision::MarkPastDue {
            customer_id,
            subscription_id,
        } => match store.tenant_for_stripe_customer(&customer_id).await? {
            None => {
                tracing::warn!(%event_id, %customer_id, "stripe customer maps to no tenant");
                BillingOutcome::UnknownCustomer
            }
            // D3 grace: the entitlement goes past-due but the org keeps its tier and quota
            // until the subscription is actually deleted. Stripe runs its own dunning flow,
            // and cutting quota on the first failed charge would punish a card that is about
            // to be retried successfully. There is deliberately no apply_tier call here.
            Some(_tenant) => {
                if store.mark_entitlement_past_due(&subscription_id).await? {
                    BillingOutcome::PastDue
                } else {
                    tracing::warn!(%event_id, %subscription_id, "past-due names an unknown subscription");
                    BillingOutcome::UnknownSubscription
                }
            }
        },

        BillingDecision::Cancel {
            customer_id,
            subscription_id,
        } => match store.tenant_for_stripe_customer(&customer_id).await? {
            None => {
                tracing::warn!(%event_id, %customer_id, "stripe customer maps to no tenant");
                BillingOutcome::UnknownCustomer
            }
            Some(tenant) => {
                if store.cancel_entitlement(&subscription_id).await? {
                    store.apply_tier(tenant, QuotaTier::Free).await?;
                    BillingOutcome::Canceled
                } else {
                    // No entitlement to cancel: do not downgrade an org on the strength of a
                    // subscription id we never issued.
                    tracing::warn!(%event_id, %subscription_id, "cancel names an unknown subscription");
                    BillingOutcome::UnknownSubscription
                }
            }
        },
    };

    store
        .record_billing_event(event_id, event_type, raw_payload, outcome.as_str())
        .await?;
    Ok(outcome)
}

/// Tests for the pure decision layer and the live-Postgres applier.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::QuotaConfig;
    use serde_json::json;
    use syntheos_contracts::{PrincipalId, TenantId};

    // ---- Fixtures ----

    /// Build a `customer.subscription.*` event with the given type, status, and price.
    fn subscription_event(
        event_id: &str,
        event_type: &str,
        customer: &str,
        subscription: &str,
        status: &str,
        price: &str,
    ) -> Value {
        json!({
            "id": event_id,
            "type": event_type,
            "data": { "object": {
                "id": subscription,
                "customer": customer,
                "status": status,
                "current_period_end": 1_710_000_000_i64,
                "items": { "data": [ { "price": { "id": price } } ] }
            }}
        })
    }

    /// Build an `invoice.payment_failed` event. `subscription` of `None` models a one-off
    /// invoice, which Stripe sends with a JSON null.
    fn invoice_failed_event(event_id: &str, customer: &str, subscription: Option<&str>) -> Value {
        json!({
            "id": event_id,
            "type": "invoice.payment_failed",
            "data": { "object": {
                "id": "in_1",
                "customer": customer,
                "subscription": match subscription {
                    Some(s) => json!(s),
                    None => Value::Null,
                }
            }}
        })
    }

    // ---- Pure decision tests ----

    /// An active subscription upserts an entitlement carrying the price id and a period end
    /// converted from Stripe's unix timestamp into RFC3339.
    #[test]
    fn created_active_upserts_entitlement() {
        let event = subscription_event(
            "evt_1",
            "customer.subscription.created",
            "cus_1",
            "sub_1",
            "active",
            "price_pro",
        );
        match decide(&event).expect("decides") {
            BillingDecision::UpsertEntitlement {
                customer_id,
                subscription_id,
                price_id,
                current_period_end,
            } => {
                assert_eq!(customer_id, "cus_1");
                assert_eq!(subscription_id, "sub_1");
                assert_eq!(price_id, "price_pro");
                let end = current_period_end.expect("period end present");
                assert!(end.starts_with("2024-03-"), "rfc3339 period end: {end}");
            }
            other => panic!("expected UpsertEntitlement, got {other:?}"),
        }
    }

    /// A trialing subscription is also good standing and upserts an entitlement.
    #[test]
    fn updated_trialing_upserts_entitlement() {
        let event = subscription_event(
            "evt_2",
            "customer.subscription.updated",
            "cus_1",
            "sub_1",
            "trialing",
            "price_pro",
        );
        assert!(matches!(
            decide(&event).expect("decides"),
            BillingDecision::UpsertEntitlement { .. }
        ));
    }

    /// A past_due subscription marks past-due rather than changing the tier.
    #[test]
    fn updated_past_due_marks_past_due() {
        let event = subscription_event(
            "evt_3",
            "customer.subscription.updated",
            "cus_1",
            "sub_1",
            "past_due",
            "price_pro",
        );
        assert!(matches!(
            decide(&event).expect("decides"),
            BillingDecision::MarkPastDue { .. }
        ));
    }

    /// A canceled status on an update cancels the entitlement.
    #[test]
    fn updated_canceled_cancels() {
        let event = subscription_event(
            "evt_4",
            "customer.subscription.updated",
            "cus_1",
            "sub_1",
            "canceled",
            "price_pro",
        );
        assert!(matches!(
            decide(&event).expect("decides"),
            BillingDecision::Cancel { .. }
        ));
    }

    /// FAIL-CLOSED: a status string we have never seen must mark past-due, never upgrade.
    /// This is the guard against Stripe adding a status after this code ships.
    #[test]
    fn unknown_status_marks_past_due_never_upgrades() {
        let event = subscription_event(
            "evt_5",
            "customer.subscription.updated",
            "cus_1",
            "sub_1",
            "some_status_stripe_invents_in_2027",
            "price_enterprise",
        );
        assert!(
            matches!(
                decide(&event).expect("decides"),
                BillingDecision::MarkPastDue { .. }
            ),
            "an unrecognized status must never produce an UpsertEntitlement"
        );
    }

    /// A deleted subscription cancels regardless of the status field.
    #[test]
    fn deleted_cancels_regardless_of_status() {
        let event = subscription_event(
            "evt_6",
            "customer.subscription.deleted",
            "cus_1",
            "sub_1",
            "active",
            "price_pro",
        );
        assert!(matches!(
            decide(&event).expect("decides"),
            BillingDecision::Cancel { .. }
        ));
    }

    /// A failed invoice tied to a subscription marks that subscription past-due.
    #[test]
    fn invoice_payment_failed_marks_past_due() {
        let event = invoice_failed_event("evt_7", "cus_1", Some("sub_1"));
        match decide(&event).expect("decides") {
            BillingDecision::MarkPastDue {
                subscription_id, ..
            } => assert_eq!(subscription_id, "sub_1"),
            other => panic!("expected MarkPastDue, got {other:?}"),
        }
    }

    /// A failed one-off invoice (null subscription) is ignored, not an error.
    #[test]
    fn invoice_payment_failed_without_subscription_is_ignored() {
        let event = invoice_failed_event("evt_8", "cus_1", None);
        assert!(matches!(
            decide(&event).expect("decides"),
            BillingDecision::Ignore { .. }
        ));
    }

    /// An event type we do not handle is ignored and names itself in the reason.
    #[test]
    fn unknown_event_type_is_ignored() {
        let event = json!({ "id": "evt_9", "type": "charge.refunded", "data": { "object": {} } });
        match decide(&event).expect("decides") {
            BillingDecision::Ignore { reason } => assert!(reason.contains("charge.refunded")),
            other => panic!("expected Ignore, got {other:?}"),
        }
    }

    /// An event with no `type` is a typed MissingField error.
    #[test]
    fn missing_type_is_missing_field() {
        let event = json!({ "id": "evt_10" });
        assert!(matches!(
            decide(&event).unwrap_err(),
            DecideError::MissingField("type")
        ));
    }

    /// A subscription event with no `data.object` is a typed MissingField error.
    #[test]
    fn missing_data_object_is_missing_field() {
        let event = json!({ "id": "evt_11", "type": "customer.subscription.created" });
        assert!(matches!(
            decide(&event).unwrap_err(),
            DecideError::MissingField("data.object")
        ));
    }

    /// An empty `items.data` array is a typed error, NOT an index panic.
    #[test]
    fn empty_items_data_is_missing_field_not_panic() {
        let event = json!({
            "id": "evt_12", "type": "customer.subscription.created",
            "data": { "object": {
                "id": "sub_1", "customer": "cus_1", "status": "active",
                "items": { "data": [] }
            }}
        });
        assert!(matches!(
            decide(&event).unwrap_err(),
            DecideError::MissingField(_)
        ));
    }

    /// An absent `current_period_end` yields `None` rather than an error.
    #[test]
    fn absent_period_end_is_none() {
        let event = json!({
            "id": "evt_13", "type": "customer.subscription.created",
            "data": { "object": {
                "id": "sub_1", "customer": "cus_1", "status": "active",
                "items": { "data": [ { "price": { "id": "price_pro" } } ] }
            }}
        });
        match decide(&event).expect("decides") {
            BillingDecision::UpsertEntitlement {
                current_period_end, ..
            } => assert!(current_period_end.is_none()),
            other => panic!("expected UpsertEntitlement, got {other:?}"),
        }
    }

    /// A body that is not JSON at all is a typed MalformedJson error, never a panic.
    #[test]
    fn parse_event_rejects_non_json() {
        assert!(matches!(
            parse_event(b"this is not json").unwrap_err(),
            DecideError::MalformedJson(_)
        ));
    }

    /// `parse_event` lifts the id and type out of a well-formed body.
    #[test]
    fn parse_event_extracts_id_and_type() {
        let body = br#"{"id":"evt_x","type":"customer.subscription.created"}"#;
        let event = parse_event(body).expect("parses");
        assert_eq!(event.id, "evt_x");
        assert_eq!(event.event_type, "customer.subscription.created");
    }

    /// Outcome text is the stable contract persisted to `billing_event.outcome`.
    #[test]
    fn outcome_strings_are_stable() {
        assert_eq!(
            BillingOutcome::Applied {
                tier: QuotaTier::Pro
            }
            .as_str(),
            "applied"
        );
        assert_eq!(BillingOutcome::Canceled.as_str(), "canceled");
        assert_eq!(BillingOutcome::PastDue.as_str(), "past_due");
        assert_eq!(BillingOutcome::Ignored.as_str(), "ignored");
        assert_eq!(BillingOutcome::UnmappedPrice.as_str(), "unmapped_price");
        assert_eq!(BillingOutcome::UnknownCustomer.as_str(), "unknown_customer");
        assert_eq!(
            BillingOutcome::UnknownSubscription.as_str(),
            "unknown_subscription"
        );
        assert_eq!(BillingOutcome::Replayed.as_str(), "replayed");
    }

    // ---- Live applier tests (skip without a Postgres URL) ----

    /// Connect to the live test database, or return `None` so the offline build stays green.
    ///
    /// Mirrors the `live_store` helper in `store.rs`; the two cannot share code because that
    /// one is private to its own test module.
    async fn live_store() -> Option<PlutusStore> {
        let url = match std::env::var("SYNTHEOS_PLUTUS_TEST_PG_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!(
                    "[plutus live test] SYNTHEOS_PLUTUS_TEST_PG_URL not set -- skipping live pipeline tests"
                );
                return None;
            }
        };
        Some(PlutusStore::open(&url).await.expect("open test store"))
    }

    /// A fixture org with a registered Stripe customer, returning (tenant, customer id).
    ///
    /// Every identifier is derived from a fresh `TenantId` so tests never collide with each
    /// other or with rows left behind by a previous run against the same database.
    async fn org_with_customer(store: &PlutusStore, name: &str) -> (TenantId, String) {
        let tenant = TenantId::new();
        store
            .create_org(tenant, name, PrincipalId::new(), QuotaTier::Free)
            .await
            .expect("create_org");
        let customer_id = format!("cus_{}", tenant.as_uuid());
        store
            .upsert_billing_customer(tenant, &customer_id)
            .await
            .expect("upsert_billing_customer");
        (tenant, customer_id)
    }

    /// Run one event end to end: decide it, then apply it.
    async fn run(store: &PlutusStore, event: &Value) -> BillingOutcome {
        let parsed = parse_event(event.to_string().as_bytes()).expect("parse_event");
        let decision = decide(&parsed.value).expect("decide");
        apply_decision(
            store,
            &parsed.id,
            &parsed.event_type,
            &parsed.value.to_string(),
            decision,
        )
        .await
        .expect("apply_decision")
    }

    /// Live: an active subscription applies its tier and rewrites the org's quota config.
    #[tokio::test]
    async fn live_subscription_created_applies_tier() {
        let Some(store) = live_store().await else {
            return;
        };
        let (tenant, customer) = org_with_customer(&store, "pipeline-created").await;
        let price = format!("price_pro_{}", tenant.as_uuid());
        store
            .insert_price_mapping(&price, QuotaTier::Pro)
            .await
            .expect("insert_price_mapping");

        let sub = format!("sub_{}", tenant.as_uuid());
        let event = subscription_event(
            &format!("evt_created_{}", tenant.as_uuid()),
            "customer.subscription.created",
            &customer,
            &sub,
            "active",
            &price,
        );

        let outcome = run(&store, &event).await;
        assert_eq!(
            outcome,
            BillingOutcome::Applied {
                tier: QuotaTier::Pro
            }
        );
        assert_eq!(store.org_tier(tenant).await.unwrap(), Some(QuotaTier::Pro));
        assert_eq!(
            store.quota_config(tenant).await.unwrap(),
            Some(QuotaTier::Pro.defaults())
        );
    }

    /// Live: deleting the subscription cancels the entitlement and drops the org to Free.
    #[tokio::test]
    async fn live_subscription_deleted_downgrades_to_free() {
        let Some(store) = live_store().await else {
            return;
        };
        let (tenant, customer) = org_with_customer(&store, "pipeline-deleted").await;
        let price = format!("price_pro_{}", tenant.as_uuid());
        store
            .insert_price_mapping(&price, QuotaTier::Pro)
            .await
            .expect("insert_price_mapping");
        let sub = format!("sub_{}", tenant.as_uuid());

        run(
            &store,
            &subscription_event(
                &format!("evt_c_{}", tenant.as_uuid()),
                "customer.subscription.created",
                &customer,
                &sub,
                "active",
                &price,
            ),
        )
        .await;

        let outcome = run(
            &store,
            &subscription_event(
                &format!("evt_d_{}", tenant.as_uuid()),
                "customer.subscription.deleted",
                &customer,
                &sub,
                "canceled",
                &price,
            ),
        )
        .await;

        assert_eq!(outcome, BillingOutcome::Canceled);
        assert_eq!(store.org_tier(tenant).await.unwrap(), Some(QuotaTier::Free));
        assert_eq!(
            store.quota_config(tenant).await.unwrap(),
            Some(QuotaTier::Free.defaults())
        );
        let ent = store
            .entitlement_for_subscription(&sub)
            .await
            .unwrap()
            .expect("entitlement exists");
        assert_eq!(ent.status, EntitlementStatus::Canceled);
    }

    /// Live: D3 grace. A failed payment marks the entitlement past-due and leaves the org's
    /// quota exactly where it was. This is the most important assertion in the task: a card
    /// that fails once must not cost the customer their paid quota mid-cycle.
    #[tokio::test]
    async fn live_payment_failed_marks_past_due_without_quota_change() {
        let Some(store) = live_store().await else {
            return;
        };
        let (tenant, customer) = org_with_customer(&store, "pipeline-past-due").await;
        let price = format!("price_pro_{}", tenant.as_uuid());
        store
            .insert_price_mapping(&price, QuotaTier::Pro)
            .await
            .expect("insert_price_mapping");
        let sub = format!("sub_{}", tenant.as_uuid());

        run(
            &store,
            &subscription_event(
                &format!("evt_c_{}", tenant.as_uuid()),
                "customer.subscription.created",
                &customer,
                &sub,
                "active",
                &price,
            ),
        )
        .await;
        let pro_quota: QuotaConfig = QuotaTier::Pro.defaults();
        assert_eq!(
            store.quota_config(tenant).await.unwrap(),
            Some(pro_quota.clone())
        );

        let outcome = run(
            &store,
            &invoice_failed_event(
                &format!("evt_f_{}", tenant.as_uuid()),
                &customer,
                Some(&sub),
            ),
        )
        .await;

        assert_eq!(outcome, BillingOutcome::PastDue);
        let ent = store
            .entitlement_for_subscription(&sub)
            .await
            .unwrap()
            .expect("entitlement exists");
        assert_eq!(ent.status, EntitlementStatus::PastDue);
        assert_eq!(
            store.quota_config(tenant).await.unwrap(),
            Some(pro_quota),
            "D3 grace: a payment failure must not change quota"
        );
        assert_eq!(store.org_tier(tenant).await.unwrap(), Some(QuotaTier::Pro));
    }

    /// Live: an unmapped price changes nothing at all -- no tier, no entitlement row.
    #[tokio::test]
    async fn live_unmapped_price_changes_nothing() {
        let Some(store) = live_store().await else {
            return;
        };
        let (tenant, customer) = org_with_customer(&store, "pipeline-unmapped").await;
        let sub = format!("sub_{}", tenant.as_uuid());
        let event = subscription_event(
            &format!("evt_u_{}", tenant.as_uuid()),
            "customer.subscription.created",
            &customer,
            &sub,
            "active",
            "price_never_mapped",
        );

        assert_eq!(run(&store, &event).await, BillingOutcome::UnmappedPrice);
        assert_eq!(store.org_tier(tenant).await.unwrap(), Some(QuotaTier::Free));
        assert!(
            store
                .entitlement_for_subscription(&sub)
                .await
                .unwrap()
                .is_none(),
            "an unmapped price must not write an entitlement"
        );
    }

    /// Live: an unregistered Stripe customer changes nothing.
    #[tokio::test]
    async fn live_unknown_customer_changes_nothing() {
        let Some(store) = live_store().await else {
            return;
        };
        let stranger = TenantId::new();
        let sub = format!("sub_{}", stranger.as_uuid());
        let event = subscription_event(
            &format!("evt_uc_{}", stranger.as_uuid()),
            "customer.subscription.created",
            &format!("cus_never_registered_{}", stranger.as_uuid()),
            &sub,
            "active",
            "price_pro",
        );

        assert_eq!(run(&store, &event).await, BillingOutcome::UnknownCustomer);
        assert!(store
            .entitlement_for_subscription(&sub)
            .await
            .unwrap()
            .is_none());
    }

    /// Live: replaying the same event id applies nothing a second time.
    #[tokio::test]
    async fn live_replayed_event_is_noop() {
        let Some(store) = live_store().await else {
            return;
        };
        let (tenant, customer) = org_with_customer(&store, "pipeline-replay").await;
        let price = format!("price_pro_{}", tenant.as_uuid());
        store
            .insert_price_mapping(&price, QuotaTier::Pro)
            .await
            .expect("insert_price_mapping");
        let sub = format!("sub_{}", tenant.as_uuid());
        let event = subscription_event(
            &format!("evt_replay_{}", tenant.as_uuid()),
            "customer.subscription.created",
            &customer,
            &sub,
            "active",
            &price,
        );

        assert_eq!(
            run(&store, &event).await,
            BillingOutcome::Applied {
                tier: QuotaTier::Pro
            }
        );
        assert_eq!(
            run(&store, &event).await,
            BillingOutcome::Replayed,
            "the second delivery of an event id must apply nothing"
        );
        assert_eq!(store.org_tier(tenant).await.unwrap(), Some(QuotaTier::Pro));
    }

    /// Live: canceling a subscription we never issued records the gap and downgrades nobody.
    #[tokio::test]
    async fn live_cancel_unknown_subscription_records_and_changes_nothing() {
        let Some(store) = live_store().await else {
            return;
        };
        let (tenant, customer) = org_with_customer(&store, "pipeline-unknown-sub").await;
        let price = format!("price_pro_{}", tenant.as_uuid());
        store
            .insert_price_mapping(&price, QuotaTier::Pro)
            .await
            .expect("insert_price_mapping");
        // Put the org on Pro so a spurious downgrade would be visible.
        store
            .apply_tier(tenant, QuotaTier::Pro)
            .await
            .expect("apply_tier");

        let event = subscription_event(
            &format!("evt_us_{}", tenant.as_uuid()),
            "customer.subscription.deleted",
            &customer,
            &format!("sub_never_issued_{}", tenant.as_uuid()),
            "canceled",
            &price,
        );

        assert_eq!(
            run(&store, &event).await,
            BillingOutcome::UnknownSubscription
        );
        assert_eq!(
            store.org_tier(tenant).await.unwrap(),
            Some(QuotaTier::Pro),
            "an unknown subscription must not downgrade the org"
        );
    }
}
