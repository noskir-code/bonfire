//! Notifications shown to a user: likes, comments and shares of their posts
//! (from here or other servers), replies to their comments, new followers
//! from the Fediverse, and requests to join the private communities they
//! moderate. Never about the user's own actions.
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
    federation::{Reacted, RemoteReaction},
    media,
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
        Column::new("community_id", "BIGINT")
            .nullable()
            .references("communities(id) ON DELETE CASCADE"),
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
    /// A like of one of the user's comments.
    CommentLike,
    Share,
    /// An account of another server followed the user.
    Follow,
    /// A user of this server followed the user.
    UserFollow,
    /// A user asked to be the user's friend.
    FriendRequest,
    /// A user accepted the user's friend request.
    FriendAccepted,
    /// Someone asked to join a private community the user moderates.
    JoinRequest,
    /// A moderator accepted the user in a private community.
    JoinApproved,
}

impl Kind {
    const ALL: [Self; 11] = [
        Self::Like,
        Self::Comment,
        Self::Reply,
        Self::CommentLike,
        Self::Share,
        Self::Follow,
        Self::UserFollow,
        Self::FriendRequest,
        Self::FriendAccepted,
        Self::JoinRequest,
        Self::JoinApproved,
    ];

    /// Stored in the database.
    fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Comment => "comment",
            Self::Reply => "reply",
            Self::CommentLike => "comment_like",
            Self::Share => "share",
            Self::Follow => "follow",
            Self::UserFollow => "user_follow",
            Self::FriendRequest => "friend_request",
            Self::FriendAccepted => "friend_accepted",
            Self::JoinRequest => "join_request",
            Self::JoinApproved => "join_approved",
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
            Self::CommentLike => "liked your comment",
            Self::Share => "shared your post",
            Self::Follow => "followed you from the Fediverse",
            Self::UserFollow => "followed you",
            Self::FriendRequest => "wants to be your friend",
            Self::FriendAccepted => "accepted your friend request",
            Self::JoinRequest => "asked to join",
            Self::JoinApproved => "accepted you in",
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
    /// Their page here, `/remote/{id}`.
    pub url: String,
    /// Like `@alice@mastodon.social`.
    pub handle: String,
    /// Through the media proxy.
    pub photo_url: Option<String>,
}

/// The community of a notification about joining it.
pub struct NotifiedCommunity {
    pub slug: String,
    pub name: String,
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
    pub community: Option<NotifiedCommunity>,
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
            .map(|actor_url| {
                let username: String = row.try_get("remote_username")?;
                let id: i64 = row.try_get("remote_id")?;
                Ok::<_, sqlx::Error>(RemoteAccount {
                    handle: handle(&username, &actor_url),
                    url: format!("/remote/{id}"),
                    photo_url: row.try_get::<Option<String>, _>("remote_icon")?.as_deref().map(media::proxy_url),
                })
            })
            .transpose()?;
        let community = match row.try_get::<Option<String>, _>("community_slug")? {
            Some(slug) => Some(NotifiedCommunity {
                slug,
                name: row.try_get("community_name")?,
            }),
            None => None,
        };
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
            community,
        }))
    }

    /// Where the notification leads: the comment, the post, the community,
    /// or the account.
    pub fn link(&self) -> String {
        if let Some(community) = &self.community {
            return match self.kind {
                Kind::JoinRequest => format!("/community/{}/settings#requests", community.slug),
                _ => format!("/community/{}", community.slug),
            };
        }
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

    /// The avatar of who did it, here or through the media proxy.
    pub fn actor_photo(&self) -> Option<String> {
        match (&self.actor, &self.remote_actor) {
            (Some(actor), _) => actor.photo_url.clone(),
            (None, Some(remote)) => remote.photo_url.clone(),
            (None, None) => None,
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
    /// A message was sent, read or deleted in this conversation: the open
    /// pages refresh it and the unread count of the messenger.
    Message(i64),
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
            ("message", id) => Self::Message(id.parse().ok()?),
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

/// Notifies the comment's author of a like by `actor_id`, unless they liked
/// their own comment. Comments of other servers have nobody here to notify.
pub async fn comment_like_added(pool: &PgPool, comment_id: i64, actor_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, actor_user_id, post_id, comment_id)
         SELECT user_id, $1, $2, post_id, id FROM comments
         WHERE id = $3 AND user_id IS NOT NULL AND user_id <> $2"
    ))
    .bind(Kind::CommentLike.as_str())
    .bind(actor_id)
    .bind(comment_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A removed like of a comment removes its notification, read or not.
pub async fn comment_like_removed(pool: &PgPool, comment_id: i64, actor_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "removed",
        "DELETE FROM notifications WHERE kind = $1 AND actor_user_id = $2 AND comment_id = $3"
    ))
    .bind(Kind::CommentLike.as_str())
    .bind(actor_id)
    .bind(comment_id)
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

fn remote_kind(kind: RemoteReaction, target: Reacted) -> Kind {
    match (kind, target) {
        (RemoteReaction::Like, Reacted::Post(_)) => Kind::Like,
        (RemoteReaction::Like, Reacted::Comment(_)) => Kind::CommentLike,
        (RemoteReaction::Share, _) => Kind::Share,
    }
}

/// A like or share from another server notifies the local author of the post
/// or comment. `remote_actor_id` is a row of `federation_actors`.
pub async fn remote_reaction_added(pool: &PgPool, kind: RemoteReaction, target: Reacted, remote_actor_id: i64) -> Result<(), sqlx::Error> {
    let query = match target {
        Reacted::Post(_) => signalled!(
            "added",
            "INSERT INTO notifications (user_id, kind, remote_actor_id, post_id)
             SELECT user_id, $1, $2, id FROM posts WHERE id = $3 AND user_id IS NOT NULL"
        ),
        Reacted::Comment(_) => signalled!(
            "added",
            "INSERT INTO notifications (user_id, kind, remote_actor_id, post_id, comment_id)
             SELECT user_id, $1, $2, post_id, id FROM comments WHERE id = $3 AND user_id IS NOT NULL"
        ),
    };
    let (Reacted::Post(id) | Reacted::Comment(id)) = target;
    sqlx::query(query)
        .bind(remote_kind(kind, target).as_str())
        .bind(remote_actor_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Undone on the other server: the notification goes too.
pub async fn remote_reaction_removed(pool: &PgPool, kind: RemoteReaction, target: Reacted, remote_actor_id: i64) -> Result<(), sqlx::Error> {
    let query = match target {
        Reacted::Post(_) => signalled!(
            "removed",
            "DELETE FROM notifications WHERE kind = $1 AND remote_actor_id = $2 AND post_id = $3 AND comment_id IS NULL"
        ),
        Reacted::Comment(_) => signalled!(
            "removed",
            "DELETE FROM notifications WHERE kind = $1 AND remote_actor_id = $2 AND comment_id = $3"
        ),
    };
    let (Reacted::Post(id) | Reacted::Comment(id)) = target;
    sqlx::query(query)
        .bind(remote_kind(kind, target).as_str())
        .bind(remote_actor_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Tells the moderators of a private community that `user_id` asks to join.
pub async fn join_requested(pool: &PgPool, community_id: i64, manager_ids: &[i32], user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, actor_user_id, community_id)
         SELECT manager, $1, $2, $3 FROM UNNEST($4::integer[]) AS manager WHERE manager <> $2"
    ))
    .bind(Kind::JoinRequest.as_str())
    .bind(user_id)
    .bind(community_id)
    .bind(manager_ids)
    .execute(pool)
    .await?;
    Ok(())
}

/// The request is answered (or withdrawn): its notifications go.
pub async fn join_request_answered(pool: &PgPool, community_id: i64, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "removed",
        "DELETE FROM notifications WHERE kind = $1 AND actor_user_id = $2 AND community_id = $3"
    ))
    .bind(Kind::JoinRequest.as_str())
    .bind(user_id)
    .bind(community_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Tells `user_id` that `moderator_id` accepted them in the community.
pub async fn join_approved(pool: &PgPool, community_id: i64, user_id: i32, moderator_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, actor_user_id, community_id) VALUES ($1, $2, $3, $4)"
    ))
    .bind(user_id)
    .bind(Kind::JoinApproved.as_str())
    .bind(moderator_id)
    .bind(community_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `follower_id` followed `user_id`.
pub async fn user_followed(pool: &PgPool, user_id: i32, follower_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, actor_user_id) VALUES ($1, $2, $3)"
    ))
    .bind(user_id)
    .bind(Kind::UserFollow.as_str())
    .bind(follower_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// An unfollow removes the notification of the follow, read or not.
pub async fn user_unfollowed(pool: &PgPool, user_id: i32, follower_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "removed",
        "DELETE FROM notifications WHERE user_id = $1 AND kind = $2 AND actor_user_id = $3"
    ))
    .bind(user_id)
    .bind(Kind::UserFollow.as_str())
    .bind(follower_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Tells `user_id` about a friend request of `actor_id`, or its acceptance.
pub async fn friend_changed(pool: &PgPool, user_id: i32, actor_id: i32, accepted: bool) -> Result<(), sqlx::Error> {
    let kind = if accepted { Kind::FriendAccepted } else { Kind::FriendRequest };
    sqlx::query(signalled!(
        "added",
        "INSERT INTO notifications (user_id, kind, actor_user_id) VALUES ($1, $2, $3)"
    ))
    .bind(user_id)
    .bind(kind.as_str())
    .bind(actor_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A request answered, cancelled, or a friendship ended: the notifications
/// between the two users about it go.
pub async fn friend_removed(pool: &PgPool, first_id: i32, second_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "removed",
        "DELETE FROM notifications WHERE kind IN ($1, $2)
           AND ((user_id = $3 AND actor_user_id = $4) OR (user_id = $4 AND actor_user_id = $3))"
    ))
    .bind(Kind::FriendRequest.as_str())
    .bind(Kind::FriendAccepted.as_str())
    .bind(first_id)
    .bind(second_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The request was accepted: its notification goes, the acceptance stays.
pub async fn friend_request_answered(pool: &PgPool, user_id: i32, requester_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(signalled!(
        "removed",
        "DELETE FROM notifications WHERE kind = $1 AND user_id = $2 AND actor_user_id = $3"
    ))
    .bind(Kind::FriendRequest.as_str())
    .bind(user_id)
    .bind(requester_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Tells the pages of `user_ids` that the conversation changed, see
/// `Change::Message`.
pub async fn signal_message(pool: &PgPool, user_ids: &[i32], conversation_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        // Same name as `CHANNEL`, see the tests below.
        "SELECT pg_notify('bonfire_notifications', person::text || ':message:' || $2::text)
         FROM UNNEST($1::integer[]) AS person",
    )
    .bind(user_ids)
    .bind(conversation_id)
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
                    a.id AS remote_id, a.actor_id AS remote_url, a.username AS remote_username,
                    a.icon_url AS remote_icon, cm.slug AS community_slug, cm.name AS community_name,
                    LEFT(p.body, $2) AS post_excerpt, LENGTH(p.body) > $2 AS post_truncated,
                    LEFT(c.body, $2) AS comment_excerpt, ",
            user_columns!("u."),
            " FROM notifications n
              LEFT JOIN users u ON u.id = n.actor_user_id
              LEFT JOIN federation_actors a ON a.id = n.remote_actor_id
              LEFT JOIN posts p ON p.id = n.post_id
              LEFT JOIN comments c ON c.id = n.comment_id
              LEFT JOIN communities cm ON cm.id = n.community_id
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
        assert_eq!(Change::parse("12:message:7"), Some((12, Change::Message(7))));
        assert_eq!(Change::parse("12:added:all"), None);
        assert_eq!(Change::parse("12"), None);
        assert_eq!(Change::parse("x:read:1"), None);
    }
}
