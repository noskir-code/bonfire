//! Private messages: conversations between two people, two users of this
//! server or a user and an account of another Fediverse server (Mastodon,
//! Pleroma, Hubzilla, Misskey... see `API::activities` for how they travel).
//!
//! A conversation between two local users is stored once, the lower user id
//! in `user_id`. With another server, `user_id` is the local user and
//! `remote_actor_id` the other.

use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    author::{Author, RemoteAuthor, remote_author_columns},
    post::{self, BodyError},
    schema::{Column, Index, Table},
    user::user_columns,
};

pub const BODY_MAX_CHARS: usize = 5_000;
/// Messages shown at once in a conversation, the latest ones.
pub const PAGE_SIZE: i64 = 200;
const EXCERPT_CHARS: i32 = 80;

pub const CONVERSATIONS_TABLE: Table = Table {
    name: "conversations",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        // The other person: a local user, or an account of another server.
        Column::new("other_user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE CASCADE"),
        Column::new("remote_actor_id", "BIGINT")
            .nullable()
            .references("federation_actors(id) ON DELETE CASCADE"),
        // When each side last read it.
        Column::new("user_read_at", "TIMESTAMPTZ").nullable(),
        Column::new("other_read_at", "TIMESTAMPTZ").nullable(),
        // How the other server writes: `note` (Mastodon, Hubzilla...) or
        // `chat` (Pleroma's chats). Answers are sent the same way.
        Column::new("remote_kind", "TEXT").default("'note'"),
        Column::new("last_message_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique(
            "conversations_pair_idx",
            "user_id, COALESCE(other_user_id, 0), COALESCE(remote_actor_id, 0)",
        ),
        Index::new("conversations_other_user_idx", "other_user_id"),
    ],
};

pub const MESSAGES_TABLE: Table = Table {
    name: "messages",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        Column::new("conversation_id", "BIGINT").references("conversations(id) ON DELETE CASCADE"),
        // Who wrote it: a local user, or the account of the other server.
        Column::new("sender_user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE CASCADE"),
        Column::new("sender_actor_id", "BIGINT")
            .nullable()
            .references("federation_actors(id) ON DELETE CASCADE"),
        Column::new("body", "TEXT"),
        // Its ActivityPub id, for messages of other servers.
        Column::new("ap_id", "TEXT").nullable(),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("edited_at", "TIMESTAMPTZ").nullable(),
    ],
    indexes: &[
        Index::new("messages_conversation_idx", "conversation_id, id"),
        // A direct note addressed to two users here is a message in each
        // conversation.
        Index::unique("messages_conversation_ap_id_idx", "conversation_id, ap_id"),
        Index::new("messages_ap_id_idx", "ap_id"),
    ],
};

pub fn validate_body(raw: &str) -> Result<String, BodyError> {
    post::validate_text(raw, BODY_MAX_CHARS)
}

/// The other person of a conversation, seen by `$1`: the columns of
/// `Author::from_row` (`u.` and `ra.`). A macro for `concat!`.
macro_rules! other_columns {
    () => {
        concat!(
            user_columns!("u."),
            ", ",
            remote_author_columns!(),
            " FROM conversations c
              LEFT JOIN users u ON u.id = CASE WHEN c.user_id = $1 THEN c.other_user_id ELSE c.user_id END
                                 AND c.remote_actor_id IS NULL
              LEFT JOIN federation_actors ra ON ra.id = c.remote_actor_id"
        )
    };
}

/// When `$1` last read the conversation `c`. A macro for `concat!`.
macro_rules! read_at {
    () => {
        "CASE WHEN c.user_id = $1 THEN c.user_read_at ELSE c.other_read_at END"
    };
}

/// A conversation in the list of the messenger.
pub struct Summary {
    pub id: i64,
    pub other: Author,
    pub last_body: String,
    pub last_is_mine: bool,
    pub last_at: DateTime<Utc>,
    pub unread: bool,
}

impl Summary {
    pub fn last_at_iso(&self) -> String {
        post::time_iso(&self.last_at)
    }

    pub fn last_at_label(&self) -> String {
        post::time_label(&self.last_at)
    }
}

/// The conversations of the user with at least a message, latest first.
pub async fn list(pool: &PgPool, user_id: i32) -> Result<Vec<Summary>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT c.id AS conversation_id, c.last_message_at, last.body AS last_body,
                COALESCE(last.sender_user_id = $1, FALSE) AS last_is_mine,
                EXISTS (SELECT 1 FROM messages m WHERE m.conversation_id = c.id
                          AND m.sender_user_id IS DISTINCT FROM $1
                          AND m.created_at > COALESCE(",
        read_at!(),
        ", '-infinity')) AS unread, ",
        other_columns!(),
        " JOIN LATERAL (SELECT LEFT(m.body, $2) AS body, m.sender_user_id FROM messages m
                        WHERE m.conversation_id = c.id ORDER BY m.id DESC LIMIT 1) last ON TRUE
         WHERE c.user_id = $1 OR c.other_user_id = $1
         ORDER BY c.last_message_at DESC"
    ))
    .bind(user_id)
    .bind(EXCERPT_CHARS)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|row| {
        Ok(Summary {
            id: row.try_get("conversation_id")?,
            other: Author::from_row(row)?,
            last_body: row.try_get("last_body")?,
            last_is_mine: row.try_get("last_is_mine")?,
            last_at: row.try_get("last_message_at")?,
            unread: row.try_get("unread")?,
        })
    })
    .collect()
}

/// Conversations with an unread message, shown next to "Messenger".
pub async fn unread_count(pool: &PgPool, user_id: i32) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(concat!(
        "SELECT COUNT(*) FROM conversations c
         WHERE (c.user_id = $1 OR c.other_user_id = $1)
           AND EXISTS (SELECT 1 FROM messages m WHERE m.conversation_id = c.id
                         AND m.sender_user_id IS DISTINCT FROM $1
                         AND m.created_at > COALESCE(",
        read_at!(),
        ", '-infinity'))"
    ))
    .bind(user_id)
    .fetch_one(pool)
    .await
}

/// A conversation opened by one of its people.
pub struct Conversation {
    pub id: i64,
    pub other: Author,
    /// `chat` when the other server writes Pleroma chat messages.
    pub remote_kind: String,
}

/// The conversation, if `user_id` is in it.
pub async fn find(pool: &PgPool, id: i64, user_id: i32) -> Result<Option<Conversation>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT c.id AS conversation_id, c.remote_kind, ",
        other_columns!(),
        " WHERE c.id = $2 AND (c.user_id = $1 OR c.other_user_id = $1)"
    ))
    .bind(user_id)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(|row| {
        Ok(Conversation {
            id: row.try_get("conversation_id")?,
            other: Author::from_row(row)?,
            remote_kind: row.try_get("remote_kind")?,
        })
    })
    .transpose()
}

/// The conversation of two local users, made the first time.
pub async fn with_user(pool: &PgPool, user_id: i32, other_id: i32) -> Result<i64, sqlx::Error> {
    let (low, high) = if user_id < other_id { (user_id, other_id) } else { (other_id, user_id) };
    sqlx::query_scalar(
        "INSERT INTO conversations (user_id, other_user_id) VALUES ($1, $2)
         ON CONFLICT (user_id, COALESCE(other_user_id, 0), COALESCE(remote_actor_id, 0))
         DO UPDATE SET user_id = EXCLUDED.user_id
         RETURNING id",
    )
    .bind(low)
    .bind(high)
    .fetch_one(pool)
    .await
}

/// The conversation of a local user with an account of another server
/// (a row of `federation_actors`), made the first time.
pub async fn with_remote(pool: &PgPool, user_id: i32, remote_actor_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO conversations (user_id, remote_actor_id) VALUES ($1, $2)
         ON CONFLICT (user_id, COALESCE(other_user_id, 0), COALESCE(remote_actor_id, 0))
         DO UPDATE SET user_id = EXCLUDED.user_id
         RETURNING id",
    )
    .bind(user_id)
    .bind(remote_actor_id)
    .fetch_one(pool)
    .await
}

/// Remembers how the other server writes, see `remote_kind`.
pub async fn set_remote_kind(pool: &PgPool, id: i64, kind: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE conversations SET remote_kind = $2 WHERE id = $1")
        .bind(id)
        .bind(kind)
        .execute(pool)
        .await?;
    Ok(())
}

/// The local users of a conversation: the ones whose pages are told.
pub async fn local_people(pool: &PgPool, id: i64) -> Result<Vec<i32>, sqlx::Error> {
    let row = sqlx::query("SELECT user_id, other_user_id FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Ok(Vec::new());
    };
    let mut people = vec![row.try_get::<i32, _>("user_id")?];
    people.extend(row.try_get::<Option<i32>, _>("other_user_id")?);
    Ok(people)
}

pub async fn mark_read(pool: &PgPool, id: i64, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE conversations SET
            user_read_at = CASE WHEN user_id = $2 THEN NOW() ELSE user_read_at END,
            other_read_at = CASE WHEN other_user_id = $2 THEN NOW() ELSE other_read_at END
         WHERE id = $1",
    )
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct Message {
    pub id: i64,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub author: Author,
    /// Written by the viewer: on the right, with "Delete".
    pub is_mine: bool,
}

impl Message {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("message_id")?,
            body: row.try_get("body")?,
            created_at: row.try_get("sent_at")?,
            edited_at: row.try_get("edited_at")?,
            author: Author::from_row(row)?,
            is_mine: row.try_get("is_mine")?,
        })
    }

    pub fn body_html(&self) -> String {
        crate::linkify::to_html(&self.body)
    }

    pub fn created_at_iso(&self) -> String {
        post::time_iso(&self.created_at)
    }

    pub fn created_at_label(&self) -> String {
        post::time_label(&self.created_at)
    }
}

/// The latest messages of a conversation, oldest first, or only those after
/// `after` (new ones, for the live page). `$1` is the viewer.
pub async fn messages(pool: &PgPool, id: i64, viewer_id: i32, after: Option<i64>) -> Result<Vec<Message>, sqlx::Error> {
    let mut messages = sqlx::query(concat!(
        "SELECT m.id AS message_id, m.body, m.created_at AS sent_at, m.edited_at,
                COALESCE(m.sender_user_id = $1, FALSE) AS is_mine, ",
        user_columns!("u."),
        ", ",
        remote_author_columns!(),
        " FROM messages m
          LEFT JOIN users u ON u.id = m.sender_user_id
          LEFT JOIN federation_actors ra ON ra.id = m.sender_actor_id
         WHERE m.conversation_id = $2 AND ($3::bigint IS NULL OR m.id > $3)
         ORDER BY m.id DESC LIMIT $4"
    ))
    .bind(viewer_id)
    .bind(id)
    .bind(after)
    .bind(PAGE_SIZE)
    .fetch_all(pool)
    .await?
    .iter()
    .map(Message::from_row)
    .collect::<Result<Vec<_>, _>>()?;
    messages.reverse();
    Ok(messages)
}

/// A message written here. The sender has read the conversation.
pub async fn add(pool: &PgPool, id: i64, sender_id: i32, body: &str) -> Result<(i64, DateTime<Utc>), sqlx::Error> {
    let row = sqlx::query(
        "WITH added AS (
            INSERT INTO messages (conversation_id, sender_user_id, body) VALUES ($1, $2, $3)
            RETURNING id, created_at
         ), touched AS (
            UPDATE conversations SET last_message_at = NOW(),
                user_read_at = CASE WHEN user_id = $2 THEN NOW() ELSE user_read_at END,
                other_read_at = CASE WHEN other_user_id = $2 THEN NOW() ELSE other_read_at END
            WHERE id = $1
         )
         SELECT id, created_at FROM added",
    )
    .bind(id)
    .bind(sender_id)
    .bind(body)
    .fetch_one(pool)
    .await?;
    Ok((row.try_get("id")?, row.try_get("created_at")?))
}

/// A message received from another server. `None` when already stored.
pub async fn add_remote(
    pool: &PgPool,
    id: i64,
    remote_actor_id: i64,
    ap_id: &str,
    body: &str,
    published: Option<DateTime<Utc>>,
) -> Result<Option<i64>, sqlx::Error> {
    let added: Option<i64> = sqlx::query_scalar(
        "INSERT INTO messages (conversation_id, sender_actor_id, ap_id, body, created_at)
         VALUES ($1, $2, $3, $4, COALESCE($5, NOW()))
         ON CONFLICT (conversation_id, ap_id) DO NOTHING
         RETURNING id",
    )
    .bind(id)
    .bind(remote_actor_id)
    .bind(ap_id)
    .bind(body)
    .bind(published)
    .fetch_optional(pool)
    .await?;
    if added.is_some() {
        sqlx::query("UPDATE conversations SET last_message_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(added)
}

/// Deletes the user's own message. Returns its conversation when it was theirs.
pub async fn delete_own(pool: &PgPool, message_id: i64, user_id: i32) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("DELETE FROM messages WHERE id = $1 AND sender_user_id = $2 RETURNING conversation_id")
        .bind(message_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// The messages received from `remote_actor_id` with this ActivityPub id
/// (one per conversation): (message id, conversation id).
pub async fn find_remote(pool: &PgPool, remote_actor_id: i64, ap_id: &str) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    sqlx::query("SELECT id, conversation_id FROM messages WHERE ap_id = $1 AND sender_actor_id = $2")
        .bind(ap_id)
        .bind(remote_actor_id)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|row| Ok((row.try_get("id")?, row.try_get("conversation_id")?)))
        .collect()
}

pub async fn update_remote(pool: &PgPool, message_id: i64, body: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE messages SET body = $2, edited_at = NOW() WHERE id = $1")
        .bind(message_id)
        .bind(body)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, message_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(message_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The account of another server in a conversation, for sending to it.
pub fn remote_of(conversation: &Conversation) -> Option<&RemoteAuthor> {
    match &conversation.other {
        Author::Remote(remote) => Some(remote),
        Author::Local(_) => None,
    }
}
