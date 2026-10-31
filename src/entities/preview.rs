//! Previews of links (title, description, image of the page), shown as a
//! card under the posts linking to it. Fetched in the background by
//! `crate::preview`, once per address, shared by every post linking to it.

use std::collections::HashMap;

use sqlx::{PgPool, Row, postgres::PgRow};

use crate::entities::{
    media,
    schema::{Column, Index, Table},
};

/// A page is fetched again after this, for a changed title or image.
pub const KEEP_DAYS: i32 = 7;
/// A page that gave no preview is tried again after this.
pub const RETRY_HOURS: i32 = 24;

pub const TABLE: Table = Table {
    name: "link_previews",
    columns: &[
        Column::new("url", "TEXT"),
        Column::new("title", "TEXT").default("''"),
        Column::new("description", "TEXT").default("''"),
        Column::new("site_name", "TEXT").default("''"),
        Column::new("image_url", "TEXT").nullable(),
        // No preview: not a web page, no title, unreachable...
        Column::new("failed", "BOOLEAN").default("FALSE"),
        Column::new("fetched_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[Index::unique("link_previews_url_idx", "url")],
};

/// What a card shows.
#[derive(Clone, Debug)]
pub struct LinkPreview {
    pub url: String,
    pub title: String,
    pub description: String,
    pub site_name: String,
    pub image_url: Option<String>,
}

impl LinkPreview {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            url: row.try_get("url")?,
            title: row.try_get("title")?,
            description: row.try_get("description")?,
            site_name: row.try_get("site_name")?,
            image_url: row.try_get("image_url")?,
        })
    }

    /// The image through the media proxy.
    pub fn image_proxy_url(&self) -> Option<String> {
        self.image_url.as_deref().map(media::proxy_url)
    }

    /// The site's name, else its domain.
    pub fn site(&self) -> String {
        if !self.site_name.trim().is_empty() {
            return self.site_name.clone();
        }
        crate::entities::author::host_of(&self.url)
            .trim_start_matches("www.")
            .to_string()
    }
}

/// The usable previews of these addresses.
pub async fn find_many(pool: &PgPool, urls: &[String]) -> Result<HashMap<String, LinkPreview>, sqlx::Error> {
    if urls.is_empty() {
        return Ok(HashMap::new());
    }
    sqlx::query("SELECT url, title, description, site_name, image_url FROM link_previews WHERE url = ANY($1) AND NOT failed")
        .bind(urls)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|row| LinkPreview::from_row(row).map(|preview| (preview.url.clone(), preview)))
        .collect()
}

/// Whether the address was fetched recently enough (or failed recently).
pub async fn is_fresh(pool: &PgPool, url: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM link_previews WHERE url = $1 AND fetched_at > NOW() - CASE
            WHEN failed THEN make_interval(hours => $3) ELSE make_interval(days => $2) END)",
    )
    .bind(url)
    .bind(KEEP_DAYS)
    .bind(RETRY_HOURS)
    .fetch_one(pool)
    .await
}

/// Saves what was found, `None` when the page gave no preview.
pub async fn save(pool: &PgPool, url: &str, found: Option<&LinkPreview>) -> Result<(), sqlx::Error> {
    if let Some(image) = found.and_then(|preview| preview.image_url.as_deref()) {
        media::register(pool, image).await?;
    }
    sqlx::query(
        "INSERT INTO link_previews (url, title, description, site_name, image_url, failed, fetched_at)
         VALUES ($1, $2, $3, $4, $5, $6, NOW())
         ON CONFLICT (url) DO UPDATE SET title = EXCLUDED.title, description = EXCLUDED.description,
            site_name = EXCLUDED.site_name, image_url = EXCLUDED.image_url,
            failed = EXCLUDED.failed, fetched_at = NOW()",
    )
    .bind(url)
    .bind(found.map(|preview| preview.title.as_str()).unwrap_or_default())
    .bind(found.map(|preview| preview.description.as_str()).unwrap_or_default())
    .bind(found.map(|preview| preview.site_name.as_str()).unwrap_or_default())
    .bind(found.and_then(|preview| preview.image_url.as_deref()))
    .bind(found.is_none())
    .execute(pool)
    .await?;
    Ok(())
}

/// Posts not looked at yet (new or edited), newest first: (id, text).
/// Older ones are left: their links may not exist anymore.
pub async fn posts_to_check(pool: &PgPool, limit: i64) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query(
        "SELECT id, body FROM posts WHERE NOT link_checked AND created_at > NOW() - make_interval(days => $2)
         ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .bind(KEEP_DAYS)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|row| Ok((row.try_get("id")?, row.try_get("body")?)))
    .collect()
}

pub async fn mark_checked(pool: &PgPool, post_ids: &[i64]) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE posts SET link_checked = TRUE WHERE id = ANY($1)")
        .bind(post_ids)
        .execute(pool)
        .await?;
    Ok(())
}
