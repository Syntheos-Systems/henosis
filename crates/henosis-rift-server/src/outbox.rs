//! Durable message-event publication from PostgreSQL into the local gateway.

use std::time::Duration;

use sqlx::{PgConnection, PgPool};
use tokio::sync::watch;
use uuid::Uuid;

use crate::ws::gateway::{Gateway, GatewayEvent};

/// Delay before polling again when no pending event can be claimed.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Maximum idle polling delay after repeated empty claims.
const MAX_IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Interval between bounded cleanup passes for already delivered rows.
const DELIVERED_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Short delay between full cleanup batches while a retention backlog exists.
const DELIVERED_CLEANUP_CATCH_UP_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum delivered rows removed by one cleanup transaction.
const DELIVERED_CLEANUP_BATCH_SIZE: i64 = 256;

/// One locked pending row whose transaction remains open through publication.
struct PendingOutboxEvent {
    /// Stable event identity persisted independently from its serialized copy.
    event_id: Uuid,
    /// Local gateway route discriminator.
    route_kind: String,
    /// Channel identifier used as the local gateway routing key.
    route_id: Uuid,
    /// Exact serialized gateway event retained across retries.
    payload: String,
    /// Prior acknowledged deliveries; crash retries are correlated only by stable event ID.
    prior_acknowledged_deliveries: i64,
}

/// Failure that makes the supervised outbox dispatcher unsafe to continue.
#[derive(Debug, thiserror::Error)]
pub enum OutboxDispatchError {
    /// PostgreSQL could not claim or acknowledge a durable event.
    #[error("event outbox database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    /// A persisted payload no longer matches the Gateway event schema.
    #[error("event outbox row {event_id} contains an invalid payload: {source}")]
    InvalidPayload {
        /// Stable row identity used to locate the corrupt payload.
        event_id: Uuid,
        /// Serialization failure returned while decoding the payload.
        #[source]
        source: serde_json::Error,
    },
    /// A persisted row and its payload disagree about their stable identity or route.
    #[error("event outbox row {event_id} has inconsistent durable routing identity")]
    IdentityMismatch {
        /// Stable row identity used to locate the inconsistent payload.
        event_id: Uuid,
    },
    /// A persisted row contains an unsupported local route kind.
    #[error("event outbox row {event_id} has unsupported route kind {route_kind:?}")]
    UnsupportedRoute {
        /// Stable row identity used to locate the unsupported route.
        event_id: Uuid,
        /// Persisted route discriminator rejected by this dispatcher.
        route_kind: String,
    },
    /// The local gateway rejected a payload whose embedded route was inconsistent.
    #[error("local gateway rejected event outbox row {event_id}")]
    PublicationRejected {
        /// Stable row identity rejected by the local gateway.
        event_id: Uuid,
    },
    /// The locked row disappeared before its delivery acknowledgement could be written.
    #[error("event outbox row {event_id} lost its delivery claim")]
    DeliveryClaimLost {
        /// Stable row identity whose acknowledgement did not update one row.
        event_id: Uuid,
    },
}

/// Supervised single-process publisher for durable message gateway events.
pub struct OutboxDispatcher {
    /// Shared database pool containing the durable outbox.
    pool: PgPool,
    /// Local in-memory gateway that owns this process's subscribers.
    gateway: Gateway,
    /// Delay used only when every pending row is absent or currently locked.
    poll_interval: Duration,
    /// Test-only mode that waits for a stop signal without accessing PostgreSQL.
    #[cfg(test)]
    idle_for_test: bool,
}

/// Constructs and runs the local durable-event dispatcher.
impl OutboxDispatcher {
    /// Build a dispatcher over the same pool and gateway used by Rift routes.
    pub fn new(pool: PgPool, gateway: Gateway) -> Self {
        Self {
            pool,
            gateway,
            poll_interval: DEFAULT_POLL_INTERVAL,
            #[cfg(test)]
            idle_for_test: false,
        }
    }

    /// Build a dispatcher that only participates in lifecycle tests.
    #[cfg(test)]
    pub(crate) fn idle_for_test(pool: PgPool, gateway: Gateway) -> Self {
        Self {
            pool,
            gateway,
            poll_interval: DEFAULT_POLL_INTERVAL,
            idle_for_test: true,
        }
    }

    /// Publish pending rows until the supervisor requests a stop.
    pub async fn run(self, mut stop: watch::Receiver<bool>) -> Result<(), OutboxDispatchError> {
        #[cfg(test)]
        if self.idle_for_test {
            while !*stop.borrow() && stop.changed().await.is_ok() {}
            return Ok(());
        }
        let mut idle_delay = self.poll_interval;
        let mut next_cleanup = tokio::time::Instant::now();
        let mut cleanup_cycle_deleted = 0_u64;
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= next_cleanup {
                let deleted = self
                    .cleanup_expired_batch(DELIVERED_CLEANUP_BATCH_SIZE)
                    .await?;
                cleanup_cycle_deleted = cleanup_cycle_deleted.saturating_add(deleted);
                let cleanup_delay = next_cleanup_delay(deleted, DELIVERED_CLEANUP_BATCH_SIZE);
                tracing::debug!(
                    deleted,
                    cycle_deleted = cleanup_cycle_deleted,
                    catch_up = cleanup_delay == DELIVERED_CLEANUP_CATCH_UP_INTERVAL,
                    "completed bounded event outbox retention batch"
                );
                if cleanup_delay == DELIVERED_CLEANUP_INTERVAL {
                    if cleanup_cycle_deleted > 0 {
                        tracing::info!(
                            deleted = cleanup_cycle_deleted,
                            "completed event outbox retention catch-up cycle"
                        );
                    }
                    cleanup_cycle_deleted = 0;
                }
                next_cleanup = tokio::time::Instant::now() + cleanup_delay;
            }
            if self.dispatch_one_matching(None).await? {
                idle_delay = self.poll_interval;
                continue;
            }
            let wake_delay =
                idle_delay.min(next_cleanup.saturating_duration_since(tokio::time::Instant::now()));
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(wake_delay) => {
                    idle_delay = next_idle_delay(idle_delay);
                }
            }
        }
    }

    /// Remove one bounded batch of delivered rows older than the retention window.
    async fn cleanup_expired_batch(&self, batch_size: i64) -> Result<u64, OutboxDispatchError> {
        let deleted = sqlx::query_as::<_, (Uuid, i64)>(
            r#"WITH expired AS (
                   SELECT event_id
                   FROM event_outbox
                   WHERE delivered_at < NOW() - INTERVAL '7 days'
                   ORDER BY delivered_at, event_id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $1
               )
               DELETE FROM event_outbox AS events
               USING expired
               WHERE events.event_id = expired.event_id
               RETURNING events.event_id, events.attempt_count"#,
        )
        .bind(batch_size)
        .fetch_all(&self.pool)
        .await?;
        for (event_id, prior_acknowledged_deliveries) in &deleted {
            tracing::trace!(
                event_id = %event_id,
                prior_acknowledged_deliveries,
                "deleted expired delivered event outbox row"
            );
        }
        Ok(deleted.len() as u64)
    }

    /// Claim and publish at most one pending event, optionally constrained for a test.
    async fn dispatch_one_matching(
        &self,
        requested_event_id: Option<Uuid>,
    ) -> Result<bool, OutboxDispatchError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (Uuid, String, Uuid, String, i64)>(
            r#"SELECT event_id, route_kind, route_id, payload::TEXT, attempt_count
               FROM event_outbox
               WHERE delivered_at IS NULL
                 AND ($1::UUID IS NULL OR event_id = $1)
               ORDER BY created_at, event_id
               FOR UPDATE SKIP LOCKED
               LIMIT 1"#,
        )
        .bind(requested_event_id)
        .fetch_optional(&mut *transaction)
        .await?
        .map(
            |(event_id, route_kind, route_id, payload, prior_acknowledged_deliveries)| {
                PendingOutboxEvent {
                    event_id,
                    route_kind,
                    route_id,
                    payload,
                    prior_acknowledged_deliveries,
                }
            },
        );
        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(false);
        };
        tracing::debug!(
            event_id = %row.event_id,
            prior_acknowledged_deliveries = row.prior_acknowledged_deliveries,
            "claimed pending event outbox row"
        );

        if row.route_kind != "channel" {
            return Err(OutboxDispatchError::UnsupportedRoute {
                event_id: row.event_id,
                route_kind: row.route_kind,
            });
        }
        let event = serde_json::from_str::<GatewayEvent>(&row.payload).map_err(|source| {
            OutboxDispatchError::InvalidPayload {
                event_id: row.event_id,
                source,
            }
        })?;
        if event.message_outbox_identity() != Some((row.event_id, row.route_id)) {
            return Err(OutboxDispatchError::IdentityMismatch {
                event_id: row.event_id,
            });
        }
        if !self.gateway.broadcast_to_channel(row.route_id, event) {
            return Err(OutboxDispatchError::PublicationRejected {
                event_id: row.event_id,
            });
        }
        tracing::debug!(
            event_id = %row.event_id,
            prior_acknowledged_deliveries = row.prior_acknowledged_deliveries,
            "published event outbox row to local gateway"
        );

        let acknowledged = sqlx::query(
            r#"UPDATE event_outbox
               SET attempt_count = attempt_count + 1,
                   last_attempt_at = NOW(),
                   delivered_at = NOW()
               WHERE event_id = $1 AND delivered_at IS NULL"#,
        )
        .bind(row.event_id)
        .execute(&mut *transaction)
        .await?;
        if acknowledged.rows_affected() != 1 {
            return Err(OutboxDispatchError::DeliveryClaimLost {
                event_id: row.event_id,
            });
        }
        transaction.commit().await?;
        tracing::debug!(
            event_id = %row.event_id,
            prior_acknowledged_deliveries = row.prior_acknowledged_deliveries,
            "acknowledged event outbox delivery"
        );
        Ok(true)
    }
}

/// Double one empty-poll delay without exceeding the process latency bound.
fn next_idle_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_IDLE_POLL_INTERVAL)
}

/// Select the prompt catch-up or normal retention interval from one batch size.
fn next_cleanup_delay(deleted: u64, batch_size: i64) -> Duration {
    if batch_size > 0 && deleted == batch_size as u64 {
        DELIVERED_CLEANUP_CATCH_UP_INTERVAL
    } else {
        DELIVERED_CLEANUP_INTERVAL
    }
}

/// Insert one serialized message mutation event through its owning SQL transaction.
pub(crate) async fn enqueue_message_event(
    connection: &mut PgConnection,
    event: &GatewayEvent,
) -> Result<(), sqlx::Error> {
    let (event_id, channel_id) = event.message_outbox_identity().ok_or_else(|| {
        sqlx::Error::Protocol("only message mutations may enter the event outbox".to_string())
    })?;
    let payload = serde_json::to_string(event).map_err(|error| {
        sqlx::Error::Protocol(format!("gateway event serialization failed: {error}"))
    })?;
    sqlx::query(
        r#"INSERT INTO event_outbox (event_id, route_kind, route_id, payload)
           VALUES ($1, 'channel', $2, $3::JSONB)"#,
    )
    .bind(event_id)
    .bind(channel_id)
    .bind(payload)
    .execute(connection)
    .await?;
    Ok(())
}

/// Live PostgreSQL transaction, retry, and identity proofs for the durable outbox.
#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use sqlx::{PgPool, postgres::PgPoolOptions};
    use uuid::Uuid;

    use super::{
        DELIVERED_CLEANUP_CATCH_UP_INTERVAL, DELIVERED_CLEANUP_INTERVAL, MAX_IDLE_POLL_INTERVAL,
        OutboxDispatcher, PendingOutboxEvent, next_cleanup_delay, next_idle_delay,
    };
    use crate::db::{self, MessageWriteAuthorization, NewAttachment};
    use crate::ws::gateway::{Gateway, GatewayEvent};

    /// One isolated human-authored channel used by a live outbox test.
    struct LiveFixture {
        /// Human actor authorized by the route layer in production.
        user_id: Uuid,
        /// Channel whose messages and outbox events are isolated by UUID.
        channel_id: Uuid,
    }

    /// Connect to the explicitly configured live PostgreSQL test database and migrate it.
    async fn live_test_pool() -> Option<PgPool> {
        let database_url = match std::env::var("HENOSIS_RIFT_TEST_DATABASE_URL") {
            Ok(database_url) => database_url,
            Err(_) => {
                eprintln!("skipping live PostgreSQL test: HENOSIS_RIFT_TEST_DATABASE_URL is unset");
                return None;
            }
        };
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(&database_url)
            .await
            .expect("live PostgreSQL test database must accept connections");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("live PostgreSQL test database must accept Rift migrations");
        Some(pool)
    }

    /// Create a uniquely named user, server, and channel without generating an event.
    async fn live_fixture(pool: &PgPool) -> LiveFixture {
        let suffix = Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..12];
        let user = db::create_user(
            pool,
            &format!("outbox_{suffix}"),
            &format!("outbox-{suffix}@example.invalid"),
            "test-hash",
            Some("Outbox Test"),
        )
        .await
        .expect("outbox test user must be created");
        let server = db::create_server(pool, &format!("outbox-{suffix}"), None, user.id)
            .await
            .expect("outbox test server must be created");
        let channel = db::create_channel(pool, server.id, "outbox", None, "text")
            .await
            .expect("outbox test channel must be created");
        LiveFixture {
            user_id: user.id,
            channel_id: channel.id,
        }
    }

    /// Fetch every durable event for one message and optional mutation type.
    async fn stored_events(
        pool: &PgPool,
        message_id: Uuid,
        event_type: Option<&str>,
    ) -> Vec<PendingOutboxEvent> {
        sqlx::query_as::<_, (Uuid, String, Uuid, String, i64)>(
            r#"SELECT event_id, route_kind, route_id, payload::TEXT, attempt_count
               FROM event_outbox
               WHERE payload #>> '{data,id}' = $1
                 AND ($2::TEXT IS NULL OR payload ->> 'type' = $2)
               ORDER BY created_at, event_id"#,
        )
        .bind(message_id.to_string())
        .bind(event_type)
        .fetch_all(pool)
        .await
        .expect("stored outbox events must be readable")
        .into_iter()
        .map(
            |(event_id, route_kind, route_id, payload, prior_acknowledged_deliveries)| {
                PendingOutboxEvent {
                    event_id,
                    route_kind,
                    route_id,
                    payload,
                    prior_acknowledged_deliveries,
                }
            },
        )
        .collect()
    }

    /// Extract and verify the stable identity duplicated inside one serialized payload.
    fn assert_stable_identity(row: &PendingOutboxEvent) -> GatewayEvent {
        let event = serde_json::from_str::<GatewayEvent>(&row.payload)
            .expect("stored payload must deserialize as a gateway event");
        assert_eq!(
            event.message_outbox_identity(),
            Some((row.event_id, row.route_id))
        );
        assert_eq!(row.route_kind, "channel");
        event
    }

    /// Repeated idle polls back off exponentially and remain capped at one second.
    #[test]
    fn idle_poll_backoff_is_exponential_and_bounded() {
        let mut delay = Duration::from_millis(25);
        let expected = [50_u64, 100, 200, 400, 800, 1_000, 1_000];
        for expected_millis in expected {
            delay = next_idle_delay(delay);
            assert_eq!(delay, Duration::from_millis(expected_millis));
        }
        assert_eq!(delay, MAX_IDLE_POLL_INTERVAL);
    }

    /// Full retention batches catch up promptly while partial batches resume hourly cadence.
    #[test]
    fn retention_schedule_catches_up_until_a_partial_batch() {
        assert_eq!(
            next_cleanup_delay(256, 256),
            DELIVERED_CLEANUP_CATCH_UP_INTERVAL
        );
        assert_eq!(next_cleanup_delay(255, 256), DELIVERED_CLEANUP_INTERVAL);
        assert_eq!(next_cleanup_delay(0, 256), DELIVERED_CLEANUP_INTERVAL);
    }

    /// PostgreSQL rejects message-shaped payloads that omit either durable identity.
    #[tokio::test]
    async fn live_schema_requires_payload_event_and_route_identities() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let event_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        for payload in [
            serde_json::json!({
                "type": "MessageDelete",
                "data": { "channel_id": channel_id }
            }),
            serde_json::json!({
                "type": "MessageDelete",
                "data": { "event_id": event_id }
            }),
        ] {
            let error = sqlx::query(
                r#"INSERT INTO event_outbox (event_id, route_kind, route_id, payload)
                   VALUES ($1, 'channel', $2, $3)"#,
            )
            .bind(event_id)
            .bind(channel_id)
            .bind(payload)
            .execute(&pool)
            .await
            .expect_err("missing durable payload identity must violate a CHECK constraint");
            assert!(
                matches!(
                    error.as_database_error().and_then(|database| database.code()),
                    Some(code) if code == "23514"
                ),
                "expected PostgreSQL check_violation, got {error}"
            );
        }
    }

    /// Committed create, update, and delete mutations each retain one stable event identity.
    #[tokio::test]
    async fn live_committed_message_mutations_write_one_stable_event_each() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let fixture = live_fixture(&pool).await;
        let attachment = NewAttachment {
            filename: "evidence.txt".to_string(),
            url: "/uploads/evidence.txt".to_string(),
            content_type: Some("text/plain".to_string()),
            size_bytes: 8,
        };
        let (message, attachments) = db::create_message_with_attachments(
            &pool,
            fixture.channel_id,
            fixture.user_id,
            "created",
            "user",
            MessageWriteAuthorization::Human,
            &[attachment],
        )
        .await
        .expect("message and create event must commit together");
        assert_eq!(attachments.len(), 1);
        db::update_message_with_fence(
            &pool,
            message.id,
            fixture.channel_id,
            fixture.user_id,
            None,
            "updated",
        )
        .await
        .expect("message update and event must commit together");
        db::delete_message_with_fence(&pool, message.id, fixture.channel_id, fixture.user_id, None)
            .await
            .expect("message delete and event must commit together");

        let rows = stored_events(&pool, message.id, None).await;
        assert_eq!(
            rows.len(),
            3,
            "each durable mutation needs exactly one event"
        );
        let event_ids = rows
            .iter()
            .map(|row| {
                assert_eq!(row.route_id, fixture.channel_id);
                assert_stable_identity(row);
                row.event_id
            })
            .collect::<HashSet<_>>();
        assert_eq!(event_ids.len(), 3, "separate mutations need separate IDs");
        for event_type in ["MessageCreate", "MessageUpdate", "MessageDelete"] {
            assert_eq!(
                stored_events(&pool, message.id, Some(event_type))
                    .await
                    .len(),
                1
            );
        }
    }

    /// Rolling back each mutation removes both its state change and its pending event.
    #[tokio::test]
    async fn live_rolled_back_message_mutations_leave_no_state_or_event() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let fixture = live_fixture(&pool).await;

        let mut create_transaction = pool.begin().await.expect("create transaction must begin");
        let (rolled_back_message, _) = db::create_message_with_attachments_in_transaction(
            &mut create_transaction,
            fixture.channel_id,
            fixture.user_id,
            "rolled back create",
            "user",
            MessageWriteAuthorization::Human,
            &[],
        )
        .await
        .expect("create mutation must reach its caller-owned transaction");
        let create_event_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM event_outbox WHERE payload #>> '{data,id}' = $1",
        )
        .bind(rolled_back_message.id.to_string())
        .fetch_one(&mut *create_transaction)
        .await
        .expect("uncommitted create event must be visible to its transaction");
        assert_eq!(create_event_count, 1);
        create_transaction
            .rollback()
            .await
            .expect("create transaction must roll back");
        assert!(
            db::get_message_by_id(&pool, rolled_back_message.id)
                .await
                .expect("rolled-back message lookup must succeed")
                .is_none()
        );
        assert!(
            stored_events(&pool, rolled_back_message.id, None)
                .await
                .is_empty()
        );

        let message = db::create_message(
            &pool,
            fixture.channel_id,
            fixture.user_id,
            "original",
            "user",
        )
        .await
        .expect("baseline message must commit");

        let mut update_transaction = pool.begin().await.expect("update transaction must begin");
        db::update_message_in_transaction(
            &mut update_transaction,
            message.id,
            fixture.channel_id,
            fixture.user_id,
            None,
            "rolled back update",
        )
        .await
        .expect("update mutation must reach its caller-owned transaction");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM event_outbox WHERE payload #>> '{data,id}' = $1 AND payload ->> 'type' = 'MessageUpdate'",
            )
            .bind(message.id.to_string())
            .fetch_one(&mut *update_transaction)
            .await
            .expect("uncommitted update event must be visible"),
            1
        );
        update_transaction
            .rollback()
            .await
            .expect("update transaction must roll back");
        assert_eq!(
            db::get_message_by_id(&pool, message.id)
                .await
                .expect("baseline message lookup must succeed")
                .expect("baseline message must survive update rollback")
                .content,
            "original"
        );
        assert!(
            stored_events(&pool, message.id, Some("MessageUpdate"))
                .await
                .is_empty()
        );

        let mut delete_transaction = pool.begin().await.expect("delete transaction must begin");
        db::delete_message_in_transaction(
            &mut delete_transaction,
            message.id,
            fixture.channel_id,
            fixture.user_id,
            None,
        )
        .await
        .expect("delete mutation must reach its caller-owned transaction");
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM messages WHERE id = $1)")
                .bind(message.id)
                .fetch_one(&mut *delete_transaction)
                .await
                .expect("uncommitted delete state must be readable")
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM event_outbox WHERE payload #>> '{data,id}' = $1 AND payload ->> 'type' = 'MessageDelete'",
            )
            .bind(message.id.to_string())
            .fetch_one(&mut *delete_transaction)
            .await
            .expect("uncommitted delete event must be visible"),
            1
        );
        delete_transaction
            .rollback()
            .await
            .expect("delete transaction must roll back");
        assert!(
            db::get_message_by_id(&pool, message.id)
                .await
                .expect("baseline message lookup must succeed")
                .is_some()
        );
        assert!(
            stored_events(&pool, message.id, Some("MessageDelete"))
                .await
                .is_empty()
        );
    }

    /// A released row lock retries a published-but-unacknowledged event with the same ID.
    #[tokio::test]
    async fn live_locked_row_recovers_with_same_event_identity() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let fixture = live_fixture(&pool).await;
        let message = db::create_message(
            &pool,
            fixture.channel_id,
            fixture.user_id,
            "retry me",
            "user",
        )
        .await
        .expect("message and pending event must commit");
        let rows = stored_events(&pool, message.id, Some("MessageCreate")).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        let original = assert_stable_identity(row);

        let gateway = Gateway::new();
        let mut receiver = gateway.subscribe_channel(fixture.channel_id);
        let dispatcher = OutboxDispatcher::new(pool.clone(), gateway.clone());
        let mut interrupted = pool.begin().await.expect("interrupted delivery must begin");
        sqlx::query("SELECT event_id FROM event_outbox WHERE event_id = $1 FOR UPDATE")
            .bind(row.event_id)
            .fetch_one(&mut *interrupted)
            .await
            .expect("interrupted dispatcher must lock its exact event");

        assert!(
            !dispatcher
                .dispatch_one_matching(Some(row.event_id))
                .await
                .expect("SKIP LOCKED claim must not fail"),
            "a competing dispatcher must skip the locked row"
        );
        assert!(gateway.broadcast_to_channel(fixture.channel_id, original.clone()));
        let first = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("simulated pre-crash publication must arrive")
            .expect("test receiver must not lag");
        interrupted
            .rollback()
            .await
            .expect("simulated crash must release the delivery transaction");

        assert!(
            dispatcher
                .dispatch_one_matching(Some(row.event_id))
                .await
                .expect("released row must be retried")
        );
        let second = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("retry publication must arrive")
            .expect("test receiver must not lag");
        assert_eq!(
            first.message_outbox_identity(),
            Some((row.event_id, fixture.channel_id))
        );
        assert_eq!(
            second.message_outbox_identity(),
            Some((row.event_id, fixture.channel_id))
        );
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap(),
            "at-least-once retry must preserve the exact serialized event"
        );
        let (attempt_count, delivered): (i64, bool) = sqlx::query_as(
            "SELECT attempt_count, delivered_at IS NOT NULL FROM event_outbox WHERE event_id = $1",
        )
        .bind(row.event_id)
        .fetch_one(&pool)
        .await
        .expect("delivery acknowledgement must remain queryable");
        assert_eq!(attempt_count, 1);
        assert!(delivered);
    }

    /// Retention deletes only an explicit batch of old delivered rows and never pending rows.
    #[tokio::test]
    async fn live_retention_is_bounded_and_preserves_pending_rows() {
        let Some(pool) = live_test_pool().await else {
            return;
        };
        let fixture = live_fixture(&pool).await;
        let mut message_ids = Vec::new();
        for content in ["old one", "old two", "recent", "pending"] {
            let message =
                db::create_message(&pool, fixture.channel_id, fixture.user_id, content, "user")
                    .await
                    .expect("retention fixture message must commit");
            message_ids.push(message.id);
        }
        for message_id in &message_ids[..2] {
            sqlx::query(
                r#"UPDATE event_outbox
                   SET attempt_count = 1,
                       last_attempt_at = NOW() - INTERVAL '8 days',
                       delivered_at = NOW() - INTERVAL '8 days'
                   WHERE payload #>> '{data,id}' = $1"#,
            )
            .bind(message_id.to_string())
            .execute(&pool)
            .await
            .expect("old delivered fixture must be marked");
        }
        sqlx::query(
            r#"UPDATE event_outbox
               SET attempt_count = 1,
                   last_attempt_at = NOW(),
                   delivered_at = NOW()
               WHERE payload #>> '{data,id}' = $1"#,
        )
        .bind(message_ids[2].to_string())
        .execute(&pool)
        .await
        .expect("recent delivered fixture must be marked");
        let dispatcher = OutboxDispatcher::new(pool.clone(), Gateway::new());

        assert_eq!(dispatcher.cleanup_expired_batch(1).await.unwrap(), 1);
        let old_remaining = sqlx::query_scalar::<_, i64>(
            r#"SELECT COUNT(*)
               FROM event_outbox
               WHERE payload #>> '{data,id}' = ANY($1)
                 AND delivered_at < NOW() - INTERVAL '7 days'"#,
        )
        .bind(
            message_ids[..2]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        )
        .fetch_one(&pool)
        .await
        .expect("old delivered rows must be countable");
        assert_eq!(old_remaining, 1, "one-row cleanup must remain bounded");
        assert_eq!(dispatcher.cleanup_expired_batch(1).await.unwrap(), 1);
        assert!(stored_events(&pool, message_ids[0], None).await.is_empty());
        assert!(stored_events(&pool, message_ids[1], None).await.is_empty());
        assert_eq!(stored_events(&pool, message_ids[2], None).await.len(), 1);
        let pending = stored_events(&pool, message_ids[3], None).await;
        assert_eq!(pending.len(), 1);
        let (attempt_count, delivered_at): (i64, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as(
                "SELECT attempt_count, delivered_at FROM event_outbox WHERE event_id = $1",
            )
            .bind(pending[0].event_id)
            .fetch_one(&pool)
            .await
            .expect("pending row must survive retention");
        assert_eq!(attempt_count, 0);
        assert!(delivered_at.is_none());
    }
}
