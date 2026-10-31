//! Follows between users of this server, and the lists of followers and
//! followed accounts shown on profiles: local users and accounts of other
//! servers together (those follows are stored in `entities::federation`).
//!
//! Following a user brings their posts and shares to the feed.

use sqlx::{
    PgPool, Row,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    author::{Author, RemoteAuthor, remote_author_columns},
    schema::{Column, Index, Table},
    user::{User, user_columns},
};

/// Accounts shown at once in a list of a profile, for each server side.
pub const LIST_SIZE: i64 = 500;

pub const TABLE: Table = Table {
    name: "user_follows",
    columns: &[
        Column::new("follower_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("followed_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("user_follows_follower_followed_idx", "follower_id, followed_id"),
        Index::new("user_follows_followed_idx", "followed_id, created_at DESC"),
    ],
};

/// Returns `false` when already followed. Nobody follows themselves.
pub async fn follow(pool: &PgPool, follower_id: i32, followed_id: i32) -> Result<bool, sqlx::Error> {
    if follower_id == followed_id {
        return Ok(false);
    }
    Ok(sqlx::query("INSERT INTO user_follows (follower_id, followed_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(follower_id)
        .bind(followed_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

/// Returns `false` when not followed.
pub async fn unfollow(pool: &PgPool, follower_id: i32, followed_id: i32) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM user_follows WHERE follower_id = $1 AND followed_id = $2")
        .bind(follower_id)
        .bind(followed_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

pub async fn is_following(pool: &PgPool, follower_id: i32, followed_id: i32) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM user_follows WHERE follower_id = $1 AND followed_id = $2)")
        .bind(follower_id)
        .bind(followed_id)
        .fetch_one(pool)
        .await
}

/// Followers and followed accounts of a user, here and on other servers.
/// Follows of other servers waiting for their Accept aren't counted.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub followers: i64,
    pub following: i64,
}

pub async fn counts(pool: &PgPool, user_id: i32) -> Result<Counts, sqlx::Error> {
    let row = sqlx::query(
        "SELECT (SELECT COUNT(*) FROM user_follows WHERE followed_id = $1)
              + (SELECT COUNT(*) FROM federation_followers WHERE user_id = $1) AS followers,
                (SELECT COUNT(*) FROM user_follows WHERE follower_id = $1)
              + (SELECT COUNT(*) FROM federation_following WHERE user_id = $1 AND accepted) AS following",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(Counts {
        followers: row.try_get("followers")?,
        following: row.try_get("following")?,
    })
}

/// An account in a list, with when the follow started and whether it's
/// accepted (always for local follows).
pub struct Listed {
    pub account: Author,
    pub accepted: bool,
    since: DateTime<Utc>,
}

/// Newest follows first, local and remote mixed.
fn newest_first(mut listed: Vec<Listed>) -> Vec<Listed> {
    listed.sort_by(|a, b| b.since.cmp(&a.since));
    listed
}

/// Who follows the user, here and on other servers.
pub async fn followers(pool: &PgPool, user_id: i32) -> Result<Vec<Listed>, sqlx::Error> {
    let mut listed = Vec::new();
    for row in sqlx::query(concat!(
        "SELECT ",
        user_columns!("u."),
        ", f.created_at AS since FROM user_follows f JOIN users u ON u.id = f.follower_id
         WHERE f.followed_id = $1 ORDER BY f.created_at DESC LIMIT $2"
    ))
    .bind(user_id)
    .bind(LIST_SIZE)
    .fetch_all(pool)
    .await?
    {
        listed.push(Listed {
            account: Author::Local(User::from_row(&row)?),
            accepted: true,
            since: row.try_get("since")?,
        });
    }
    for row in sqlx::query(concat!(
        "SELECT ",
        remote_author_columns!(),
        ", f.created_at AS since FROM federation_followers f JOIN federation_actors ra ON ra.id = f.actor_id
         WHERE f.user_id = $1 ORDER BY f.created_at DESC LIMIT $2"
    ))
    .bind(user_id)
    .bind(LIST_SIZE)
    .fetch_all(pool)
    .await?
    {
        listed.push(Listed {
            account: Author::Remote(RemoteAuthor::from_row(&row)?),
            accepted: true,
            since: row.try_get("since")?,
        });
    }
    Ok(newest_first(listed))
}

/// Who the user follows, here and on other servers. Follows waiting for the
/// other server's Accept only when `with_pending` (the user's own profile).
pub async fn following(pool: &PgPool, user_id: i32, with_pending: bool) -> Result<Vec<Listed>, sqlx::Error> {
    let mut listed = Vec::new();
    for row in sqlx::query(concat!(
        "SELECT ",
        user_columns!("u."),
        ", f.created_at AS since FROM user_follows f JOIN users u ON u.id = f.followed_id
         WHERE f.follower_id = $1 ORDER BY f.created_at DESC LIMIT $2"
    ))
    .bind(user_id)
    .bind(LIST_SIZE)
    .fetch_all(pool)
    .await?
    {
        listed.push(Listed {
            account: Author::Local(User::from_row(&row)?),
            accepted: true,
            since: row.try_get("since")?,
        });
    }
    for row in sqlx::query(concat!(
        "SELECT ",
        remote_author_columns!(),
        ", f.created_at AS since, f.accepted FROM federation_following f JOIN federation_actors ra ON ra.id = f.actor_id
         WHERE f.user_id = $1 AND (f.accepted OR $3) ORDER BY f.created_at DESC LIMIT $2"
    ))
    .bind(user_id)
    .bind(LIST_SIZE)
    .bind(with_pending)
    .fetch_all(pool)
    .await?
    {
        listed.push(Listed {
            account: Author::Remote(RemoteAuthor::from_row(&row)?),
            accepted: row.try_get("accepted")?,
            since: row.try_get("since")?,
        });
    }
    Ok(newest_first(listed))
}
