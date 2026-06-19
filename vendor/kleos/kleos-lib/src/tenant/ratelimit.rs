use crate::tenant::pool::TenantPools;
use crate::Result;

// z11-007: the in-memory per-process RateLimiter that used to live here was a
// dead duplicate of `crate::ratelimit::RateLimiter` (the live one, used by
// kleos-credd). Nothing constructed this one outside its own tests, so it was
// removed. Tenant-scoped rate limiting uses the DB-backed functions below.

// ---------------------------------------------------------------------------
// DB-backed rate limiter (persistent, fixed-window)
// ---------------------------------------------------------------------------

/// Check whether the given key is within the rate limit.
///
/// Uses a fixed-window strategy: counts requests recorded in [window_start, now).
/// Returns true if the request is allowed, false if the limit is exceeded.
#[tracing::instrument(skip(db), fields(key = %key, max_requests, window_seconds))]
pub async fn check_rate_limit(
    db: &TenantPools,
    key: &str,
    max_requests: i64,
    window_seconds: i64,
) -> Result<bool> {
    let key_owned = key.to_string();

    let row = db
        .read(move |conn| {
            let mut stmt =
                conn.prepare("SELECT count, window_start FROM rate_limits WHERE key = ?1")?;

            let mut rows = stmt.query(rusqlite::params![key_owned])?;

            match rows.next()? {
                Some(r) => {
                    let count: i64 = r.get(0)?;
                    let window_start: String = r.get(1)?;
                    Ok(Some((count, window_start)))
                }
                None => Ok(None),
            }
        })
        .await?;

    if let Some((count, window_start)) = row {
        let key_owned2 = key.to_string();
        let window_start2 = window_start.clone();

        let expired: i64 = db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT (strftime('%s', 'now') - strftime('%s', ?1)) > ?2",
                    rusqlite::params![window_start2, window_seconds],
                    |r| r.get(0),
                )?)
            })
            .await?;

        if expired != 0 {
            // Window has expired -- reset.
            db.write(move |conn| {
                conn.execute(
                    "UPDATE rate_limits
                     SET count = 0, window_start = datetime('now'), updated_at = datetime('now')
                     WHERE key = ?1",
                    rusqlite::params![key_owned2],
                )?;
                Ok(())
            })
            .await?;
            return Ok(true);
        }

        Ok(count < max_requests)
    } else {
        // No row yet means this key has never been seen -- allow.
        Ok(true)
    }
}

/// Increment the request counter for a key.
///
/// Creates the row if it does not exist. Resets the window when expired.
#[tracing::instrument(skip(db), fields(key = %key))]
pub async fn increment_counter(db: &TenantPools, key: &str) -> Result<()> {
    let key_owned = key.to_string();

    db.write(move |conn| {
        conn.execute(
            "INSERT INTO rate_limits (key, count, window_start)
             VALUES (?1, 1, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET
                 count = count + 1,
                 updated_at = datetime('now')",
            rusqlite::params![key_owned],
        )?;
        Ok(())
    })
    .await?;

    Ok(())
}

/// Atomically increment the request counter and return whether the request
/// is within the limit. Resets the window in-place when it has expired.
///
/// SECURITY: the previous `check_rate_limit` + `increment_counter` sequence
/// had a time-of-check-to-time-of-use race. Two concurrent requests could
/// both read `count < limit`, both pass, and both increment -- bursting past
/// the limit. This collapses the check and the increment into one SQL
/// statement so the atomicity of a single UPDATE under SQLite's writer lock
/// provides the needed serialization.
#[tracing::instrument(skip(db), fields(key = %key, max_requests, window_seconds))]
pub async fn check_and_increment(
    db: &TenantPools,
    key: &str,
    max_requests: i64,
    window_seconds: i64,
) -> Result<bool> {
    let key_owned = key.to_string();

    // Upsert and reset the window atomically. Returns the post-increment count.
    let count: i64 = db
        .write(move |conn| {
            Ok(conn.query_row(
                "INSERT INTO rate_limits (key, count, window_start, updated_at)
                 VALUES (?1, 1, datetime('now'), datetime('now'))
                 ON CONFLICT(key) DO UPDATE SET
                     count = CASE
                         WHEN (strftime('%s','now') - strftime('%s', window_start)) > ?2 THEN 1
                         ELSE count + 1
                     END,
                     window_start = CASE
                         WHEN (strftime('%s','now') - strftime('%s', window_start)) > ?2 THEN datetime('now')
                         ELSE window_start
                     END,
                     updated_at = datetime('now')
                 RETURNING count",
                rusqlite::params![key_owned, window_seconds],
                |r| r.get(0),
            )?)
        })
        .await?;

    Ok(count <= max_requests)
}
