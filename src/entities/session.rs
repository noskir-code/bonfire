//! "Stay signed in" tokens. Sessions are kept in memory and lost when the
//! server restarts: these tokens, stored in a cookie and in the database,
//! sign the user back in. See `handlers::session::restore_remembered`.

use sqlx::{PgPool, Row};

use crate::entities::schema::{Column, Index, Table};

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "remember_tokens",
    columns: &[
        // Public part of the token, used to find the row.
        Column::new("selector", "TEXT"),
        // Secret part, stored hashed with argon2 like a password: a copy of the
        // database is not enough to sign in.
        Column::new("validator_hash", "TEXT"),
        // Tokens are deleted with their user.
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("expires_at", "TIMESTAMPTZ"),
    ],
    indexes: &[
        Index::unique("remember_tokens_selector_idx", "selector"),
        Index::new("remember_tokens_user_id_idx", "user_id"),
    ],
};

pub struct StoredToken {
    pub user_id: i32,
    pub validator_hash: String,
}

pub async fn create(
    pool: &PgPool,
    selector: &str,
    validator_hash: &str,
    user_id: i32,
    days: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO remember_tokens (selector, validator_hash, user_id, expires_at)
         VALUES ($1, $2, $3, NOW() + make_interval(days => $4))",
    )
    .bind(selector)
    .bind(validator_hash)
    .bind(user_id)
    .bind(days)
    .execute(pool)
    .await?;
    Ok(())
}

/// The token for `selector`, if it exists and has not expired.
pub async fn find_valid(pool: &PgPool, selector: &str) -> Result<Option<StoredToken>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT user_id, validator_hash FROM remember_tokens
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

/// Pushes the expiry back, so the user stays signed in while they keep coming.
pub async fn extend(pool: &PgPool, selector: &str, days: i32) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE remember_tokens SET expires_at = NOW() + make_interval(days => $2) WHERE selector = $1")
        .bind(selector)
        .bind(days)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, selector: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM remember_tokens WHERE selector = $1")
        .bind(selector)
        .execute(pool)
        .await?;
    Ok(())
}

/// Removes expired tokens, returns how many.
pub async fn delete_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM remember_tokens WHERE expires_at <= NOW()")
        .execute(pool)
        .await?
        .rows_affected())
}
