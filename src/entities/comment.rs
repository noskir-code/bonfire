use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    author::{Author, remote_author_columns},
    media::{self, Attachment},
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
        Column::new("sensitive", "BOOLEAN").default("FALSE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("updated_at", "TIMESTAMPTZ").default("NOW()"),
        // Last edit by its author, shown as "edited".
        Column::new("edited_at", "TIMESTAMPTZ").nullable(),
    ],
    indexes: &[
        Index::new("comments_post_created_at_idx", "post_id, created_at"),
        Index::new("comments_user_idx", "user_id, created_at DESC"),
        Index::unique("comments_ap_id_idx", "ap_id"),
    ],
};

/// One row per user who liked a comment.
pub const LIKES_TABLE: Table = Table {
    name: "comment_likes",
    columns: &[
        Column::new("comment_id", "BIGINT").references("comments(id) ON DELETE CASCADE"),
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[Index::unique("comment_likes_comment_user_idx", "comment_id, user_id")],
};

/// Columns read by `Comment::from_row`, with the joins they need: the
/// comment as `c`, its author as `u`. `$liked` and `$own` are the SQL telling
/// whether the viewer liked and wrote it. A macro so it can be used in `concat!`.
macro_rules! comment_select {
    ($liked:literal, $own:literal) => {
        concat!(
            "SELECT c.id AS comment_id, c.body, c.sensitive, c.created_at AS commented_at, c.edited_at,
                    c.post_id, c.ap_id, c.parent_id, COALESCE(pu.username, pra.username) AS parent_author,
                    (SELECT COUNT(*) FROM comment_likes cl WHERE cl.comment_id = c.id)
                    + (SELECT COUNT(*) FROM remote_reactions rl WHERE rl.comment_id = c.id AND rl.kind = 'like')
                    AS like_count, ",
            $liked,
            " AS liked, ",
            $own,
            " AS is_own, ",
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
    pub sensitive: bool,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub author: Author,
    /// ActivityPub id, for replies received from other servers.
    pub ap_id: Option<String>,
    /// The comment it replies to, and that comment's author.
    pub parent_id: Option<i64>,
    pub parent_author: Option<String>,
    pub like_count: i64,
    /// Whether the viewer liked it, always `false` for visitors.
    pub liked: bool,
    /// Whether the viewer wrote it.
    pub is_own: bool,
    /// Media of comments received from other servers.
    pub attachments: Vec<Attachment>,
}

impl Comment {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("comment_id")?,
            body: row.try_get("body")?,
            sensitive: row.try_get("sensitive")?,
            created_at: row.try_get("commented_at")?,
            edited_at: row.try_get("edited_at")?,
            author: Author::from_row(row)?,
            ap_id: row.try_get("ap_id")?,
            parent_id: row.try_get("parent_id")?,
            parent_author: row.try_get("parent_author")?,
            like_count: row.try_get("like_count")?,
            liked: row.try_get("liked")?,
            is_own: row.try_get("is_own")?,
            attachments: Vec::new(),
        })
    }

    pub fn edited_at_iso(&self) -> Option<String> {
        self.edited_at.as_ref().map(post::time_iso)
    }

    /// The text as HTML, with clickable addresses, see `linkify`.
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

    /// The text as HTML, with clickable addresses, see `linkify`.
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
#[allow(clippy::too_many_arguments)]
pub async fn create_remote(
    pool: &PgPool,
    post_id: i64,
    remote_actor_id: i64,
    parent_id: Option<i64>,
    ap_id: &str,
    body: &str,
    sensitive: bool,
    published: Option<DateTime<Utc>>,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO comments (post_id, remote_actor_id, parent_id, ap_id, body, created_at, sensitive)
         VALUES ($1, $2, $3, $4, $5, COALESCE($6, NOW()), $7)
         ON CONFLICT (ap_id) DO NOTHING
         RETURNING id",
    )
    .bind(post_id)
    .bind(remote_actor_id)
    .bind(parent_id)
    .bind(ap_id)
    .bind(body)
    .bind(published)
    .bind(sensitive)
    .fetch_optional(pool)
    .await
}

/// Who may change a comment: its post, and its local author (`None` for a
/// comment of another server).
pub struct Owners {
    pub post_id: i64,
    pub author_id: Option<i32>,
}

pub async fn owners(pool: &PgPool, comment_id: i64) -> Result<Option<Owners>, sqlx::Error> {
    sqlx::query("SELECT post_id, user_id FROM comments WHERE id = $1")
        .bind(comment_id)
        .fetch_optional(pool)
        .await?
        .map(|row| {
            Ok(Owners {
                post_id: row.try_get("post_id")?,
                author_id: row.try_get("user_id")?,
            })
        })
        .transpose()
}

/// The text of a comment, to edit it.
pub async fn body_of(pool: &PgPool, comment_id: i64) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT body FROM comments WHERE id = $1")
        .bind(comment_id)
        .fetch_optional(pool)
        .await
}

/// Changes the text of the user's own comment. Returns `false` when it isn't theirs.
pub async fn update_own(pool: &PgPool, comment_id: i64, user_id: i32, body: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE comments SET body = $3, edited_at = NOW(), updated_at = NOW() WHERE id = $1 AND user_id = $2",
    )
    .bind(comment_id)
    .bind(user_id)
    .bind(body)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// Deletes a comment and the replies below it.
pub async fn delete(pool: &PgPool, comment_id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM comments WHERE id = $1")
        .bind(comment_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

/// Comments written here among a comment and the replies below it, at any
/// depth: they are deleted with it, other servers are told.
pub async fn local_thread(pool: &PgPool, comment_id: i64) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "WITH RECURSIVE thread AS (
            SELECT id, user_id FROM comments WHERE id = $1
            UNION
            SELECT c.id, c.user_id FROM comments c JOIN thread t ON c.parent_id = t.id
         )
         SELECT id FROM thread WHERE user_id IS NOT NULL",
    )
    .bind(comment_id)
    .fetch_all(pool)
    .await
}

/// Comments written here on a post: deleted with it, other servers are told.
pub async fn local_ids_of_post(pool: &PgPool, post_id: i64) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM comments WHERE post_id = $1 AND user_id IS NOT NULL")
        .bind(post_id)
        .fetch_all(pool)
        .await
}

async fn with_attachments(pool: &PgPool, mut comments: Vec<Comment>) -> Result<Vec<Comment>, sqlx::Error> {
    let remote: Vec<i64> = comments
        .iter()
        .filter(|comment| comment.author.is_remote())
        .map(|comment| comment.id)
        .collect();
    let mut attachments = media::for_comments(pool, &remote).await?;
    for comment in &mut comments {
        if let Some(found) = attachments.remove(&comment.id) {
            comment.attachments = found;
        }
    }
    Ok(comments)
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
        comment_select!("FALSE", "FALSE"),
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

/// A comment of any post, with its post id: for the federation of a comment
/// on a followers-only post, after checking the post with
/// `post::find_federable`.
pub async fn find_any(pool: &PgPool, comment_id: i64) -> Result<Option<(Comment, i64)>, sqlx::Error> {
    sqlx::query(concat!(comment_select!("FALSE", "FALSE"), " WHERE c.id = $1"))
        .bind(comment_id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(|row| Ok((Comment::from_row(row)?, row.try_get("post_id")?)))
        .transpose()
}

/// Comments of a post, oldest first like a conversation, as seen by `viewer_id`.
pub async fn for_post(pool: &PgPool, post_id: i64, viewer_id: Option<i32>) -> Result<Vec<Comment>, sqlx::Error> {
    let comments = sqlx::query(concat!(
        comment_select!(
            "EXISTS (SELECT 1 FROM comment_likes vl WHERE vl.comment_id = c.id AND vl.user_id = $2)",
            "COALESCE(c.user_id = $2, FALSE)"
        ),
        " WHERE c.post_id = $1 AND u.suspended_at IS NULL
         ORDER BY c.created_at, c.id"
    ))
    .bind(post_id)
    .bind(viewer_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(Comment::from_row)
    .collect::<Result<Vec<_>, _>>()?;
    with_attachments(pool, comments).await
}

/// The post of a comment, to check the user can see it before liking.
pub async fn post_of(pool: &PgPool, comment_id: i64) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT post_id FROM comments WHERE id = $1")
        .bind(comment_id)
        .fetch_optional(pool)
        .await
}

/// Adds the like, or removes it when the user already liked the comment.
/// The caller checks the user can see the comment's post.
pub async fn toggle_like(pool: &PgPool, comment_id: i64, user_id: i32) -> Result<post::Toggled, sqlx::Error> {
    let removed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "DELETE FROM comment_likes WHERE comment_id = $1 AND user_id = $2 RETURNING created_at",
    )
    .bind(comment_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    if let Some(reacted_at) = removed {
        return Ok(post::Toggled { active: false, reacted_at });
    }
    // DO UPDATE instead of DO NOTHING so a double click still returns the row.
    let reacted_at = sqlx::query_scalar(
        "INSERT INTO comment_likes (comment_id, user_id) VALUES ($1, $2)
         ON CONFLICT (comment_id, user_id) DO UPDATE SET created_at = comment_likes.created_at
         RETURNING created_at",
    )
    .bind(comment_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(post::Toggled { active: true, reacted_at })
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
