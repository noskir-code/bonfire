use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    author::{Author, remote_author_columns},
    schema::{Column, Index, Table},
    user::user_columns,
};

pub const BODY_MAX_CHARS: usize = 10_000;
/// Posts shown at once on a page, newest first.
pub const PAGE_SIZE: i64 = 50;

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "posts",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // Posts are deleted with their author: a local user, or for a post
        // received from another server, its author there.
        Column::new("user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE CASCADE"),
        Column::new("remote_actor_id", "BIGINT")
            .nullable()
            .references("federation_actors(id) ON DELETE CASCADE"),
        // Its ActivityPub id and page, for posts of other servers.
        Column::new("ap_id", "TEXT").nullable(),
        Column::new("ap_url", "TEXT").nullable(),
        // Posted in a community, deleted with it.
        Column::new("community_id", "BIGINT")
            .nullable()
            .references("communities(id) ON DELETE CASCADE"),
        Column::new("body", "TEXT"),
        Column::new("visibility", "TEXT").default("'public'"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("updated_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        // Home timeline, and the posts of one user.
        Index::new("posts_created_at_idx", "created_at DESC"),
        Index::new("posts_user_id_created_at_idx", "user_id, created_at DESC"),
        Index::new("posts_community_created_at_idx", "community_id, created_at DESC"),
        Index::unique("posts_ap_id_idx", "ap_id"),
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

/// The community a post is in, as shown on the post.
#[derive(Clone, Debug)]
pub struct PostCommunity {
    pub id: i64,
    pub public_id: i64,
    pub slug: String,
    pub name: String,
    pub is_public: bool,
}

/// A post with its author and reactions, as seen by one viewer.
pub struct Post {
    pub id: i64,
    pub body: String,
    pub visibility: Visibility,
    pub created_at: DateTime<Utc>,
    pub author: Author,
    pub community: Option<PostCommunity>,
    /// ActivityPub id and page, for posts received from other servers.
    pub ap_id: Option<String>,
    pub ap_url: Option<String>,
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
            author: Author::from_row(row)?,
            community: match row.try_get::<Option<i64>, _>("community_id")? {
                Some(id) => Some(PostCommunity {
                    id,
                    public_id: row.try_get("community_public_id")?,
                    slug: row.try_get("community_slug")?,
                    name: row.try_get("community_name")?,
                    is_public: row.try_get::<String, _>("community_visibility")? == "public",
                }),
                None => None,
            },
            ap_id: row.try_get("ap_id")?,
            ap_url: row.try_get("ap_url")?,
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

    /// Only someone else's public posts can be shared, not those of a
    /// private community.
    pub fn can_share(&self) -> bool {
        !self.is_own
            && self.visibility == Visibility::Public
            && self.community.as_ref().is_none_or(|community| community.is_public)
    }

    pub fn created_at_iso(&self) -> String {
        time_iso(&self.created_at)
    }

    pub fn created_at_label(&self) -> String {
        time_label(&self.created_at)
    }
}

/// Columns read by `Post::from_row`. `$1` is always the viewer id (NULL for
/// visitors), `$shared_by` the SQL giving `shared_by`. Queries using it must
/// only show what the viewer can see, with `post_visible!()`.
macro_rules! post_select {
    ($shared_by:literal) => {
        concat!(
            "SELECT p.id AS post_id, p.body, p.visibility, p.created_at AS posted_at, p.ap_id, p.ap_url,
                    COALESCE(p.user_id = $1, FALSE) AS is_own,
                    (SELECT COUNT(*) FROM post_likes l WHERE l.post_id = p.id) AS like_count,
                    (SELECT COUNT(*) FROM comments pc WHERE pc.post_id = p.id) AS comment_count,
                    (SELECT COUNT(*) FROM post_shares s WHERE s.post_id = p.id) AS share_count,
                    EXISTS (SELECT 1 FROM post_likes l WHERE l.post_id = p.id AND l.user_id = $1) AS liked,
                    EXISTS (SELECT 1 FROM post_shares s WHERE s.post_id = p.id AND s.user_id = $1) AS shared,
                    p.community_id, cm.public_id AS community_public_id, cm.slug AS community_slug,
                    cm.name AS community_name, cm.visibility AS community_visibility, ",
            $shared_by,
            " AS shared_by, ",
            user_columns!("u."),
            ", ",
            remote_author_columns!(),
            " FROM posts p
              LEFT JOIN users u ON u.id = p.user_id
              LEFT JOIN federation_actors ra ON ra.id = p.remote_actor_id
              LEFT JOIN communities cm ON cm.id = p.community_id"
        )
    };
}

/// Whether the viewer `$1` can see the post `p` (its community joined as
/// `cm`): their own posts, public posts outside communities, and posts of
/// communities that aren't banned and are public or have the viewer as member.
macro_rules! post_visible {
    () => {
        "(COALESCE(p.user_id = $1, FALSE) OR (p.visibility = 'public' AND (p.community_id IS NULL OR (
            cm.status <> 'banned' AND (cm.visibility = 'public' OR EXISTS (
                SELECT 1 FROM community_members mm WHERE mm.community_id = cm.id AND mm.user_id = $1))))))"
    };
}
pub(crate) use post_visible;

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

/// `community_id` posts it in a community: the author must be a member.
pub async fn create(
    pool: &PgPool,
    user_id: i32,
    body: &str,
    visibility: Visibility,
    community_id: Option<i64>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO posts (user_id, body, visibility, community_id) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(user_id)
    .bind(body)
    .bind(visibility.as_str())
    .bind(community_id)
    .fetch_one(pool)
    .await
}

/// A post received from another server, in a community. Returns `None` when
/// a post with this ActivityPub id is already stored.
pub async fn create_remote(
    pool: &PgPool,
    remote_actor_id: i64,
    community_id: i64,
    ap_id: &str,
    ap_url: Option<&str>,
    body: &str,
    published: Option<DateTime<Utc>>,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO posts (remote_actor_id, community_id, ap_id, ap_url, body, created_at)
         VALUES ($1, $2, $3, $4, $5, COALESCE($6, NOW()))
         ON CONFLICT (ap_id) DO NOTHING
         RETURNING id",
    )
    .bind(remote_actor_id)
    .bind(community_id)
    .bind(ap_id)
    .bind(ap_url)
    .bind(body)
    .bind(published)
    .fetch_optional(pool)
    .await
}

/// A post or comment stored from another server, by its ActivityPub id.
pub enum RemoteObject {
    /// (post id, community id)
    Post(i64, Option<i64>),
    /// The comment's id.
    Comment(i64),
}

/// What `ap_id` is, when it was received from `remote_actor_id`.
pub async fn find_remote_object(pool: &PgPool, remote_actor_id: i64, ap_id: &str) -> Result<Option<RemoteObject>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT 'post' AS kind, id, community_id AS parent FROM posts WHERE ap_id = $1 AND remote_actor_id = $2
         UNION ALL
         SELECT 'comment', id, NULL FROM comments WHERE ap_id = $1 AND remote_actor_id = $2
         LIMIT 1",
    )
    .bind(ap_id)
    .bind(remote_actor_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        let id: i64 = row.try_get("id")?;
        let parent: Option<i64> = row.try_get("parent")?;
        Ok(match row.try_get::<String, _>("kind")?.as_str() {
            "post" => RemoteObject::Post(id, parent),
            _ => RemoteObject::Comment(id),
        })
    })
    .transpose()
}

/// The text of a post or comment changed on its server.
pub async fn update_remote_body(pool: &PgPool, object: &RemoteObject, body: &str) -> Result<(), sqlx::Error> {
    let (query, id) = match object {
        RemoteObject::Post(id, _) => ("UPDATE posts SET body = $1, updated_at = NOW() WHERE id = $2", *id),
        RemoteObject::Comment(id) => ("UPDATE comments SET body = $1, updated_at = NOW() WHERE id = $2", *id),
    };
    sqlx::query(query).bind(body).bind(id).execute(pool).await?;
    Ok(())
}

pub async fn delete_remote(pool: &PgPool, object: &RemoteObject) -> Result<(), sqlx::Error> {
    let (query, id) = match object {
        RemoteObject::Post(id, _) => ("DELETE FROM posts WHERE id = $1", *id),
        RemoteObject::Comment(id) => ("DELETE FROM comments WHERE id = $1", *id),
    };
    sqlx::query(query).bind(id).execute(pool).await?;
    Ok(())
}

/// Removes a post of a community, by its moderators.
pub async fn delete_in_community(pool: &PgPool, post_id: i64, community_id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM posts WHERE id = $1 AND community_id = $2")
        .bind(post_id)
        .bind(community_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

/// Local id of a post from its ActivityPub id: one of ours
/// (`/ap/posts/{id}`, see `API::Federation`), or one received.
pub async fn find_by_ap_id(pool: &PgPool, ap_id: &str) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM posts WHERE ap_id = $1")
        .bind(ap_id)
        .fetch_optional(pool)
        .await
}

fn to_posts(rows: Vec<PgRow>) -> Result<Vec<Post>, sqlx::Error> {
    rows.iter().map(Post::from_row).collect()
}

/// Public posts of everyone, and the viewer's own private ones. Posts of the
/// communities the viewer ignores are left out.
pub async fn timeline(pool: &PgPool, viewer_id: Option<i32>) -> Result<Vec<Post>, sqlx::Error> {
    let rows = sqlx::query(concat!(
        post_select!("NULL::text"),
        " WHERE ",
        post_visible!(),
        " AND NOT EXISTS (SELECT 1 FROM community_ignores ci
                          WHERE ci.community_id = p.community_id AND ci.user_id = $1)
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
           AND ",
        post_visible!(),
        "
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

/// Order of a community's posts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    New,
    /// Most liked first.
    Top,
    /// Likes and comments, cooling down with age: what's active now.
    Hot,
}

impl Sort {
    pub const ALL: [Self; 3] = [Self::Hot, Self::New, Self::Top];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Top => "top",
            Self::Hot => "hot",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::New => "New",
            Self::Top => "Top",
            Self::Hot => "Hot",
        }
    }

    /// Hot when missing or unknown.
    pub fn parse(value: Option<&str>) -> Self {
        Self::ALL
            .into_iter()
            .find(|sort| Some(sort.as_str()) == value)
            .unwrap_or(Self::Hot)
    }
}

/// Posts of a community the viewer can see, see `post_visible!`.
pub async fn for_community(
    pool: &PgPool,
    community_id: i64,
    viewer_id: Option<i32>,
    sort: Sort,
) -> Result<Vec<Post>, sqlx::Error> {
    // Three queries rather than a dynamic ORDER BY: sqlx wants SQL known at
    // compile time.
    macro_rules! ordered {
        ($order:literal) => {
            concat!(
                post_select!("NULL::text"),
                " WHERE p.community_id = $2 AND ",
                post_visible!(),
                " ORDER BY ",
                $order,
                " LIMIT $3"
            )
        };
    }
    let query = match sort {
        Sort::New => ordered!("p.created_at DESC, p.id DESC"),
        Sort::Top => ordered!("like_count DESC, p.created_at DESC, p.id DESC"),
        // Hacker News' formula: points over (age in hours + 2) ^ 1.5. The
        // counts are written out: Postgres doesn't accept column aliases
        // inside an ORDER BY expression.
        Sort::Hot => ordered!(
            "((SELECT COUNT(*) FROM post_likes hl WHERE hl.post_id = p.id)
              + (SELECT COUNT(*) FROM comments hc WHERE hc.post_id = p.id) + 1)
              / POWER(EXTRACT(EPOCH FROM (NOW() - p.created_at)) / 3600 + 2, 1.5) DESC, p.id DESC"
        ),
    };
    let rows = sqlx::query(query)
        .bind(viewer_id)
        .bind(community_id)
        .bind(PAGE_SIZE)
        .fetch_all(pool)
        .await?;
    to_posts(rows)
}

/// One post, `None` if it doesn't exist or the viewer can't see it.
pub async fn find(pool: &PgPool, post_id: i64, viewer_id: Option<i32>) -> Result<Option<Post>, sqlx::Error> {
    sqlx::query(concat!(
        post_select!("NULL::text"),
        " WHERE p.id = $2 AND ",
        post_visible!()
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

/// A reaction after `toggle`.
#[derive(Clone, Copy, Debug)]
pub struct Toggled {
    /// `false` when it was removed.
    pub active: bool,
    /// When the reaction was made, also for a removed one.
    pub reacted_at: DateTime<Utc>,
}

/// Adds the reaction, or removes it when the user already reacted.
/// Returns `None` when the post doesn't exist or can't be reacted to:
/// hidden from the user, or a share of their own or a private post.
pub async fn toggle(
    pool: &PgPool,
    reaction: Reaction,
    post_id: i64,
    user_id: i32,
) -> Result<Option<Toggled>, sqlx::Error> {
    let Some(post) = find(pool, post_id, Some(user_id)).await? else {
        return Ok(None);
    };
    if matches!(reaction, Reaction::Share) && !post.can_share() {
        return Ok(None);
    }

    let (delete, insert) = match reaction {
        Reaction::Like => (
            "DELETE FROM post_likes WHERE post_id = $1 AND user_id = $2 RETURNING created_at",
            "INSERT INTO post_likes (post_id, user_id) VALUES ($1, $2)
             ON CONFLICT (post_id, user_id) DO UPDATE SET created_at = post_likes.created_at
             RETURNING created_at",
        ),
        Reaction::Share => (
            "DELETE FROM post_shares WHERE post_id = $1 AND user_id = $2 RETURNING created_at",
            "INSERT INTO post_shares (post_id, user_id) VALUES ($1, $2)
             ON CONFLICT (post_id, user_id) DO UPDATE SET created_at = post_shares.created_at
             RETURNING created_at",
        ),
    };

    let removed: Option<DateTime<Utc>> = sqlx::query_scalar(delete)
        .bind(post_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    if let Some(reacted_at) = removed {
        return Ok(Some(Toggled { active: false, reacted_at }));
    }
    // DO UPDATE instead of DO NOTHING so a double click still returns the row.
    let reacted_at = sqlx::query_scalar(insert).bind(post_id).bind(user_id).fetch_one(pool).await?;
    Ok(Some(Toggled { active: true, reacted_at }))
}
