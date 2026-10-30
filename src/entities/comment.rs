use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    post::{self, BodyError},
    schema::{Column, Index, Table},
    user::{User, user_columns},
};

pub const BODY_MAX_CHARS: usize = 2_000;

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "comments",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // Comments are deleted with their post or their author.
        Column::new("post_id", "BIGINT").references("posts(id) ON DELETE CASCADE"),
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("body", "TEXT"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("updated_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[Index::new("comments_post_created_at_idx", "post_id, created_at")],
};

pub struct Comment {
    pub id: i64,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub author: User,
}

impl Comment {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("comment_id")?,
            body: row.try_get("body")?,
            created_at: row.try_get("commented_at")?,
            author: User::from_row(row)?,
        })
    }

    pub fn created_at_iso(&self) -> String {
        post::time_iso(&self.created_at)
    }

    pub fn created_at_label(&self) -> String {
        post::time_label(&self.created_at)
    }
}

pub fn validate_body(raw: &str) -> Result<String, BodyError> {
    post::validate_text(raw, BODY_MAX_CHARS)
}

/// The post must exist and be visible to the author, see `post::find`.
pub async fn create(pool: &PgPool, post_id: i64, user_id: i32, body: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO comments (post_id, user_id, body) VALUES ($1, $2, $3) RETURNING id")
        .bind(post_id)
        .bind(user_id)
        .bind(body)
        .fetch_one(pool)
        .await
}

/// Comments of a post, oldest first like a conversation.
pub async fn for_post(pool: &PgPool, post_id: i64) -> Result<Vec<Comment>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT c.id AS comment_id, c.body, c.created_at AS commented_at, ",
        user_columns!("u."),
        " FROM comments c JOIN users u ON u.id = c.user_id
         WHERE c.post_id = $1
         ORDER BY c.created_at, c.id"
    ))
    .bind(post_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(Comment::from_row)
    .collect()
}
