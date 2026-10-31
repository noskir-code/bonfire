//! Media of other Fediverse servers: pictures, videos and sounds attached to
//! their posts and comments, and the avatars of their accounts.
//!
//! Pages never load them from the other server: they go through
//! `/media/{hash}` (see `handlers::media`), which downloads them once into a
//! cache. Visitors' addresses stay hidden from the other servers, like with
//! Mastodon's media proxy. Only addresses saved here can be proxied, so the
//! proxy can't be used to download anything else.

use std::collections::HashMap;

use rsa::sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::entities::schema::{Column, Index, Table};

/// More are ignored: Mastodon allows 4, Misskey 16.
pub const MAX_ATTACHMENTS: usize = 16;
const DESCRIPTION_MAX_CHARS: usize = 1_500;

/// Remote addresses that may be proxied, by `hash`.
pub const MEDIA_TABLE: Table = Table {
    name: "remote_media",
    columns: &[
        // `hash(url)`, the address of the proxy: `/media/{hash}`.
        Column::new("hash", "TEXT"),
        Column::new("url", "TEXT"),
        // Extension of the cached file once downloaded, see `handlers::media`.
        Column::new("extension", "TEXT").nullable(),
        // Last failed download: not tried again for a while.
        Column::new("failed_at", "TIMESTAMPTZ").nullable(),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[Index::unique("remote_media_hash_idx", "hash")],
};

/// Files attached to posts and comments received from other servers.
pub const ATTACHMENTS_TABLE: Table = Table {
    name: "attachments",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // One of them is set.
        Column::new("post_id", "BIGINT").nullable().references("posts(id) ON DELETE CASCADE"),
        Column::new("comment_id", "BIGINT")
            .nullable()
            .references("comments(id) ON DELETE CASCADE"),
        Column::new("url", "TEXT"),
        Column::new("media_type", "TEXT").default("''"),
        // The alt text written by the author.
        Column::new("description", "TEXT").default("''"),
        Column::new("width", "INTEGER").nullable(),
        Column::new("height", "INTEGER").nullable(),
        Column::new("position", "INTEGER").default("0"),
    ],
    indexes: &[
        Index::new("attachments_post_idx", "post_id, position"),
        Index::new("attachments_comment_idx", "comment_id, position"),
    ],
};

/// The proxy's name of a remote address: hex SHA-256 of it.
pub fn hash(url: &str) -> String {
    Sha256::digest(url.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Where pages load `url` from.
pub fn proxy_url(url: &str) -> String {
    format!("/media/{}", hash(url))
}

/// Lets the proxy serve `url`.
pub async fn register(pool: &PgPool, url: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO remote_media (hash, url) VALUES ($1, $2) ON CONFLICT (hash) DO NOTHING")
        .bind(hash(url))
        .bind(url)
        .execute(pool)
        .await?;
    Ok(())
}

/// Registers the avatars of accounts saved before the proxy existed.
pub async fn register_known_avatars(pool: &PgPool) -> Result<(), sqlx::Error> {
    let urls: Vec<String> = sqlx::query_scalar(
        "SELECT a.icon_url FROM federation_actors a
         WHERE a.icon_url IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM remote_media m WHERE m.url = a.icon_url)",
    )
    .fetch_all(pool)
    .await?;
    for url in urls {
        register(pool, &url).await?;
    }
    Ok(())
}

/// A proxied address: the remote address, and the extension of its cached
/// file when it was downloaded.
pub struct Proxied {
    pub url: String,
    pub extension: Option<String>,
    /// Failed less than `RETRY_MINUTES` ago.
    pub recently_failed: bool,
}

const RETRY_MINUTES: i32 = 30;

pub async fn find(pool: &PgPool, hash: &str) -> Result<Option<Proxied>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT url, extension, COALESCE(failed_at > NOW() - make_interval(mins => $2), FALSE) AS recently_failed
         FROM remote_media WHERE hash = $1",
    )
    .bind(hash)
    .bind(RETRY_MINUTES)
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok(Proxied {
            url: row.try_get("url")?,
            extension: row.try_get("extension")?,
            recently_failed: row.try_get("recently_failed")?,
        })
    })
    .transpose()
}

pub async fn set_cached(pool: &PgPool, hash: &str, extension: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE remote_media SET extension = $2, failed_at = NULL WHERE hash = $1")
        .bind(hash)
        .bind(extension)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_failed(pool: &PgPool, hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE remote_media SET failed_at = NOW() WHERE hash = $1")
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    /// Shown as a link.
    Other,
}

/// A file attached to a post or a comment.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub url: String,
    pub media_type: String,
    pub description: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

impl Attachment {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            url: row.try_get("url")?,
            media_type: row.try_get("media_type")?,
            description: row.try_get("description")?,
            width: row.try_get("width")?,
            height: row.try_get("height")?,
        })
    }

    /// From the media type, else from the end of the address.
    pub fn kind(&self) -> MediaKind {
        let media_type = self.media_type.to_ascii_lowercase();
        let extension = self
            .url
            .split(['?', '#'])
            .next()
            .and_then(|path| path.rsplit_once('.'))
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();
        if media_type.starts_with("image/")
            || (media_type.is_empty() && matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif"))
        {
            MediaKind::Image
        } else if media_type.starts_with("video/") || (media_type.is_empty() && matches!(extension.as_str(), "mp4" | "webm")) {
            MediaKind::Video
        } else if media_type.starts_with("audio/")
            || (media_type.is_empty() && matches!(extension.as_str(), "mp3" | "ogg" | "opus" | "m4a" | "wav" | "flac"))
        {
            MediaKind::Audio
        } else {
            MediaKind::Other
        }
    }

    pub fn is_image(&self) -> bool {
        self.kind() == MediaKind::Image
    }

    pub fn is_video(&self) -> bool {
        self.kind() == MediaKind::Video
    }

    pub fn is_audio(&self) -> bool {
        self.kind() == MediaKind::Audio
    }

    /// Where the page loads it from, see the module.
    pub fn proxy_url(&self) -> String {
        proxy_url(&self.url)
    }
}

/// An attachment read from a note, before it's saved.
pub struct NewAttachment {
    pub url: String,
    pub media_type: String,
    pub description: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

impl NewAttachment {
    /// The alt text, cut to a reasonable length.
    pub fn new(url: String, media_type: String, description: &str, width: Option<i32>, height: Option<i32>) -> Self {
        Self {
            url,
            media_type,
            description: description.trim().chars().take(DESCRIPTION_MAX_CHARS).collect(),
            width,
            height,
        }
    }
}

/// What the attachments belong to.
#[derive(Clone, Copy, Debug)]
pub enum Owner {
    Post(i64),
    Comment(i64),
}

/// Replaces the attachments of a post or comment (an edit can change them).
pub async fn save_attachments(pool: &PgPool, owner: Owner, attachments: &[NewAttachment]) -> Result<(), sqlx::Error> {
    let (post_id, comment_id) = match owner {
        Owner::Post(id) => (Some(id), None),
        Owner::Comment(id) => (None, Some(id)),
    };
    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM attachments WHERE post_id = $1 OR comment_id = $2")
        .bind(post_id)
        .bind(comment_id)
        .execute(&mut *transaction)
        .await?;
    for (position, attachment) in attachments.iter().take(MAX_ATTACHMENTS).enumerate() {
        sqlx::query(
            "INSERT INTO attachments (post_id, comment_id, url, media_type, description, width, height, position)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(post_id)
        .bind(comment_id)
        .bind(&attachment.url)
        .bind(&attachment.media_type)
        .bind(&attachment.description)
        .bind(attachment.width)
        .bind(attachment.height)
        .bind(position as i32)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("INSERT INTO remote_media (hash, url) VALUES ($1, $2) ON CONFLICT (hash) DO NOTHING")
            .bind(hash(&attachment.url))
            .bind(&attachment.url)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await
}

/// Attachments of these posts, in order.
pub async fn for_posts(pool: &PgPool, post_ids: &[i64]) -> Result<HashMap<i64, Vec<Attachment>>, sqlx::Error> {
    if post_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT post_id AS owner_id, url, media_type, description, width, height FROM attachments
         WHERE post_id = ANY($1) ORDER BY position, id",
    )
    .bind(post_ids)
    .fetch_all(pool)
    .await?;
    group(&rows)
}

/// Attachments of these comments, in order.
pub async fn for_comments(pool: &PgPool, comment_ids: &[i64]) -> Result<HashMap<i64, Vec<Attachment>>, sqlx::Error> {
    if comment_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT comment_id AS owner_id, url, media_type, description, width, height FROM attachments
         WHERE comment_id = ANY($1) ORDER BY position, id",
    )
    .bind(comment_ids)
    .fetch_all(pool)
    .await?;
    group(&rows)
}

fn group(rows: &[PgRow]) -> Result<HashMap<i64, Vec<Attachment>>, sqlx::Error> {
    let mut grouped: HashMap<i64, Vec<Attachment>> = HashMap::new();
    for row in rows {
        grouped.entry(row.try_get("owner_id")?).or_default().push(Attachment::from_row(row)?);
    }
    Ok(grouped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_hex_sha256() {
        assert_eq!(hash(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn kind_from_type_or_extension() {
        let attachment = |url: &str, media_type: &str| Attachment {
            url: url.to_string(),
            media_type: media_type.to_string(),
            description: String::new(),
            width: None,
            height: None,
        };
        assert_eq!(attachment("https://x/a", "image/webp").kind(), MediaKind::Image);
        assert_eq!(attachment("https://x/a.MP4?x=1", "").kind(), MediaKind::Video);
        assert_eq!(attachment("https://x/a.ogg", "").kind(), MediaKind::Audio);
        assert_eq!(attachment("https://x/a.pdf", "application/pdf").kind(), MediaKind::Other);
    }
}
