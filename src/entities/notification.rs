//! Notifications shown to a user: likes, comments and shares of their posts,
//! replies to their comments, and new followers from the Fediverse. Never
//! about the user's own actions.
//!
//! Every change also sends a Postgres notification on `CHANNEL` saying what
//! changed (see `Change`), so the user's open pages update at once (see
//! `handlers::notification`), even when served by another Bonfire server.

use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    post::{self, Reaction},
    schema::{Column, Index, Table},
    user::{User, user_columns},
};

/// Postgres channel (LISTEN/NOTIFY) of the changes.
pub const CHANNEL: &str = "bonfire_notifications";
/// Notifications shown at once, newest first.
pub const PAGE_SIZE: i64 = 50;
/// Read notifications older than this are deleted at startup.
pub const KEEP_READ_DAYS: i32 = 90;
const EXCERPT_CHARS: i32 = 120;

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "notifications",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // Who receives it.
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("kind", "TEXT"),
        // Who did it: a local user, or an account of another server.
        Column::new("actor_user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE CASCADE"),
        Column::new("remote_actor_id", "BIGINT")
            .nullable()
            .references("federation_actors(id) ON DELETE CASCADE"),
        // What it's about, deleted with it.
        Column::new("post_id", "BIGINT").nullable().references("posts(id) ON DELETE CASCADE"),
        Column::new("comment_id", "BIGINT")
            .nullable()
            .references("comments(id) ON DELETE CASCADE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("read_at", "TIMESTAMPTZ").nullable(),
    ],
    indexes: &[
        Index::new("notifications_user_created_at_idx", "user_id, created_at DESC"),
        // Unread count shown on every page.
        Index::new("notifications_user_read_at_idx", "user_id, read_at"),
    ],
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Like,
    Comment,
    /// A comment answering one of the user's comments.
    Reply,
    Share,
    /// An account of another server followed the user.
    Follow,
}

impl Kind {
    const ALL: [Self; 5] = [Self::Like, Self::Comment, Self::Reply, Self::Share, Self::Follow];

    /// Stored in the database.
    fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Comment => "comment",
            Self::Reply => "reply",
            Self::Share => "share",
            Self::Follow => "follow",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// What happened, after the actor's name.
    pub fn action(self) -> &'static str {
        match self {
            Self::Like => "liked your post",
            Self::Comment => "commented on your post",
            Self::Reply => "replied to your comment",
            Self::Share => "shared your post",
            Self::Follow => "followed you from the Fediverse",
        }
    }

    fn from_reaction(reaction: Reaction) -> Self {
        match reaction {
            Reaction::Like => Self::Like,
            Reaction::Share => Self::Share,
        }
    }
}

/// An account of another server, as shown in a notification.
pub struct RemoteAccount {
    /// Their address, opening their profile on their server.
    pub url: String,
    /// Like `@alice@mastodon.social`.
    pub handle: String,
}

pub struct Notification {
    pub id: i64,
    pub kind: Kind,
    pub created_at: DateTime<Utc>,
    pub unread: bool,
    pub actor: Option<User>,
    pub remote_actor: Option<RemoteAccount>,
    pub post_id: Option<i64>,
    /// Start of the post, `post_truncated` when there is more.
    pub post_excerpt: Option<String>,
    pub post_truncated: bool,
    pub comment_id: Option<i64>,
    pub comment_excerpt: Option<String>,
}

impl Notification {
    fn from_row(row: &PgRow) -> Result<Option<Self>, sqlx::Error> {
        let kind: String = row.try_get("kind")?;
        // Written by a newer version: skipped rather than failing the page.
        let Some(kind) = Kind::parse(&kind) else {
            return Ok(None);
        };
        // The user columns are NULL for notifications from other servers.
        let actor = match row.try_get::<Option<i32>, _>("id")? {
            Some(_) => Some(User::from_row(row)?),
            None => None,
        };
        let remote_actor = row
            .try_get::<Option<String>, _>("remote_url")?
            .map(|url| {
                let username: String = row.try_get("remote_username")?;
                Ok::<_, sqlx::Error>(RemoteAccount { handle: handle(&username, &url), url })
            })
            .transpose()?;
        Ok(Some(Self {
            id: row.try_get("notification_id")?,
            kind,
            created_at: row.try_get("notified_at")?,
            unread: row.try_get::<Option<DateTime<Utc>>, _>("read_at")?.is_none(),
            actor,
            remote_actor,
            post_id: row.try_get("post_id")?,
            post_excerpt: row.try_get("post_excerpt")?,
            post_truncated: row.try_get::<Option<bool>, _>("post_truncated")?.unwrap_or(false),
            comment_id: row.try_get("comment_id")?,
            comment_excerpt: row.try_get("comment_excerpt")?,
        }))
    }

    /// Where the notification leads: the comment, the post, or the account.
    pub fn link(&self) -> String {
        match (self.post_id, self.comment_id) {
            (Some(post_id), Some(comment_id)) => format!("/posts/{post_id}#comment-{comment_id}"),
            (Some(post_id), None) => format!("/posts/{post_id}"),
            _ => match (&self.actor, &self.remote_actor) {
                (Some(actor), _) => format!("/user/{}", actor.username),
                (None, Some(remote)) => remote.url.clone(),
                (None, None) => "/notifications".to_string(),
            },
        }
    }

    pub fn actor_name(&self) -> String {
        match (&self.actor, &self.remote_actor) {
            (Some(actor), _) => actor.display_name(),
            (None, Some(remote)) => remote.handle.clone(),
            (None, None) => "Someone".to_string(),
        }
    }

    pub fn created_at_iso(&self) -> String {
        post::time_iso(&self.created_at)
    }

    pub fn created_at_label(&self) -> String {
        post::time_label(&self.created_at)
    }
}

/// `@username@host` from an actor address like `https://host/users/username`.
fn handle(username: &str, actor_url: &str) -> String {
    let host = actor_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(actor_url);
    if username.is_empty() {
        host.to_string()
    } else {
        format!("@{username}@{host}")
    }
}

/// A change of a user's notifications, sent on `CHANNEL` as
/// `user_id:event:notification_id` (or `:all`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Added(i64),
    Removed(i64),
    Read(i64),
    AllRemoved,
    AllRead,
}

impl Change {
    /// The user and the change, from a signal.
    pub fn parse(payload: &str) -> Option<(i32, Self)> {
        let mut parts = payload.splitn(3, ':');
        let user_id = parts.next()?.parse().ok()?;
        let change = match (parts.next()?, parts.next()?) {
            ("removed", "all") => Self::AllRemoved,
            ("read", "all") => Self::AllRead,
            ("added", id) => Self::Added(id.parse().ok()?),
            ("removed", id) => Self::Removed(id.parse().ok()?),
            ("read", id) => Self::Read(id.parse().ok()?),
            _ => return None,
        };
        Some((user_id, change))
    }
}

/// Wraps a statement returning `user_id, id` so each changed notification is
/// signalled on `CHANNEL` with `$event` (`added`, `removed` or `read`).
macro_rules! signalled {
    ($event:literal, $statement:literal) => {
        concat!(
            "WITH changed AS (",
            $statement,
            // Same name as `CHANNEL`, see the tests below.
            " RETURNING user_id, id) SELECT pg_notify('bonfire_notifications', user_id::text || ':",
            $event,
            ":' || id::text) FROM changed"
        )
    };
}

/// Like `signalled!`, for statements changing every notification of the user
/// `$1`: one signal `user_id:$event:all` instead of one per notification.
macro_rules! signalled_all {
    ($event:literal, $statement:literal) => {
        concat!(
            "WITH changed AS (",
            $statement,
            " RETURNING 1) SELECT pg_notify('bonfire_notifications', $1::text || ':",
            $event,
            ":all') WHERE EXISTS (SELECT 1 FROM changed)"
        )
    };
}

/// Notifies the post's author of a like or share by `actor_id`. Nothing for
/// reactions to one's own posts.
pub async fn reaction_added(
    pool: &PgPool,
    reaction: Reaction,
    post_id: i64,
    actor_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, actor_user_id, post_id)
         SELECT user_id, $1, $2, id FROM posts WHERE id = $3 AND user_id <> $2"
    ))
    .bind(Kind::from_reaction(reaction).as_str())
    .bind(actor_id)
    .bind(post_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A removed like or share removes its notification, read or not.
pub async fn reaction_removed(
    pool: &PgPool,
    reaction: Reaction,
    post_id: i64,
    actor_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "removed",
        "DELETE FROM notifications WHERE kind = $1 AND actor_user_id = $2 AND post_id = $3"
    ))
    .bind(Kind::from_reaction(reaction).as_str())
    .bind(actor_id)
    .bind(post_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A new comment notifies the author of the comment it replies to
/// (`parent_author`), and the post's author. Once each: a post author whose
/// comment gets a reply only gets the reply. Never the commenter.
pub async fn comment_added(
    pool: &PgPool,
    post_id: i64,
    comment_id: i64,
    actor_id: i32,
    parent_author: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, actor_user_id, post_id, comment_id)
         SELECT $5::integer, $6, $2, $3, $4 WHERE $5::integer <> $2
         UNION ALL
         SELECT user_id, $1, $2, id, $4 FROM posts
         WHERE id = $3 AND user_id <> $2 AND user_id IS DISTINCT FROM $5::integer"
    ))
    .bind(Kind::Comment.as_str())
    .bind(actor_id)
    .bind(post_id)
    .bind(comment_id)
    .bind(parent_author)
    .bind(Kind::Reply.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// Like `comment_added`, for a reply from another server: `remote_actor_id`
/// is a row of `federation_actors`.
pub async fn remote_comment_added(
    pool: &PgPool,
    post_id: i64,
    comment_id: i64,
    remote_actor_id: i64,
    parent_author: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, remote_actor_id, post_id, comment_id)
         SELECT $5::integer, $6, $2, $3, $4 WHERE $5::integer IS NOT NULL
         UNION ALL
         SELECT user_id, $1, $2, id, $4 FROM posts
         WHERE id = $3 AND user_id IS NOT NULL AND user_id IS DISTINCT FROM $5::integer"
    ))
    .bind(Kind::Comment.as_str())
    .bind(remote_actor_id)
    .bind(post_id)
    .bind(comment_id)
    .bind(parent_author)
    .bind(Kind::Reply.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// `remote_actor_id` is a row of `federation_actors`.
pub async fn followed(pool: &PgPool, user_id: i32, remote_actor_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, remote_actor_id) VALUES ($1, $2, $3)"
    ))
    .bind(user_id)
    .bind(Kind::Follow.as_str())
    .bind(remote_actor_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Notifications with what they are about, newest first. `$1` is the user,
/// `$4` an optional id to load only that one, `$5` an optional id to load
/// only older ones.
macro_rules! notification_select {
    () => {
        concat!(
            "SELECT n.id AS notification_id, n.kind, n.created_at AS notified_at, n.read_at,
                    n.post_id, n.comment_id,
                    a.actor_id AS remote_url, a.username AS remote_username,
                    LEFT(p.body, $2) AS post_excerpt, LENGTH(p.body) > $2 AS post_truncated,
                    LEFT(c.body, $2) AS comment_excerpt, ",
            user_columns!("u."),
            " FROM notifications n
              LEFT JOIN users u ON u.id = n.actor_user_id
              LEFT JOIN federation_actors a ON a.id = n.remote_actor_id
              LEFT JOIN posts p ON p.id = n.post_id
              LEFT JOIN comments c ON c.id = n.comment_id
             WHERE n.user_id = $1
               AND ($4::bigint IS NULL OR n.id = $4)
               AND ($5::bigint IS NULL OR n.id < $5)
             ORDER BY n.id DESC
             LIMIT $3"
        )
    };
}

async fn select(
    pool: &PgPool,
    user_id: i32,
    limit: i64,
    only: Option<i64>,
    before: Option<i64>,
) -> Result<Vec<Notification>, sqlx::Error> {
    let rows = sqlx::query(notification_select!())
        .bind(user_id)
        .bind(EXCERPT_CHARS)
        .bind(limit)
        .bind(only)
        .bind(before)
        .fetch_all(pool)
        .await?;
    let mut notifications = Vec::with_capacity(rows.len());
    for row in &rows {
        notifications.extend(Notification::from_row(row)?);
    }
    Ok(notifications)
}

/// A page of notifications, newest first, older than `before` when given.
/// The flag tells whether older ones remain.
pub async fn page(pool: &PgPool, user_id: i32, before: Option<i64>) -> Result<(Vec<Notification>, bool), sqlx::Error> {
    // One more than shown: if it exists, there is a next page.
    let mut notifications = select(pool, user_id, PAGE_SIZE + 1, None, before).await?;
    let has_more = notifications.len() as i64 > PAGE_SIZE;
    notifications.truncate(PAGE_SIZE as usize);
    Ok((notifications, has_more))
}

/// One notification of the user.
pub async fn find(pool: &PgPool, user_id: i32, id: i64) -> Result<Option<Notification>, sqlx::Error> {
    Ok(select(pool, user_id, 1, Some(id), None).await?.pop())
}

pub async fn unread_count(pool: &PgPool, user_id: i32) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND read_at IS NULL")
        .bind(user_id)
        .fetch_one(pool)
        .await
}

/// Marks one notification read. Does nothing for another user's notification.
pub async fn mark_read(pool: &PgPool, user_id: i32, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "read",
        "UPDATE notifications SET read_at = NOW() WHERE user_id = $1 AND id = $2 AND read_at IS NULL"
    ))
    .bind(user_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_all_read(pool: &PgPool, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled_all!(
        "read",
        "UPDATE notifications SET read_at = NOW() WHERE user_id = $1 AND read_at IS NULL"
    ))
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, user_id: i32, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!("removed", "DELETE FROM notifications WHERE user_id = $1 AND id = $2"))
        .bind(user_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_all(pool: &PgPool, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled_all!("removed", "DELETE FROM notifications WHERE user_id = $1"))
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_old_read(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM notifications WHERE read_at < NOW() - make_interval(days => $1)")
            .bind(KEEP_READ_DAYS)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_use_the_channel() {
        let channel = format!("pg_notify('{CHANNEL}'");
        assert!(signalled!("removed", "DELETE FROM notifications").contains(&channel));
        assert!(signalled_all!("read", "UPDATE notifications").contains(&channel));
    }

    #[test]
    fn changes_are_parsed() {
        assert_eq!(Change::parse("12:added:345"), Some((12, Change::Added(345))));
        assert_eq!(Change::parse("12:removed:345"), Some((12, Change::Removed(345))));
        assert_eq!(Change::parse("12:read:all"), Some((12, Change::AllRead)));
        assert_eq!(Change::parse("12:removed:all"), Some((12, Change::AllRemoved)));
        assert_eq!(Change::parse("12:added:all"), None);
        assert_eq!(Change::parse("12"), None);
        assert_eq!(Change::parse("x:read:1"), None);
    }
}
