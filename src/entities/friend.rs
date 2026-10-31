//! Friends: a mutual relationship between two users of this server, asked by
//! one and accepted by the other (unlike following, which is one-way). The
//! Fediverse has no such relationship, so only local users can be friends.
//!
//! Friends see each other's posts in their feed.

use sqlx::{PgPool, Row};

use crate::entities::{
    schema::{Column, Index, Table},
    user::{User, user_columns},
};

/// Friends shown at once on a profile.
pub const LIST_SIZE: i64 = 500;

pub const TABLE: Table = Table {
    name: "friendships",
    columns: &[
        // Who asked.
        Column::new("requester_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("addressee_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("accepted", "BOOLEAN").default("FALSE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("accepted_at", "TIMESTAMPTZ").nullable(),
    ],
    indexes: &[
        // One row per pair, whoever asked.
        Index::unique(
            "friendships_pair_idx",
            "LEAST(requester_id, addressee_id), GREATEST(requester_id, addressee_id)",
        ),
        Index::new("friendships_addressee_idx", "addressee_id, accepted"),
        Index::new("friendships_requester_idx", "requester_id, accepted"),
    ],
};

/// Where two users are, seen by the first one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    None,
    Friends,
    /// The viewer asked, waiting for the other.
    RequestSent,
    /// The other asked the viewer.
    RequestReceived,
}

pub async fn state(pool: &PgPool, viewer_id: i32, other_id: i32) -> Result<State, sqlx::Error> {
    let row = sqlx::query(
        "SELECT requester_id, accepted FROM friendships
         WHERE (requester_id = $1 AND addressee_id = $2) OR (requester_id = $2 AND addressee_id = $1)",
    )
    .bind(viewer_id)
    .bind(other_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(State::None);
    };
    Ok(match (row.try_get::<bool, _>("accepted")?, row.try_get::<i32, _>("requester_id")? == viewer_id) {
        (true, _) => State::Friends,
        (false, true) => State::RequestSent,
        (false, false) => State::RequestReceived,
    })
}

/// What a click on "Add friend" did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    /// A new request, the other must accept it.
    Requested,
    /// The other had asked already: now friends.
    Accepted,
    /// Already asked, or already friends.
    Nothing,
}

/// Asks `to` to be friends, or accepts their request if they asked first.
pub async fn request(pool: &PgPool, from: i32, to: i32) -> Result<Added, sqlx::Error> {
    if from == to {
        return Ok(Added::Nothing);
    }
    if accept(pool, from, to).await? {
        return Ok(Added::Accepted);
    }
    let inserted = sqlx::query(
        "INSERT INTO friendships (requester_id, addressee_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(from)
    .bind(to)
    .execute(pool)
    .await?;
    Ok(if inserted.rows_affected() > 0 { Added::Requested } else { Added::Nothing })
}

/// `me` accepts the request of `requester`. Returns `false` when there was none.
pub async fn accept(pool: &PgPool, me: i32, requester: i32) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE friendships SET accepted = TRUE, accepted_at = NOW()
         WHERE requester_id = $2 AND addressee_id = $1 AND NOT accepted",
    )
    .bind(me)
    .bind(requester)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// Cancels a request, declines one, or ends a friendship: whatever links the
/// two users goes. Returns what it was, seen by `me`.
pub async fn remove(pool: &PgPool, me: i32, other: i32) -> Result<State, sqlx::Error> {
    let found = state(pool, me, other).await?;
    sqlx::query(
        "DELETE FROM friendships
         WHERE (requester_id = $1 AND addressee_id = $2) OR (requester_id = $2 AND addressee_id = $1)",
    )
    .bind(me)
    .bind(other)
    .execute(pool)
    .await?;
    Ok(found)
}

async fn users(pool: &PgPool, query: &'static str, user_id: i32) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query(query)
        .bind(user_id)
        .bind(LIST_SIZE)
        .fetch_all(pool)
        .await?
        .iter()
        .map(User::from_row)
        .collect()
}

/// Friends of the user, most recent first.
pub async fn friends(pool: &PgPool, user_id: i32) -> Result<Vec<User>, sqlx::Error> {
    users(
        pool,
        concat!(
            "SELECT ",
            user_columns!("u."),
            " FROM friendships f
              JOIN users u ON u.id = CASE WHEN f.requester_id = $1 THEN f.addressee_id ELSE f.requester_id END
             WHERE (f.requester_id = $1 OR f.addressee_id = $1) AND f.accepted
             ORDER BY f.accepted_at DESC LIMIT $2"
        ),
        user_id,
    )
    .await
}

/// Requests the user received and didn't answer yet, oldest first.
pub async fn received(pool: &PgPool, user_id: i32) -> Result<Vec<User>, sqlx::Error> {
    users(
        pool,
        concat!(
            "SELECT ",
            user_columns!("u."),
            " FROM friendships f JOIN users u ON u.id = f.requester_id
             WHERE f.addressee_id = $1 AND NOT f.accepted ORDER BY f.created_at LIMIT $2"
        ),
        user_id,
    )
    .await
}

/// Requests the user sent, still waiting.
pub async fn sent(pool: &PgPool, user_id: i32) -> Result<Vec<User>, sqlx::Error> {
    users(
        pool,
        concat!(
            "SELECT ",
            user_columns!("u."),
            " FROM friendships f JOIN users u ON u.id = f.addressee_id
             WHERE f.requester_id = $1 AND NOT f.accepted ORDER BY f.created_at DESC LIMIT $2"
        ),
        user_id,
    )
    .await
}

pub async fn count(pool: &PgPool, user_id: i32) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM friendships WHERE (requester_id = $1 OR addressee_id = $1) AND accepted")
        .bind(user_id)
        .fetch_one(pool)
        .await
}
