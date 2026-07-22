use crate::db::Database;
use crate::{EngError, Result};
use serde::{Deserialize, Serialize};

/// Represents a published Axon event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub channel: String,
    pub action: String,
    pub payload: serde_json::Value,
    pub source: Option<String>,
    pub agent: Option<String>,
    pub user_id: i64,
    pub created_at: String,
}

/// Request payload for publishing an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishEventRequest {
    pub channel: String,
    pub action: String,
    pub payload: Option<serde_json::Value>,
    pub source: Option<String>,
    pub agent: Option<String>,
    pub user_id: Option<i64>,
}

/// Aggregate statistics for the Axon event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxonStats {
    pub total_events: i64,
    pub channels: i64,
    pub sources: i64,
    /// Per-channel breakdown: event count and most recent `created_at`. Ports
    /// the standalone axon `/stats.by_channel` payload.
    #[serde(default)]
    pub by_channel: Vec<ChannelStat>,
}

/// One row of the per-channel stats breakdown returned by [`get_stats`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelStat {
    pub channel: String,
    pub count: i64,
    pub latest: Option<String>,
}

/// An Axon pub/sub channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub retain_hours: i64,
    pub created_at: String,
    #[serde(default)]
    pub event_count: i64,
    #[serde(default)]
    pub subscriber_count: i64,
}

/// An agent's subscription to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: i64,
    pub agent: String,
    pub channel: String,
    pub filter_type: Option<String>,
    pub webhook_url: Option<String>,
    pub user_id: i64, // owning tenant; re-added to the table by migration 97
    pub created_at: String,
}

/// Tracks an agent's read position in a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    pub agent: String,
    pub channel: String,
    pub last_event_id: i64,
    pub updated_at: String,
    pub user_id: i64, // owning tenant; re-added to the table by migration 98
}

/// Request payload for subscribing to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeRequest {
    pub agent: String,
    pub channel: String,
    pub filter_type: Option<String>,
    pub webhook_url: Option<String>,
}

/// Converts a rusqlite Row into an Event. `owner_user_id` fills `Event.user_id`
/// (the column is not in `EVENT_COLUMNS`); correctness comes from the
/// always-applied `user_id` predicate, so the value is the caller's id.
fn row_to_event(row: &rusqlite::Row<'_>, owner_user_id: i64) -> Result<Event> {
    let payload_str: String = row.get(4)?;
    let payload: serde_json::Value = serde_json::from_str(&payload_str)?;
    let source: String = row.get(2)?;
    Ok(Event {
        id: row.get(0)?,
        channel: row.get(1)?,
        source: Some(source.clone()),
        agent: Some(source),
        action: row.get(3)?,
        payload,
        created_at: row.get(5)?,
        user_id: owner_user_id,
    })
}

const EVENT_COLUMNS: &str = "id, channel, source, type, payload, created_at";

/// Resolves the event source, defaulting to "unknown".
fn resolve_source(req: &PublishEventRequest) -> String {
    req.source
        .clone()
        .or_else(|| req.agent.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Publishes an event to a channel.
#[tracing::instrument(skip(db, req), fields(channel = %req.channel, action = %req.action))]
pub async fn publish_event(db: &Database, req: PublishEventRequest) -> Result<Event> {
    let payload = req
        .payload
        .clone()
        .unwrap_or(serde_json::Value::Object(Default::default()));
    let payload_str = serde_json::to_string(&payload)?;
    let user_id = req.user_id.unwrap_or(1);
    let source = resolve_source(&req);
    let channel = req.channel.clone();
    let action = req.action.clone();

    let id = db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO axon_events (channel, source, type, payload, user_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![channel, source, action, payload_str, user_id],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await?;
    get_event(db, id, user_id).await
}

/// Retrieves a single event by ID.
#[tracing::instrument(skip(db), fields(event_id = id, user_id))]
pub async fn get_event(db: &Database, id: i64, user_id: i64) -> Result<Event> {
    let sql = format!("SELECT {EVENT_COLUMNS} FROM axon_events WHERE id = ?1 AND user_id = ?2");

    db.read(move |conn| {
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params![id, user_id])?;
        let row = rows
            .next()?
            .ok_or_else(|| EngError::NotFound(format!("event {}", id)))?;
        row_to_event(row, user_id)
    })
    .await
}

/// Queries events with optional filters.
#[tracing::instrument(skip(db), fields(channel = ?channel, action = ?action, source = ?source, limit, offset, user_id))]
pub async fn query_events(
    db: &Database,
    channel: Option<&str>,
    action: Option<&str>,
    source: Option<&str>,
    limit: usize,
    offset: usize,
    user_id: i64,
) -> Result<Vec<Event>> {
    // user_id is always the first bound parameter; channel/action/source append.
    let mut clauses: Vec<String> = vec!["user_id = ?1".to_string()];
    let mut param_idx = 2usize;
    let mut params_vec: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Integer(user_id)];

    if let Some(c) = channel {
        clauses.push(format!("channel = ?{}", param_idx));
        params_vec.push(rusqlite::types::Value::Text(c.to_string()));
        param_idx += 1;
    }
    if let Some(a) = action {
        clauses.push(format!("type = ?{}", param_idx));
        params_vec.push(rusqlite::types::Value::Text(a.to_string()));
        param_idx += 1;
    }
    if let Some(s) = source {
        clauses.push(format!("source = ?{}", param_idx));
        params_vec.push(rusqlite::types::Value::Text(s.to_string()));
        param_idx += 1;
    }
    let mut sql = format!("SELECT {EVENT_COLUMNS} FROM axon_events WHERE ");
    sql.push_str(&clauses.join(" AND "));
    sql.push_str(&format!(
        " ORDER BY id DESC LIMIT ?{} OFFSET ?{}",
        param_idx,
        param_idx + 1
    ));
    params_vec.push(rusqlite::types::Value::Integer(limit as i64));
    params_vec.push(rusqlite::types::Value::Integer(offset as i64));

    db.read(move |conn| {
        let mut stmt = conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(params_vec.iter().cloned());
        let mut rows = stmt.query(params)?;
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push(row_to_event(row, user_id)?);
        }
        Ok(results)
    })
    .await
}

/// Lists all Axon channels with the caller's per-user event count. Channel
/// metadata is a shared namespace, but `event_count` is scoped to `user_id` so
/// the caller does not learn how many events other users published in
/// single-DB mode.
#[tracing::instrument(skip(db), fields(user_id))]
pub async fn list_channels(db: &Database, user_id: i64) -> Result<Vec<Channel>> {
    let sql = "SELECT c.id, c.name, c.description, c.retain_hours, c.created_at,
                      (SELECT COUNT(*) FROM axon_events WHERE channel = c.name AND user_id = ?1) as event_count,
                      (SELECT COUNT(*) FROM axon_subscriptions WHERE channel = c.name) as subscriber_count
               FROM axon_channels c ORDER BY c.name ASC";

    db.read(move |conn| {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params![user_id])?;
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push(Channel {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                retain_hours: row.get(3)?,
                created_at: row.get(4)?,
                event_count: row.get(5)?,
                subscriber_count: row.get(6)?,
            });
        }
        Ok(results)
    })
    .await
}

/// Creates a channel if it does not exist.
#[tracing::instrument(skip(db, description), fields(name = %name))]
pub async fn ensure_channel(
    db: &Database,
    name: String,
    description: Option<String>,
) -> Result<()> {
    let sql = "INSERT INTO axon_channels (name, description)
               VALUES (?1, ?2)
               ON CONFLICT(name) DO NOTHING";

    db.write(move |conn| {
        conn.execute(sql, rusqlite::params![name, description])?;
        Ok(())
    })
    .await
}

/// Creates or updates a subscription.
#[tracing::instrument(skip(db, req), fields(agent = %req.agent, channel = %req.channel, user_id))]
pub async fn upsert_subscription(
    db: &Database,
    req: SubscribeRequest,
    user_id: i64,
) -> Result<Subscription> {
    // user_id is bound and included in the ON CONFLICT target so subscriptions
    // isolate per tenant on a shared axon table (see migration 97).
    let sql = "INSERT INTO axon_subscriptions (agent, channel, filter_type, webhook_url, user_id)
               VALUES (?1, ?2, ?3, ?4, ?5)
               ON CONFLICT(agent, channel, user_id) DO UPDATE SET
                   filter_type = excluded.filter_type,
                   webhook_url = excluded.webhook_url";

    let a = req.agent.clone();
    let c = req.channel.clone();
    let ft = req.filter_type.clone();
    let wh = req.webhook_url.clone();
    db.write(move |conn| {
        conn.execute(sql, rusqlite::params![a, c, ft, wh, user_id])?;
        Ok(())
    })
    .await?;
    get_subscription(db, &req.agent, &req.channel, user_id).await
}

/// Retrieves a subscription by agent and channel.
#[tracing::instrument(skip(db), fields(agent = %agent, channel = %channel, user_id))]
pub async fn get_subscription(
    db: &Database,
    agent: &str,
    channel: &str,
    user_id: i64,
) -> Result<Subscription> {
    let sql = "SELECT id, agent, channel, filter_type, webhook_url, created_at
               FROM axon_subscriptions
               WHERE agent = ?1 AND channel = ?2 AND user_id = ?3";
    let agent_s = agent.to_string();
    let channel_s = channel.to_string();

    db.read(move |conn| {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params![agent_s, channel_s, user_id])?;
        let row = rows
            .next()?
            .ok_or_else(|| EngError::NotFound("subscription".into()))?;
        Ok(Subscription {
            id: row.get(0)?,
            agent: row.get(1)?,
            channel: row.get(2)?,
            filter_type: row.get(3)?,
            webhook_url: row.get(4)?,
            user_id,
            created_at: row.get(5)?,
        })
    })
    .await
}

/// Removes a subscription owned by `user_id`.
#[tracing::instrument(skip(db), fields(agent = %agent, channel = %channel, user_id))]
pub async fn delete_subscription(
    db: &Database,
    agent: &str,
    channel: &str,
    user_id: i64,
) -> Result<bool> {
    // Scope by user_id so one tenant cannot delete another's subscription on a
    // shared axon table.
    let sql = "DELETE FROM axon_subscriptions WHERE agent = ?1 AND channel = ?2 AND user_id = ?3";
    let a = agent.to_string();
    let c = channel.to_string();

    let n = db
        .write(move |conn| Ok(conn.execute(sql, rusqlite::params![a, c, user_id])?))
        .await?;
    Ok(n > 0)
}

/// Lists all subscriptions for an agent.
#[tracing::instrument(skip(db), fields(agent = %agent, user_id))]
pub async fn list_subscriptions_for_agent(
    db: &Database,
    agent: &str,
    user_id: i64,
) -> Result<Vec<Subscription>> {
    let sql = "SELECT id, agent, channel, filter_type, webhook_url, created_at
               FROM axon_subscriptions
               WHERE agent = ?1 AND user_id = ?2
               ORDER BY channel ASC";
    let a = agent.to_string();

    db.read(move |conn| {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params![a, user_id])?;
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push(Subscription {
                id: row.get(0)?,
                agent: row.get(1)?,
                channel: row.get(2)?,
                filter_type: row.get(3)?,
                webhook_url: row.get(4)?,
                user_id,
                created_at: row.get(5)?,
            });
        }
        Ok(results)
    })
    .await
}

/// Retrieves the cursor position for an agent on a channel.
#[tracing::instrument(skip(db), fields(agent = %agent, channel = %channel, user_id))]
pub async fn get_cursor(db: &Database, agent: &str, channel: &str, user_id: i64) -> Result<Cursor> {
    // Scope the cursor read by user_id: the (agent, channel) cursor is shared
    // across tenants on a shared axon table, so without this one tenant's
    // consume would advance a position another tenant then skips past.
    let sql = "SELECT agent, channel, last_event_id, updated_at
               FROM axon_cursors
               WHERE agent = ?1 AND channel = ?2 AND user_id = ?3";
    let a = agent.to_string();
    let c = channel.to_string();

    db.read(move |conn| {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params![a.clone(), c.clone(), user_id])?;
        match rows.next()? {
            Some(row) => Ok(Cursor {
                agent: row.get(0)?,
                channel: row.get(1)?,
                last_event_id: row.get(2)?,
                updated_at: row.get(3)?,
                user_id,
            }),
            None => Ok(Cursor {
                agent: a,
                channel: c,
                last_event_id: 0,
                updated_at: String::new(),
                user_id,
            }),
        }
    })
    .await
}

/// Creates or updates a cursor position.
async fn upsert_cursor(
    db: &Database,
    agent: &str,
    channel: &str,
    last_event_id: i64,
    user_id: i64,
) -> Result<()> {
    // user_id is part of the cursor key so each tenant advances its own cursor.
    let sql = "INSERT INTO axon_cursors (agent, channel, last_event_id, updated_at, user_id)
               VALUES (?1, ?2, ?3, datetime('now'), ?4)
               ON CONFLICT(agent, channel, user_id) DO UPDATE SET
                   last_event_id = excluded.last_event_id,
                   updated_at = excluded.updated_at";
    let a = agent.to_string();
    let c = channel.to_string();

    db.write(move |conn| {
        conn.execute(sql, rusqlite::params![a, c, last_event_id, user_id])?;
        Ok(())
    })
    .await
}

/// Consumes events from a cursor position forward.
#[tracing::instrument(skip(db), fields(agent = %agent, channel = %channel, limit, user_id))]
pub async fn consume(
    db: &Database,
    agent: &str,
    channel: &str,
    limit: usize,
    user_id: i64,
) -> Result<Vec<Event>> {
    let cursor = get_cursor(db, agent, channel, user_id).await?;
    let last = cursor.last_event_id;
    let sql = format!(
        "SELECT {EVENT_COLUMNS} FROM axon_events
         WHERE channel = ?1 AND id > ?2 AND user_id = ?4
         ORDER BY id ASC LIMIT ?3"
    );
    let channel_s = channel.to_string();

    let events: Vec<Event> = db
        .read(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params![channel_s, last, limit as i64, user_id])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(row_to_event(row, user_id)?);
            }
            Ok(out)
        })
        .await?;
    if let Some(max_id) = events.iter().map(|e| e.id).max() {
        upsert_cursor(db, agent, channel, max_id, user_id).await?;
    }
    Ok(events)
}

/// Returns aggregate Axon statistics including a per-channel breakdown, scoped
/// to `user_id` so counts isolate per user in single-DB mode.
#[tracing::instrument(skip(db), fields(user_id))]
pub async fn get_stats(db: &Database, user_id: i64) -> Result<AxonStats> {
    db.read(move |conn| {
        let (total_events, channels, sources) = conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT channel), COUNT(DISTINCT source)
                 FROM axon_events WHERE user_id = ?1",
            rusqlite::params![user_id],
            |row| {
                let total: i64 = row.get(0)?;
                let chans: i64 = row.get(1)?;
                let srcs: i64 = row.get(2)?;
                Ok((total, chans, srcs))
            },
        )?;

        let mut by_channel = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT channel, COUNT(*), MAX(created_at)
                 FROM axon_events
                 WHERE user_id = ?1
                 GROUP BY channel
                 ORDER BY channel ASC",
        )?;
        let mut rows = stmt.query(rusqlite::params![user_id])?;
        while let Some(row) = rows.next()? {
            by_channel.push(ChannelStat {
                channel: row.get(0)?,
                count: row.get(1)?,
                latest: row.get(2)?,
            });
        }

        Ok(AxonStats {
            total_events,
            channels,
            sources,
            by_channel,
        })
    })
    .await
}

/// Publishes an event internally (no HTTP). Used by other services (Loom, Soma, etc.)
/// to emit lifecycle events into the Axon bus.
///
/// `user_id` is the owning tenant of the event. It must be threaded from the
/// caller's context: in shared-monolith mode axon_events is one table across all
/// tenants, so a hardcoded id (the previous behavior) filed every service's
/// lifecycle events under user 1 and leaked one tenant's activity into another
/// tenant's event feed.
#[tracing::instrument(skip(db, payload), fields(%channel, %action, user_id))]
pub async fn publish_internal(
    db: &Database,
    channel: &str,
    source: &str,
    action: &str,
    payload: serde_json::Value,
    user_id: i64,
) -> Result<i64> {
    let req = PublishEventRequest {
        channel: channel.to_string(),
        action: action.to_string(),
        payload: Some(payload),
        source: Some(source.to_string()),
        agent: None,
        user_id: Some(user_id),
    };
    let event = publish_event(db, req).await?;
    Ok(event.id)
}

/// Unit tests.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    /// Creates an in-memory database for testing.
    async fn setup() -> Database {
        Database::connect_memory().await.expect("db")
    }

    /// Publishes an event and retrieves it by ID.
    #[tokio::test]
    async fn publish_and_get_event() {
        let db = setup().await;
        let ev = publish_event(
            &db,
            PublishEventRequest {
                channel: "test".into(),
                action: "ping".into(),
                payload: Some(serde_json::json!({"k": "v"})),
                source: Some("agent-a".into()),
                agent: None,
                user_id: Some(1),
            },
        )
        .await
        .expect("publish");
        assert_eq!(ev.channel, "test");
        assert_eq!(ev.action, "ping");
        assert_eq!(ev.source.as_deref(), Some("agent-a"));
        let fetched = get_event(&db, ev.id, 1).await.expect("get");
        assert_eq!(fetched.id, ev.id);
    }

    /// Consuming events advances the cursor position.
    #[tokio::test]
    async fn consume_advances_cursor() {
        let db = setup().await;
        for i in 0..3 {
            publish_event(
                &db,
                PublishEventRequest {
                    channel: "cons".into(),
                    action: format!("act-{i}"),
                    payload: None,
                    source: Some("src".into()),
                    agent: None,
                    user_id: Some(1),
                },
            )
            .await
            .expect("publish");
        }
        let first = consume(&db, "agent-x", "cons", 10, 1).await.expect("c1");
        assert_eq!(first.len(), 3);
        let second = consume(&db, "agent-x", "cons", 10, 1).await.expect("c2");
        assert!(second.is_empty());
    }

    /// Single-DB isolation: with user_id restored on axon_events (monolith
    /// migration 68 / tenant v59), a shared in-memory DB again separates user 1
    /// from user 2 on consume. The cross-shard invariant is also covered by
    /// kleos-lib/tests/tenant_isolation.rs::axon_events_isolated_across_tenants.
    #[tokio::test]
    async fn consume_is_scoped_by_user() {
        let db = setup().await;
        publish_event(
            &db,
            PublishEventRequest {
                channel: "s".into(),
                action: "a".into(),
                payload: None,
                source: Some("x".into()),
                agent: None,
                user_id: Some(1),
            },
        )
        .await
        .unwrap();
        let other = consume(&db, "who", "s", 10, 2).await.unwrap();
        assert!(other.is_empty());
    }

    /// Upserting the same subscription twice does not duplicate it.
    #[tokio::test]
    async fn subscription_upsert_is_idempotent() {
        let db = setup().await;
        upsert_subscription(
            &db,
            SubscribeRequest {
                agent: "a1".into(),
                channel: "c1".into(),
                filter_type: Some("ping".into()),
                webhook_url: None,
            },
            1,
        )
        .await
        .unwrap();
        let s2 = upsert_subscription(
            &db,
            SubscribeRequest {
                agent: "a1".into(),
                channel: "c1".into(),
                filter_type: Some("pong".into()),
                webhook_url: None,
            },
            1,
        )
        .await
        .unwrap();
        assert_eq!(s2.filter_type.as_deref(), Some("pong"));
        let all = list_subscriptions_for_agent(&db, "a1", 1).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    /// The seeded "system" channel appears in the channel list.
    #[tokio::test]
    async fn list_channels_returns_seeded() {
        let db = setup().await;
        let channels = list_channels(&db, 1).await.unwrap();
        assert!(channels.iter().any(|c| c.name == "system"));
    }

    /// Two tenants may hold a subscription on the same (agent, channel) after the
    /// UNIQUE key widened to include user_id (migration 97), and every read/write
    /// helper is scoped so one tenant cannot see, overwrite, or delete another's
    /// subscription row on the shared table.
    #[tokio::test]
    async fn subscription_isolated_across_users() {
        let db = setup().await;
        // Same agent + channel, two different owners, distinct webhook URLs.
        upsert_subscription(
            &db,
            SubscribeRequest {
                agent: "shared".into(),
                channel: "chan".into(),
                filter_type: None,
                webhook_url: Some("http://localhost:7001/a".into()),
            },
            1,
        )
        .await
        .unwrap();
        upsert_subscription(
            &db,
            SubscribeRequest {
                agent: "shared".into(),
                channel: "chan".into(),
                filter_type: None,
                webhook_url: Some("http://localhost:7002/b".into()),
            },
            2,
        )
        .await
        .expect("second tenant can subscribe the same agent/channel");

        // Each tenant reads only its own row.
        let s1 = get_subscription(&db, "shared", "chan", 1).await.unwrap();
        let s2 = get_subscription(&db, "shared", "chan", 2).await.unwrap();
        assert_eq!(s1.webhook_url.as_deref(), Some("http://localhost:7001/a"));
        assert_eq!(s2.webhook_url.as_deref(), Some("http://localhost:7002/b"));

        // Listing is scoped: tenant 1 sees exactly one row.
        let listed = list_subscriptions_for_agent(&db, "shared", 1)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);

        // Deleting tenant 1's row leaves tenant 2's intact.
        assert!(delete_subscription(&db, "shared", "chan", 1).await.unwrap());
        assert!(get_subscription(&db, "shared", "chan", 1).await.is_err());
        assert!(get_subscription(&db, "shared", "chan", 2).await.is_ok());
    }

    /// Each tenant's consume cursor is keyed by user_id (migration 98), so one
    /// tenant advancing its read position on a shared (agent, channel) does not
    /// cause another tenant to skip its own unread events.
    #[tokio::test]
    async fn cursor_isolated_across_users() {
        let db = setup().await;
        // Two events per tenant on the same channel.
        for owner in [1_i64, 2_i64] {
            for i in 0..2 {
                publish_event(
                    &db,
                    PublishEventRequest {
                        channel: "cc".into(),
                        action: format!("a-{owner}-{i}"),
                        payload: None,
                        source: Some("src".into()),
                        agent: None,
                        user_id: Some(owner),
                    },
                )
                .await
                .unwrap();
            }
        }

        // Tenant 1 consumes its two events and advances only its own cursor.
        let first = consume(&db, "reader", "cc", 10, 1).await.unwrap();
        assert_eq!(first.len(), 2);

        // Tenant 2's cursor is untouched, so it still sees its own two events
        // rather than being skipped past by tenant 1's advance.
        let other = consume(&db, "reader", "cc", 10, 2).await.unwrap();
        assert_eq!(other.len(), 2);

        // Cursors are independent rows.
        let c1 = get_cursor(&db, "reader", "cc", 1).await.unwrap();
        let c2 = get_cursor(&db, "reader", "cc", 2).await.unwrap();
        assert!(c1.last_event_id > 0);
        assert!(c2.last_event_id > 0);
        assert_ne!(c1.last_event_id, c2.last_event_id);
    }
}
