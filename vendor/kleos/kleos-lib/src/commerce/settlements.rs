use rusqlite::params;
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::db::Database;
use crate::{EngError, Result};

use super::types::AccountBalance;

// ---------------------------------------------------------------------------
// Account balance
// ---------------------------------------------------------------------------

/// Get or create an account balance for a user.
pub async fn get_balance(db: &Database, user_id: i64) -> Result<AccountBalance> {
    db.read(move |conn| {
        match conn.query_row(
            "SELECT user_id, balance, currency, updated_at
             FROM account_balances WHERE user_id = ?1",
            params![user_id],
            |row| {
                Ok(AccountBalance {
                    user_id: row.get(0)?,
                    balance: Decimal::from_str(&row.get::<_, String>(1)?).unwrap_or(Decimal::ZERO),
                    currency: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        ) {
            Ok(b) => Ok(b),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(AccountBalance {
                user_id,
                balance: Decimal::ZERO,
                currency: "USDC".to_string(),
                updated_at: String::new(),
            }),
            Err(e) => Err(EngError::Database(e)),
        }
    })
    .await
}

/// Get today's spend for a user.
pub async fn get_daily_spend(db: &Database, user_id: i64) -> Result<Decimal> {
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    db.read(move |conn| {
        match conn.query_row(
            "SELECT total_amount FROM daily_spend WHERE user_id = ?1 AND date = ?2",
            params![user_id, date],
            |row| row.get::<_, String>(0),
        ) {
            Ok(s) => Ok(Decimal::from_str(&s).unwrap_or(Decimal::ZERO)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Decimal::ZERO),
            Err(e) => Err(EngError::Database(e)),
        }
    })
    .await
}
