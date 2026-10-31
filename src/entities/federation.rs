//! What Bonfire stores about the Fediverse: accounts of other servers that
//! follow local users and communities, the ones local users follow, likes
//! and shares from other servers, and activities waiting to be delivered.
//! The protocol itself is in `src/API/`.

use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    author::RemoteAuthor,
    media,
    schema::{Column, Index, Table},
};

/// Accounts of other servers, cached so their key isn't fetched on every request.
pub const ACTORS_TABLE: Table = Table {
    name: "federation_actors",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // Their ActivityPub id, like https://mastodon.social/users/alice
        Column::new("actor_id", "TEXT"),
        Column::new("username", "TEXT").default("''"),
        // Shown next to their posts and comments.
        Column::new("display_name", "TEXT").default("''"),
        Column::new("icon_url", "TEXT").nullable(),
        Column::new("profile_url", "TEXT").nullable(),
        // Their bio, as plain text.
        Column::new("summary", "TEXT").default("''"),
        // Their followers collection: posts addressed to it are followers-only.
        Column::new("followers_url", "TEXT").nullable(),
        // Blocked by the site's staff: what they send is ignored.
        Column::new("blocked_at", "TIMESTAMPTZ").nullable(),
        Column::new("inbox", "TEXT"),
        // One inbox for the whole server, so a post reaches it once.
        Column::new("shared_inbox", "TEXT").nullable(),
        Column::new("key_id", "TEXT"),
        Column::new("public_key_pem", "TEXT"),
        Column::new("fetched_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("federation_actors_actor_id_idx", "actor_id"),
        Index::new("federation_actors_key_id_idx", "key_id"),
    ],
};

/// Accounts of other servers following a local user.
pub const FOLLOWERS_TABLE: Table = Table {
    name: "federation_followers",
    columns: &[
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("actor_id", "BIGINT").references("federation_actors(id) ON DELETE CASCADE"),
        // Id of their Follow activity, an Undo can refer to it.
        Column::new("follow_id", "TEXT").default("''"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("federation_followers_user_actor_idx", "user_id, actor_id"),
        Index::new("federation_followers_actor_idx", "actor_id"),
    ],
};

/// Accounts of other servers following a local community: a Misskey user
/// following `@cooking@domain`, for example. Following is how they join: only
/// followers can post in the community.
pub const COMMUNITY_FOLLOWERS_TABLE: Table = Table {
    name: "community_followers",
    columns: &[
        Column::new("community_id", "BIGINT").references("communities(id) ON DELETE CASCADE"),
        Column::new("actor_id", "BIGINT").references("federation_actors(id) ON DELETE CASCADE"),
        Column::new("follow_id", "TEXT").default("''"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("community_followers_community_actor_idx", "community_id, actor_id"),
        Index::new("community_followers_actor_idx", "actor_id"),
    ],
};

/// Accounts of other servers followed by local users. `accepted` once their
/// server answered the Follow with an Accept.
pub const FOLLOWING_TABLE: Table = Table {
    name: "federation_following",
    columns: &[
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("actor_id", "BIGINT").references("federation_actors(id) ON DELETE CASCADE"),
        // Id of the Follow activity sent, their Accept refers to it.
        Column::new("follow_id", "TEXT"),
        Column::new("accepted", "BOOLEAN").default("FALSE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("federation_following_user_actor_idx", "user_id, actor_id"),
        Index::new("federation_following_actor_idx", "actor_id"),
        Index::new("federation_following_follow_idx", "follow_id"),
    ],
};

/// Likes and shares made on other servers, of posts and comments stored
/// here. Misskey reactions (emojis) count as likes, the emoji is kept.
pub const REACTIONS_TABLE: Table = Table {
    name: "remote_reactions",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // `like` or `share`.
        Column::new("kind", "TEXT"),
        Column::new("actor_id", "BIGINT").references("federation_actors(id) ON DELETE CASCADE"),
        // One of them is set.
        Column::new("post_id", "BIGINT").nullable().references("posts(id) ON DELETE CASCADE"),
        Column::new("comment_id", "BIGINT")
            .nullable()
            .references("comments(id) ON DELETE CASCADE"),
        // Id of their Like or Announce, an Undo can refer to it.
        Column::new("activity_id", "TEXT").default("''"),
        Column::new("emoji", "TEXT").default("''"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        // One like and one share per account and object. `ON CONFLICT` in
        // `add_reaction` repeats this expression.
        Index::unique(
            "remote_reactions_unique_idx",
            "kind, actor_id, COALESCE(post_id, 0), COALESCE(comment_id, 0)",
        ),
        Index::new("remote_reactions_post_idx", "post_id, kind"),
        Index::new("remote_reactions_comment_idx", "comment_id, kind"),
        Index::new("remote_reactions_activity_idx", "activity_id"),
        // Shares by followed accounts in the feed.
        Index::new("remote_reactions_actor_created_idx", "actor_id, created_at DESC"),
    ],
};

/// Activities to send, kept until the other server accepts them so a server
/// that is down, or a restart of Bonfire, doesn't lose them.
pub const DELIVERIES_TABLE: Table = Table {
    name: "federation_deliveries",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // The activity is signed with the key of this user, or of this community.
        Column::new("user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE CASCADE"),
        Column::new("community_id", "BIGINT")
            .nullable()
            .references("communities(id) ON DELETE CASCADE"),
        Column::new("inbox", "TEXT"),
        // JSON, sent as is.
        Column::new("activity", "TEXT"),
        Column::new("attempts", "INTEGER").default("0"),
        Column::new("next_attempt_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("last_error", "TEXT").default("''"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[Index::new("federation_deliveries_next_attempt_idx", "next_attempt_at")],
};

/// An account of another server.
#[derive(Clone, Debug)]
pub struct RemoteActor {
    pub id: i64,
    pub actor_id: String,
    pub inbox: String,
    pub public_key_pem: String,
    /// `None` for accounts saved before it was read.
    pub followers_url: Option<String>,
    /// Blocked by the staff, see `set_blocked`.
    pub blocked: bool,
}

/// A remote actor as read from its JSON, before it is saved.
pub struct NewRemoteActor {
    pub actor_id: String,
    pub username: String,
    pub display_name: String,
    pub icon_url: Option<String>,
    pub profile_url: Option<String>,
    pub summary: String,
    pub followers_url: Option<String>,
    pub inbox: String,
    pub shared_inbox: Option<String>,
    pub key_id: String,
    pub public_key_pem: String,
}

impl RemoteActor {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            actor_id: row.try_get("actor_id")?,
            inbox: row.try_get("inbox")?,
            public_key_pem: row.try_get("public_key_pem")?,
            followers_url: row.try_get("followers_url")?,
            blocked: row.try_get("blocked")?,
        })
    }
}

/// Columns read by `RemoteActor::from_row`, a macro for `concat!`.
macro_rules! actor_columns {
    () => {
        "id, actor_id, inbox, public_key_pem, followers_url, blocked_at IS NOT NULL AS blocked"
    };
}

pub async fn find_actor_by_key(pool: &PgPool, key_id: &str) -> Result<Option<RemoteActor>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT ",
        actor_columns!(),
        " FROM federation_actors WHERE key_id = $1 LIMIT 1"
    ))
    .bind(key_id)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(RemoteActor::from_row)
    .transpose()
}

/// An actor by its ActivityPub id, with when it was last fetched.
pub async fn find_actor(pool: &PgPool, actor_id: &str) -> Result<Option<(RemoteActor, DateTime<Utc>)>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT ",
        actor_columns!(),
        ", fetched_at FROM federation_actors WHERE actor_id = $1"
    ))
    .bind(actor_id)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(|row| Ok((RemoteActor::from_row(row)?, row.try_get("fetched_at")?)))
    .transpose()
}

/// Columns read by `profile_from_row`, a macro for `concat!`.
macro_rules! profile_columns {
    () => {
        "a.id AS ra_id, a.actor_id AS ra_actor_id, a.username AS ra_username,
         a.display_name AS ra_display_name, a.icon_url AS ra_icon_url,
         a.profile_url AS ra_profile_url, a.summary"
    };
}

fn profile_from_row(row: &PgRow) -> Result<(RemoteAuthor, String), sqlx::Error> {
    Ok((RemoteAuthor::from_row(row)?, row.try_get("summary")?))
}

/// An account of another server as shown on its page here, with its bio.
pub async fn actor_profile(pool: &PgPool, id: i64) -> Result<Option<(RemoteAuthor, String)>, sqlx::Error> {
    sqlx::query(concat!("SELECT ", profile_columns!(), " FROM federation_actors a WHERE a.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(profile_from_row)
        .transpose()
}

/// Adds the actor, or updates it when it's already known.
pub async fn save_actor(pool: &PgPool, actor: &NewRemoteActor) -> Result<RemoteActor, sqlx::Error> {
    if let Some(icon) = &actor.icon_url {
        media::register(pool, icon).await?;
    }
    let row = sqlx::query(concat!(
        "INSERT INTO federation_actors
            (actor_id, username, inbox, shared_inbox, key_id, public_key_pem, display_name, icon_url, profile_url,
             summary, followers_url)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (actor_id) DO UPDATE SET
            username = EXCLUDED.username, inbox = EXCLUDED.inbox,
            shared_inbox = EXCLUDED.shared_inbox, key_id = EXCLUDED.key_id,
            public_key_pem = EXCLUDED.public_key_pem, display_name = EXCLUDED.display_name,
            icon_url = EXCLUDED.icon_url, profile_url = EXCLUDED.profile_url,
            summary = EXCLUDED.summary, followers_url = EXCLUDED.followers_url, fetched_at = NOW()
         RETURNING ",
        actor_columns!()
    ))
    .bind(&actor.actor_id)
    .bind(&actor.username)
    .bind(&actor.inbox)
    .bind(&actor.shared_inbox)
    .bind(&actor.key_id)
    .bind(&actor.public_key_pem)
    .bind(&actor.display_name)
    .bind(&actor.icon_url)
    .bind(&actor.profile_url)
    .bind(&actor.summary)
    .bind(&actor.followers_url)
    .fetch_one(pool)
    .await?;
    RemoteActor::from_row(&row)
}

/// Blocks an account of another server, or lifts the block. Blocking removes
/// what it posted here, its likes and shares, and its follows both ways: it
/// gets nothing more from here, and what it sends is ignored.
pub async fn set_blocked(pool: &PgPool, id: i64, blocked: bool) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("UPDATE federation_actors SET blocked_at = CASE WHEN $2 THEN NOW() END WHERE id = $1")
        .bind(id)
        .bind(blocked)
        .execute(&mut *transaction)
        .await?;
    if blocked {
        for statement in [
            "DELETE FROM posts WHERE remote_actor_id = $1",
            "DELETE FROM comments WHERE remote_actor_id = $1",
            "DELETE FROM remote_reactions WHERE actor_id = $1",
            "DELETE FROM federation_followers WHERE actor_id = $1",
            "DELETE FROM community_followers WHERE actor_id = $1",
            "DELETE FROM federation_following WHERE actor_id = $1",
        ] {
            sqlx::query(statement).bind(id).execute(&mut *transaction).await?;
        }
    }
    transaction.commit().await
}

/// Forgets an account deleted on its server, and its follows.
pub async fn delete_actor(pool: &PgPool, actor_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM federation_actors WHERE actor_id = $1")
        .bind(actor_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns `false` when the actor already followed: servers send the Follow
/// again when they didn't get the Accept.
pub async fn add_follower(pool: &PgPool, user_id: i32, actor: &RemoteActor, follow_id: &str) -> Result<bool, sqlx::Error> {
    // xmax is 0 on a row just inserted, not on one updated by the conflict.
    sqlx::query_scalar(
        "INSERT INTO federation_followers (user_id, actor_id, follow_id) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, actor_id) DO UPDATE SET follow_id = EXCLUDED.follow_id
         RETURNING (xmax = 0)",
    )
    .bind(user_id)
    .bind(actor.id)
    .bind(follow_id)
    .fetch_one(pool)
    .await
}

/// Removes the follow of `actor` on `user_id`.
pub async fn remove_follower(pool: &PgPool, user_id: i32, actor: &RemoteActor) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM federation_followers WHERE user_id = $1 AND actor_id = $2")
        .bind(user_id)
        .bind(actor.id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Removes the follow made by the Follow activity `follow_id`, for an Undo
/// that only gives the id.
pub async fn remove_follow(pool: &PgPool, actor: &RemoteActor, follow_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM federation_followers WHERE actor_id = $1 AND follow_id = $2")
        .bind(actor.id)
        .bind(follow_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns `false` when the actor already followed the community.
pub async fn add_community_follower(
    pool: &PgPool,
    community_id: i64,
    actor: &RemoteActor,
    follow_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO community_followers (community_id, actor_id, follow_id) VALUES ($1, $2, $3)
         ON CONFLICT (community_id, actor_id) DO UPDATE SET follow_id = EXCLUDED.follow_id
         RETURNING (xmax = 0)",
    )
    .bind(community_id)
    .bind(actor.id)
    .bind(follow_id)
    .fetch_one(pool)
    .await
}

pub async fn remove_community_follower(pool: &PgPool, community_id: i64, actor: &RemoteActor) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM community_followers WHERE community_id = $1 AND actor_id = $2")
        .bind(community_id)
        .bind(actor.id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether the actor follows the community: then they may post in it.
pub async fn follows_community(pool: &PgPool, community_id: i64, actor: &RemoteActor) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM community_followers WHERE community_id = $1 AND actor_id = $2)",
    )
    .bind(community_id)
    .bind(actor.id)
    .fetch_one(pool)
    .await
}

pub async fn community_follower_count(pool: &PgPool, community_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM community_followers WHERE community_id = $1")
        .bind(community_id)
        .fetch_one(pool)
        .await
}

/// Inboxes reaching every follower of the community, once per server.
pub async fn community_follower_inboxes(pool: &PgPool, community_id: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT COALESCE(a.shared_inbox, a.inbox)
         FROM community_followers f JOIN federation_actors a ON a.id = f.actor_id
         WHERE f.community_id = $1",
    )
    .bind(community_id)
    .fetch_all(pool)
    .await
}

/// The inbox reaching a remote actor, by its ActivityPub id: their server's
/// shared inbox when it has one, so a server also reached through followers
/// gets the activity once.
pub async fn actor_inbox(pool: &PgPool, actor_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT COALESCE(shared_inbox, inbox) FROM federation_actors WHERE actor_id = $1")
        .bind(actor_id)
        .fetch_optional(pool)
        .await
}

/// Public posts of a user, shown as their post count on other servers.
pub async fn public_post_count(pool: &PgPool, user_id: i32) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE user_id = $1 AND visibility = 'public'")
        .bind(user_id)
        .fetch_one(pool)
        .await
}

/// For NodeInfo: local accounts and public posts written here.
pub async fn instance_counts(pool: &PgPool) -> Result<(i64, i64), sqlx::Error> {
    let row = sqlx::query(
        "SELECT (SELECT COUNT(*) FROM users) AS users,
                (SELECT COUNT(*) FROM posts WHERE visibility = 'public' AND user_id IS NOT NULL) AS posts",
    )
    .fetch_one(pool)
    .await?;
    Ok((row.try_get("users")?, row.try_get("posts")?))
}

/// Inboxes reaching every follower of these users, once per server when
/// it has a shared inbox.
pub async fn follower_inboxes(pool: &PgPool, user_ids: &[i32]) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT COALESCE(a.shared_inbox, a.inbox)
         FROM federation_followers f JOIN federation_actors a ON a.id = f.actor_id
         WHERE f.user_id = ANY($1)",
    )
    .bind(user_ids)
    .fetch_all(pool)
    .await
}

// --- Following accounts of other servers ---

/// Saves a Follow sent by `user_id`, waiting for its Accept. Following again
/// replaces the previous Follow.
pub async fn add_following(pool: &PgPool, user_id: i32, actor_id: i64, follow_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO federation_following (user_id, actor_id, follow_id) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, actor_id) DO UPDATE SET follow_id = EXCLUDED.follow_id, accepted = FALSE",
    )
    .bind(user_id)
    .bind(actor_id)
    .bind(follow_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes the follow, returning the id of the Follow to undo.
pub async fn remove_following(pool: &PgPool, user_id: i32, actor_id: i64) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("DELETE FROM federation_following WHERE user_id = $1 AND actor_id = $2 RETURNING follow_id")
        .bind(user_id)
        .bind(actor_id)
        .fetch_optional(pool)
        .await
}

/// Their Accept of the Follow `follow_id`, or of the follow of `user_id` when
/// their server doesn't say which Follow. Returns whether a follow changed.
pub async fn accept_following(pool: &PgPool, actor: &RemoteActor, follow_id: Option<&str>, user_id: i32) -> Result<bool, sqlx::Error> {
    let accepted = sqlx::query(
        "UPDATE federation_following SET accepted = TRUE
         WHERE actor_id = $1 AND user_id = $3 AND ($2::text IS NULL OR follow_id = $2)",
    )
    .bind(actor.id)
    .bind(follow_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(accepted.rows_affected() > 0)
}

/// Their Reject of a Follow, or the removal of an accepted one.
pub async fn reject_following(pool: &PgPool, actor: &RemoteActor, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM federation_following WHERE actor_id = $1 AND user_id = $2")
        .bind(actor.id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// `None` when `user_id` doesn't follow the actor, else whether it's accepted.
pub async fn following_state(pool: &PgPool, user_id: i32, actor_id: i64) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar("SELECT accepted FROM federation_following WHERE user_id = $1 AND actor_id = $2")
        .bind(user_id)
        .bind(actor_id)
        .fetch_optional(pool)
        .await
}

/// A local user following the actor `actor_id` (its ActivityPub id), whose
/// key can fetch what it sends. `None` when nobody here follows it.
pub async fn a_local_follower(pool: &PgPool, actor_id: &str) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT f.user_id FROM federation_following f JOIN federation_actors a ON a.id = f.actor_id
         WHERE a.actor_id = $1 AND f.accepted ORDER BY f.created_at LIMIT 1",
    )
    .bind(actor_id)
    .fetch_optional(pool)
    .await
}

// --- Likes and shares from other servers ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteReaction {
    Like,
    Share,
}

impl RemoteReaction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Share => "share",
        }
    }

    fn parse(value: &str) -> Self {
        if value == "share" { Self::Share } else { Self::Like }
    }
}

/// What a remote reaction is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reacted {
    Post(i64),
    Comment(i64),
}

impl Reacted {
    fn ids(self) -> (Option<i64>, Option<i64>) {
        match self {
            Self::Post(id) => (Some(id), None),
            Self::Comment(id) => (None, Some(id)),
        }
    }
}

/// Saves the reaction. Returns `false` when it was already there: servers
/// send activities again when they're not sure they arrived.
pub async fn add_reaction(
    pool: &PgPool,
    kind: RemoteReaction,
    actor: &RemoteActor,
    target: Reacted,
    activity_id: &str,
    emoji: &str,
) -> Result<bool, sqlx::Error> {
    let (post_id, comment_id) = target.ids();
    sqlx::query_scalar(
        "INSERT INTO remote_reactions (kind, actor_id, post_id, comment_id, activity_id, emoji)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (kind, actor_id, COALESCE(post_id, 0), COALESCE(comment_id, 0))
         DO UPDATE SET activity_id = EXCLUDED.activity_id, emoji = EXCLUDED.emoji
         RETURNING (xmax = 0)",
    )
    .bind(kind.as_str())
    .bind(actor.id)
    .bind(post_id)
    .bind(comment_id)
    .bind(activity_id)
    .bind(emoji)
    .fetch_one(pool)
    .await
}

/// Removes a reaction of `actor`, found by the id of its activity, or else by
/// its kind and target. Returns what was removed.
pub async fn remove_reaction(
    pool: &PgPool,
    actor: &RemoteActor,
    activity_id: &str,
    fallback: Option<(RemoteReaction, Reacted)>,
) -> Result<Option<(RemoteReaction, Reacted)>, sqlx::Error> {
    let (kind, post_id, comment_id) = match fallback {
        Some((kind, target)) => {
            let (post_id, comment_id) = target.ids();
            (Some(kind.as_str()), post_id, comment_id)
        }
        None => (None, None, None),
    };
    let row = sqlx::query(
        "DELETE FROM remote_reactions WHERE id = (
            SELECT id FROM remote_reactions WHERE actor_id = $1 AND (
                (activity_id <> '' AND activity_id = $2)
                OR (kind = $3 AND post_id IS NOT DISTINCT FROM $4 AND comment_id IS NOT DISTINCT FROM $5))
            LIMIT 1)
         RETURNING kind, post_id, comment_id",
    )
    .bind(actor.id)
    .bind(activity_id)
    .bind(kind)
    .bind(post_id)
    .bind(comment_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        let kind = RemoteReaction::parse(&row.try_get::<String, _>("kind")?);
        let target = match (row.try_get::<Option<i64>, _>("post_id")?, row.try_get::<Option<i64>, _>("comment_id")?) {
            (Some(post_id), _) => Reacted::Post(post_id),
            (None, comment_id) => Reacted::Comment(comment_id.unwrap_or_default()),
        };
        Ok((kind, target))
    })
    .transpose()
}

/// Remote accounts that commented on a post: they have it, so they are told
/// when it's deleted.
pub async fn commenters_of(pool: &PgPool, post_id: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT a.actor_id FROM comments c JOIN federation_actors a ON a.id = c.remote_actor_id
         WHERE c.post_id = $1",
    )
    .bind(post_id)
    .fetch_all(pool)
    .await
}

/// Followers of a community with the ids of their Follows, to tell them the
/// community left the Fediverse.
pub async fn community_follows(pool: &PgPool, community_id: i64) -> Result<Vec<(String, String, String)>, sqlx::Error> {
    sqlx::query(
        "SELECT a.actor_id, a.inbox, f.follow_id FROM community_followers f
         JOIN federation_actors a ON a.id = f.actor_id WHERE f.community_id = $1",
    )
    .bind(community_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|row| Ok((row.try_get("actor_id")?, row.try_get("inbox")?, row.try_get("follow_id")?)))
    .collect()
}

pub async fn remove_community_followers(pool: &PgPool, community_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM community_followers WHERE community_id = $1")
        .bind(community_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Who signs an activity: a local user or a local community.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signer {
    User(i32),
    Community(i64),
}

/// RSA keys of a signer: (public id, public PEM, private PEM).
/// `None` until they are made, see `API::linker::keys`.
pub async fn local_keys(pool: &PgPool, signer: Signer) -> Result<Option<(i64, String, String)>, sqlx::Error> {
    let query = match signer {
        Signer::User(_) => {
            "SELECT public_id, ap_public_key, ap_private_key FROM users
             WHERE id = $1 AND ap_public_key IS NOT NULL AND ap_private_key IS NOT NULL"
        }
        Signer::Community(_) => {
            "SELECT public_id, ap_public_key, ap_private_key FROM communities
             WHERE id = $1 AND ap_public_key IS NOT NULL AND ap_private_key IS NOT NULL"
        }
    };
    let row = sqlx::query(query)
        .bind(signer_id(signer))
        .fetch_optional(pool)
        .await?;
    row.map(|row| {
        Ok((
            row.try_get("public_id")?,
            row.try_get("ap_public_key")?,
            row.try_get("ap_private_key")?,
        ))
    })
    .transpose()
}

/// Both ids as one number, for queries taking either.
fn signer_id(signer: Signer) -> i64 {
    match signer {
        Signer::User(id) => i64::from(id),
        Signer::Community(id) => id,
    }
}

/// Saves the keys unless the signer already has some: when two requests make
/// keys at the same time, the first one wins and both use it.
pub async fn save_local_keys(pool: &PgPool, signer: Signer, public_pem: &str, private_pem: &str) -> Result<(), sqlx::Error> {
    let query = match signer {
        Signer::User(_) => {
            "UPDATE users SET ap_public_key = $1, ap_private_key = $2 WHERE id = $3 AND ap_private_key IS NULL"
        }
        Signer::Community(_) => {
            "UPDATE communities SET ap_public_key = $1, ap_private_key = $2 WHERE id = $3 AND ap_private_key IS NULL"
        }
    };
    sqlx::query(query)
        .bind(public_pem)
        .bind(private_pem)
        .bind(signer_id(signer))
        .execute(pool)
        .await?;
    Ok(())
}

/// Queues `activity`, signed by `signer`, for each inbox.
pub async fn enqueue(pool: &PgPool, signer: Signer, inboxes: &[String], activity: &str) -> Result<(), sqlx::Error> {
    if inboxes.is_empty() {
        return Ok(());
    }
    let (user_id, community_id) = match signer {
        Signer::User(id) => (Some(id), None),
        Signer::Community(id) => (None, Some(id)),
    };
    sqlx::query(
        "INSERT INTO federation_deliveries (user_id, community_id, inbox, activity)
         SELECT $1, $4, inbox, $3 FROM UNNEST($2::text[]) AS inbox",
    )
    .bind(user_id)
    .bind(inboxes)
    .bind(activity)
    .bind(community_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct Delivery {
    pub id: i64,
    pub signer: Signer,
    pub inbox: String,
    pub activity: String,
    pub attempts: i32,
}

/// How long a claimed delivery is reserved: if the server stops while sending,
/// the delivery is tried again after that.
const CLAIM_MINUTES: i32 = 10;

/// Takes up to `limit` deliveries that are due. Several Bonfire servers on the
/// same database never take the same one.
pub async fn claim_deliveries(pool: &PgPool, limit: i64) -> Result<Vec<Delivery>, sqlx::Error> {
    sqlx::query(
        "UPDATE federation_deliveries SET next_attempt_at = NOW() + make_interval(mins => $2)
         WHERE id IN (
            SELECT id FROM federation_deliveries WHERE next_attempt_at <= NOW()
            ORDER BY next_attempt_at LIMIT $1 FOR UPDATE SKIP LOCKED
         )
         RETURNING id, user_id, community_id, inbox, activity, attempts",
    )
    .bind(limit)
    .bind(CLAIM_MINUTES)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|row| {
        let user_id: Option<i32> = row.try_get("user_id")?;
        let community_id: Option<i64> = row.try_get("community_id")?;
        Ok(Delivery {
            id: row.try_get("id")?,
            // One of them is always set, see `enqueue`.
            signer: match (user_id, community_id) {
                (Some(id), _) => Signer::User(id),
                (None, id) => Signer::Community(id.unwrap_or_default()),
            },
            inbox: row.try_get("inbox")?,
            activity: row.try_get("activity")?,
            attempts: row.try_get("attempts")?,
        })
    })
    .collect()
}

/// When the next delivery is due, `None` when there is nothing to send.
pub async fn next_delivery_at(pool: &PgPool) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar("SELECT MIN(next_attempt_at) FROM federation_deliveries")
        .fetch_one(pool)
        .await
}

/// Sent, or abandoned.
pub async fn finish_delivery(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM federation_deliveries WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn retry_delivery(pool: &PgPool, id: i64, delay_seconds: f64, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE federation_deliveries
         SET attempts = attempts + 1, next_attempt_at = NOW() + make_interval(secs => $2), last_error = $3
         WHERE id = $1",
    )
    .bind(id)
    .bind(delay_seconds)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}
