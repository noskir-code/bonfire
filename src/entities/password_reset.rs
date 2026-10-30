//! "Forgot password" links. Same design as the "stay signed in" tokens:
//! `selector.validator`, with only an argon2 hash of the validator stored.

use sqlx::{PgPool, Row};

use crate::entities::schema::{Column, Index, Table};

/// How long a link sent by email works.
pub const VALID_MINUTES: i32 = 15;
/// Minimum time between two emails for the same account, against spam.
pub const RESEND_DELAY_SECONDS: i32 = 120;

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "password_reset_tokens",
    columns: &[
        Column::new("selector", "TEXT"),
        Column::new("validator_hash", "TEXT"),
        // Tokens are deleted with their user.
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("expires_at", "TIMESTAMPTZ"),
    ],
    indexes: &[
        Index::unique("password_reset_tokens_selector_idx", "selector"),
        Index::new("password_reset_tokens_user_id_idx", "user_id, created_at DESC"),
    ],
};

pub struct StoredToken {
    pub user_id: i32,
    pub validator_hash: String,
}

pub async fn create(pool: &PgPool, selector: &str, validator_hash: &str, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO password_reset_tokens (selector, validator_hash, user_id, expires_at)
         VALUES ($1, $2, $3, NOW() + make_interval(mins => $4))",
    )
    .bind(selector)
    .bind(validator_hash)
    .bind(user_id)
    .bind(VALID_MINUTES)
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether a link was sent to the user less than `RESEND_DELAY_SECONDS` ago.
pub async fn recently_sent(pool: &PgPool, user_id: i32) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM password_reset_tokens
         WHERE user_id = $1 AND created_at > NOW() - make_interval(secs => $2))",
    )
    .bind(user_id)
    .bind(f64::from(RESEND_DELAY_SECONDS))
    .fetch_one(pool)
    .await
}

/// The token for `selector`, if it exists and has not expired.
pub async fn find_valid(pool: &PgPool, selector: &str) -> Result<Option<StoredToken>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT user_id, validator_hash FROM password_reset_tokens
         WHERE selector = $1 AND expires_at > NOW()",
    )
    .bind(selector)
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok(StoredToken {
            user_id: row.try_get("user_id")?,
            validator_hash: row.try_get("validator_hash")?,
        })
    })
    .transpose()
}

/// Once the password is reset, every link sent to the user stops working.
pub async fn delete_for_user(pool: &PgPool, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM password_reset_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM password_reset_tokens WHERE expires_at <= NOW()")
        .execute(pool)
        .await?
        .rows_affected())
}
