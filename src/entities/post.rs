use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    schema::{Column, Index, Table},
    user::{User, user_columns},
};

pub const BODY_MAX_CHARS: usize = 10_000;
/// Posts shown at once on a page, newest first.
pub const PAGE_SIZE: i64 = 50;

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "posts",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // Posts are deleted with their author.
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("body", "TEXT"),
        Column::new("visibility", "TEXT").default("'public'"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("updated_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        // Home timeline, and the posts of one user.
        Index::new("posts_created_at_idx", "created_at DESC"),
        Index::new("posts_user_id_created_at_idx", "user_id, created_at DESC"),
    ],
};

/// One row per user who liked a post.
pub const LIKES_TABLE: Table = Table {
    name: "post_likes",
    columns: &[
        Column::new("post_id", "BIGINT").references("posts(id) ON DELETE CASCADE"),
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[Index::unique("post_likes_post_user_idx", "post_id, user_id")],
};

/// One row per user who shared (reposted) a post on their profile.
pub const SHARES_TABLE: Table = Table {
    name: "post_shares",
    columns: &[
        Column::new("post_id", "BIGINT").references("posts(id) ON DELETE CASCADE"),
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("post_shares_post_user_idx", "post_id, user_id"),
        // Shares shown on a profile.
        Index::new("post_shares_user_created_at_idx", "user_id, created_at DESC"),
    ],
};

/// Who can see a post. "Friends" and "Following" will come with relationships.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    /// Only the author.
    Private,
}

impl Visibility {
    pub const ALL: [Self; 2] = [Self::Public, Self::Private];

    /// Stored in the database and sent by the form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "Public",
            Self::Private => "Only me",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|visibility| visibility.as_str() == value)
    }
}

/// For the `datetime` attribute of `<time>`, read by `assets/system.js`.
pub fn time_iso(time: &DateTime<Utc>) -> String {
    time.to_rfc3339()
}

/// Shown when JavaScript is off, `system.js` replaces it with "x minutes ago".
pub fn time_label(time: &DateTime<Utc>) -> String {
    time.format("%b %-d, %Y · %H:%M UTC").to_string()
}

/// A post with its author and reactions, as seen by one viewer.
pub struct Post {
    pub id: i64,
    pub body: String,
    pub visibility: Visibility,
    pub created_at: DateTime<Utc>,
    pub author: User,
    pub like_count: i64,
    pub comment_count: i64,
    pub share_count: i64,
    /// Whether the viewer liked / shared it, always `false` for visitors.
    pub liked: bool,
    pub shared: bool,
    /// Whether the viewer wrote it.
    pub is_own: bool,
    /// Set when the post is on a profile because that user shared it.
    pub shared_by: Option<String>,
}

impl Post {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        let visibility: String = row.try_get("visibility")?;
        Ok(Self {
            id: row.try_get("post_id")?,
            body: row.try_get("body")?,
            // An unknown value is treated as the most restrictive one.
            visibility: Visibility::parse(&visibility).unwrap_or(Visibility::Private),
            created_at: row.try_get("posted_at")?,
            author: User::from_row(row)?,
            like_count: row.try_get("like_count")?,
            comment_count: row.try_get("comment_count")?,
            share_count: row.try_get("share_count")?,
            liked: row.try_get("liked")?,
            shared: row.try_get("shared")?,
            is_own: row.try_get("is_own")?,
            shared_by: row.try_get("shared_by")?,
        })
    }

    pub fn is_private(&self) -> bool {
        self.visibility == Visibility::Private
    }

    /// Only someone else's public posts can be shared.
    pub fn can_share(&self) -> bool {
        !self.is_own && self.visibility == Visibility::Public
    }

    pub fn created_at_iso(&self) -> String {
        time_iso(&self.created_at)
    }

    pub fn created_at_label(&self) -> String {
        time_label(&self.created_at)
    }
}

/// Columns read by `Post::from_row`. `$1` is always the viewer id (NULL for
/// visitors), `$shared_by` the SQL giving `shared_by`. Queries using it only
/// show what the viewer can see: `(p.visibility = 'public' OR p.user_id = $1)`.
macro_rules! post_select {
    ($shared_by:literal) => {
        concat!(
            "SELECT p.id AS post_id, p.body, p.visibility, p.created_at AS posted_at,
                    COALESCE(p.user_id = $1, FALSE) AS is_own,
                    (SELECT COUNT(*) FROM post_likes l WHERE l.post_id = p.id) AS like_count,
                    (SELECT COUNT(*) FROM comments c WHERE c.post_id = p.id) AS comment_count,
                    (SELECT COUNT(*) FROM post_shares s WHERE s.post_id = p.id) AS share_count,
                    EXISTS (SELECT 1 FROM post_likes l WHERE l.post_id = p.id AND l.user_id = $1) AS liked,
                    EXISTS (SELECT 1 FROM post_shares s WHERE s.post_id = p.id AND s.user_id = $1) AS shared, ",
            $shared_by,
            " AS shared_by, ",
            user_columns!("u."),
            " FROM posts p JOIN users u ON u.id = p.user_id"
        )
    };
}

/// Trims the text and checks it's not empty, at most `max_chars`, and has no
/// control characters other than line breaks and tabs.
pub fn validate_text(raw: &str, max_chars: usize) -> Result<String, BodyError> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(BodyError::Empty);
    }
    if text.chars().count() > max_chars {
        return Err(BodyError::TooLong);
    }
    if text
        .chars()
        .any(|letter| letter.is_control() && !matches!(letter, '\n' | '\r' | '\t'))
    {
        return Err(BodyError::InvalidCharacters);
    }
    // Browsers send line breaks as \r\n.
    Ok(text.replace("\r\n", "\n"))
}

pub fn validate_body(raw: &str) -> Result<String, BodyError> {
    validate_text(raw, BODY_MAX_CHARS)
}

#[derive(Clone, Copy, Debug)]
pub enum BodyError {
    Empty,
    TooLong,
    InvalidCharacters,
}

pub async fn create(
    pool: &PgPool,
    user_id: i32,
    body: &str,
    visibility: Visibility,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO posts (user_id, body, visibility) VALUES ($1, $2, $3) RETURNING id")
        .bind(user_id)
        .bind(body)
        .bind(visibility.as_str())
        .fetch_one(pool)
        .await
}

fn to_posts(rows: Vec<PgRow>) -> Result<Vec<Post>, sqlx::Error> {
    rows.iter().map(Post::from_row).collect()
}

/// Public posts of everyone, and the viewer's own private ones.
pub async fn timeline(pool: &PgPool, viewer_id: Option<i32>) -> Result<Vec<Post>, sqlx::Error> {
    let rows = sqlx::query(concat!(
        post_select!("NULL::text"),
        " WHERE (p.visibility = 'public' OR p.user_id = $1)
         ORDER BY p.created_at DESC, p.id DESC
         LIMIT $2"
    ))
    .bind(viewer_id)
    .bind(PAGE_SIZE)
    .fetch_all(pool)
    .await?;
    to_posts(rows)
}

/// What a profile shows: the posts the user wrote and the ones they shared,
/// newest first (by share date for shares), limited to what the viewer can see.
pub async fn by_author(
    pool: &PgPool,
    author_id: i32,
    viewer_id: Option<i32>,
) -> Result<Vec<Post>, sqlx::Error> {
    let rows = sqlx::query(concat!(
        post_select!("CASE WHEN p.user_id <> $2 THEN sharer.username END"),
        " LEFT JOIN post_shares ps ON ps.post_id = p.id AND ps.user_id = $2
          LEFT JOIN users sharer ON sharer.id = ps.user_id
         WHERE (p.user_id = $2 OR ps.user_id IS NOT NULL)
           AND (p.visibility = 'public' OR p.user_id = $1)
         ORDER BY COALESCE(ps.created_at, p.created_at) DESC, p.id DESC
         LIMIT $3"
    ))
    .bind(viewer_id)
    .bind(author_id)
    .bind(PAGE_SIZE)
    .fetch_all(pool)
    .await?;
    to_posts(rows)
}

/// One post, `None` if it doesn't exist or the viewer can't see it.
pub async fn find(pool: &PgPool, post_id: i64, viewer_id: Option<i32>) -> Result<Option<Post>, sqlx::Error> {
    sqlx::query(concat!(
        post_select!("NULL::text"),
        " WHERE p.id = $2 AND (p.visibility = 'public' OR p.user_id = $1)"
    ))
    .bind(viewer_id)
    .bind(post_id)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(Post::from_row)
    .transpose()
}

#[derive(Clone, Copy, Debug)]
pub enum Reaction {
    Like,
    Share,
}

/// Adds the reaction, or removes it when the user already reacted.
/// Returns `None` when the post doesn't exist or can't be reacted to:
/// hidden from the user, or a share of their own or a private post.
pub async fn toggle(
    pool: &PgPool,
    reaction: Reaction,
    post_id: i64,
    user_id: i32,
) -> Result<Option<bool>, sqlx::Error> {
    let Some(post) = find(pool, post_id, Some(user_id)).await? else {
        return Ok(None);
    };
    if matches!(reaction, Reaction::Share) && !post.can_share() {
        return Ok(None);
    }

    let (delete, insert) = match reaction {
        Reaction::Like => (
            "DELETE FROM post_likes WHERE post_id = $1 AND user_id = $2",
            "INSERT INTO post_likes (post_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        ),
        Reaction::Share => (
            "DELETE FROM post_shares WHERE post_id = $1 AND user_id = $2",
            "INSERT INTO post_shares (post_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        ),
    };

    let removed = sqlx::query(delete)
        .bind(post_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0;
    if removed {
        return Ok(Some(false));
    }
    sqlx::query(insert).bind(post_id).bind(user_id).execute(pool).await?;
    Ok(Some(true))
}
