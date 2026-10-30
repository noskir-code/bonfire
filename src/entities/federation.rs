//! What Bonfire stores about the Fediverse: accounts of other servers that
//! follow local users and communities, and activities waiting to be
//! delivered to them.
//! The protocol itself is in `src/API/`.

use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::schema::{Column, Index, Table};

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
}

/// A remote actor as read from its JSON, before it is saved.
pub struct NewRemoteActor {
    pub actor_id: String,
    pub username: String,
    pub display_name: String,
    pub icon_url: Option<String>,
    pub profile_url: Option<String>,
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
        })
    }
}

/// Columns read by `RemoteActor::from_row`, a macro for `concat!`.
macro_rules! actor_columns {
    () => {
        "id, actor_id, inbox, public_key_pem"
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

/// Adds the actor, or updates it when it's already known.
pub async fn save_actor(pool: &PgPool, actor: &NewRemoteActor) -> Result<RemoteActor, sqlx::Error> {
    let row = sqlx::query(concat!(
        "INSERT INTO federation_actors
            (actor_id, username, inbox, shared_inbox, key_id, public_key_pem, display_name, icon_url, profile_url)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (actor_id) DO UPDATE SET
            username = EXCLUDED.username, inbox = EXCLUDED.inbox,
            shared_inbox = EXCLUDED.shared_inbox, key_id = EXCLUDED.key_id,
            public_key_pem = EXCLUDED.public_key_pem, display_name = EXCLUDED.display_name,
            icon_url = EXCLUDED.icon_url, profile_url = EXCLUDED.profile_url, fetched_at = NOW()
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
    .fetch_one(pool)
    .await?;
    RemoteActor::from_row(&row)
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

pub async fn follower_count(pool: &PgPool, user_id: i32) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM federation_followers WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
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
