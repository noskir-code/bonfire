use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    author::{Author, remote_author_columns},
    post::{self, BodyError, post_visible},
    schema::{Column, Index, Table},
    user::user_columns,
};

pub const BODY_MAX_CHARS: usize = 2_000;

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "comments",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // Comments are deleted with their post or their author: a local
        // user, or for a reply received from another server, its author there.
        Column::new("post_id", "BIGINT").references("posts(id) ON DELETE CASCADE"),
        Column::new("user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE CASCADE"),
        Column::new("remote_actor_id", "BIGINT")
            .nullable()
            .references("federation_actors(id) ON DELETE CASCADE"),
        // Its ActivityPub id, for replies of other servers.
        Column::new("ap_id", "TEXT").nullable(),
        // The comment this one replies to, `NULL` for a comment on the post itself.
        Column::new("parent_id", "BIGINT")
            .nullable()
            .references("comments(id) ON DELETE CASCADE"),
        Column::new("body", "TEXT"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("updated_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::new("comments_post_created_at_idx", "post_id, created_at"),
        Index::unique("comments_ap_id_idx", "ap_id"),
    ],
};

/// Columns read by `Comment::from_row`, with the joins they need: the
/// comment as `c`, its author as `u`. A macro so it can be used in `concat!`.
macro_rules! comment_select {
    () => {
        concat!(
            "SELECT c.id AS comment_id, c.body, c.created_at AS commented_at, c.post_id, c.ap_id,
                    c.parent_id, COALESCE(pu.username, pra.username) AS parent_author, ",
            user_columns!("u."),
            ", ",
            remote_author_columns!(),
            " FROM comments c
              LEFT JOIN users u ON u.id = c.user_id
              LEFT JOIN federation_actors ra ON ra.id = c.remote_actor_id
              LEFT JOIN comments pc ON pc.id = c.parent_id
              LEFT JOIN users pu ON pu.id = pc.user_id
              LEFT JOIN federation_actors pra ON pra.id = pc.remote_actor_id"
        )
    };
}

pub struct Comment {
    pub id: i64,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub author: Author,
    /// ActivityPub id, for replies received from other servers.
    pub ap_id: Option<String>,
    /// The comment it replies to, and that comment's author.
    pub parent_id: Option<i64>,
    pub parent_author: Option<String>,
}

impl Comment {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("comment_id")?,
            body: row.try_get("body")?,
            created_at: row.try_get("commented_at")?,
            author: Author::from_row(row)?,
            ap_id: row.try_get("ap_id")?,
            parent_id: row.try_get("parent_id")?,
            parent_author: row.try_get("parent_author")?,
        })
    }

    pub fn created_at_iso(&self) -> String {
        post::time_iso(&self.created_at)
    }

    pub fn created_at_label(&self) -> String {
        post::time_label(&self.created_at)
    }
}

/// Characters of the post shown above a comment on a profile.
const POST_EXCERPT_CHARS: i32 = 100;

/// A comment on a profile, with the post it answers. Its author is the
/// profile's user, so only the post's author is loaded.
pub struct ProfileComment {
    pub id: i64,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub post_id: i64,
    pub post_author: String,
    /// Start of the post, `post_truncated` when there is more.
    pub post_excerpt: String,
    pub post_truncated: bool,
}

impl ProfileComment {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("comment_id")?,
            body: row.try_get("body")?,
            created_at: row.try_get("commented_at")?,
            post_id: row.try_get("post_id")?,
            post_author: row.try_get("post_author")?,
            post_excerpt: row.try_get("post_excerpt")?,
            post_truncated: row.try_get("post_truncated")?,
        })
    }

    pub fn created_at_iso(&self) -> String {
        post::time_iso(&self.created_at)
    }

    pub fn created_at_label(&self) -> String {
        post::time_label(&self.created_at)
    }
}

/// Comments of a user, newest first, only on posts the viewer can see
/// (same rule as `post::find`).
pub async fn by_author(
    pool: &PgPool,
    author_id: i32,
    viewer_id: Option<i32>,
) -> Result<Vec<ProfileComment>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT c.id AS comment_id, c.body, c.created_at AS commented_at, p.id AS post_id,
                COALESCE(pu.username, pra.username) AS post_author,
                LEFT(p.body, $3) AS post_excerpt, LENGTH(p.body) > $3 AS post_truncated
         FROM comments c
         JOIN posts p ON p.id = c.post_id
         LEFT JOIN users pu ON pu.id = p.user_id
         LEFT JOIN federation_actors pra ON pra.id = p.remote_actor_id
         LEFT JOIN communities cm ON cm.id = p.community_id
         WHERE c.user_id = $2 AND ",
        post_visible!(),
        " ORDER BY c.created_at DESC, c.id DESC
         LIMIT $4"
    ))
    .bind(viewer_id)
    .bind(author_id)
    .bind(POST_EXCERPT_CHARS)
    .bind(post::PAGE_SIZE)
    .fetch_all(pool)
    .await?
    .iter()
    .map(ProfileComment::from_row)
    .collect()
}

pub fn validate_body(raw: &str) -> Result<String, BodyError> {
    post::validate_text(raw, BODY_MAX_CHARS)
}

/// The post must exist and be visible to the author, see `post::find`, and
/// `parent_id` be a comment of the same post, see `parent_author`.
pub async fn create(
    pool: &PgPool,
    post_id: i64,
    user_id: i32,
    parent_id: Option<i64>,
    body: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO comments (post_id, user_id, parent_id, body) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(post_id)
    .bind(user_id)
    .bind(parent_id)
    .bind(body)
    .fetch_one(pool)
    .await
}

/// The comment `comment_id` on `post_id`: its local author, `None` inside
/// when it came from another server. `None` when it's not on that post.
pub async fn parent_author(pool: &PgPool, post_id: i64, comment_id: i64) -> Result<Option<Option<i32>>, sqlx::Error> {
    sqlx::query_scalar("SELECT user_id FROM comments WHERE id = $1 AND post_id = $2")
        .bind(comment_id)
        .bind(post_id)
        .fetch_optional(pool)
        .await
}

/// A reply received from another server. Returns `None` when a comment with
/// this ActivityPub id is already stored.
pub async fn create_remote(
    pool: &PgPool,
    post_id: i64,
    remote_actor_id: i64,
    parent_id: Option<i64>,
    ap_id: &str,
    body: &str,
    published: Option<DateTime<Utc>>,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO comments (post_id, remote_actor_id, parent_id, ap_id, body, created_at)
         VALUES ($1, $2, $3, $4, $5, COALESCE($6, NOW()))
         ON CONFLICT (ap_id) DO NOTHING
         RETURNING id",
    )
    .bind(post_id)
    .bind(remote_actor_id)
    .bind(parent_id)
    .bind(ap_id)
    .bind(body)
    .bind(published)
    .fetch_optional(pool)
    .await
}

/// Local id and post of a comment from its ActivityPub id, for one received.
pub async fn find_by_ap_id(pool: &PgPool, ap_id: &str) -> Result<Option<(i64, i64)>, sqlx::Error> {
    let row = sqlx::query("SELECT id, post_id FROM comments WHERE ap_id = $1")
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    row.map(|row| Ok((row.try_get("id")?, row.try_get("post_id")?))).transpose()
}

/// A comment with its post id, `None` unless a visitor can see the post:
/// what other Fediverse servers may see.
pub async fn find_public(pool: &PgPool, comment_id: i64) -> Result<Option<(Comment, i64)>, sqlx::Error> {
    sqlx::query(concat!(
        comment_select!(),
        " JOIN posts p ON p.id = c.post_id
          LEFT JOIN communities cm ON cm.id = p.community_id
         WHERE c.id = $2 AND ",
        post_visible!()
    ))
    // `$1` is the viewer of `post_visible!`: nobody.
    .bind(None::<i32>)
    .bind(comment_id)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(|row| Ok((Comment::from_row(row)?, row.try_get("post_id")?)))
    .transpose()
}

/// Comments of a post, oldest first like a conversation.
pub async fn for_post(pool: &PgPool, post_id: i64) -> Result<Vec<Comment>, sqlx::Error> {
    sqlx::query(concat!(
        comment_select!(),
        " WHERE c.post_id = $1
         ORDER BY c.created_at, c.id"
    ))
    .bind(post_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(Comment::from_row)
    .collect()
}

/// A comment on the post with all the replies below it, at any depth.
pub struct Thread {
    pub comment: Comment,
    /// Oldest first. Replies to replies are here too, showing who they answer:
    /// one level of indentation stays readable on a phone.
    pub replies: Vec<Comment>,
}

/// Groups comments (oldest first) under the comment on the post they come from.
pub fn threads(comments: Vec<Comment>) -> Vec<Thread> {
    use std::collections::HashMap;

    let parents: HashMap<i64, Option<i64>> =
        comments.iter().map(|comment| (comment.id, comment.parent_id)).collect();
    // Follows the parents up to a comment on the post. Bounded, in case of a cycle.
    let root = |mut id: i64| {
        for _ in 0..parents.len() {
            match parents.get(&id).copied().flatten() {
                Some(parent) if parents.contains_key(&parent) => id = parent,
                _ => break,
            }
        }
        id
    };

    let mut threads: Vec<Thread> = Vec::new();
    let mut positions: HashMap<i64, usize> = HashMap::new();
    for comment in comments {
        let root_id = root(comment.id);
        match positions.get(&root_id) {
            Some(&position) if root_id != comment.id => threads[position].replies.push(comment),
            _ => {
                positions.insert(comment.id, threads.len());
                threads.push(Thread { comment, replies: Vec::new() });
            }
        }
    }
    threads
}
