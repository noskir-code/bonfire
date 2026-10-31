//! Reports: users (here, or on another server through a Fediverse `Flag`)
//! tell the site's staff about a post, a comment or an account breaking the
//! rules. The staff handle them on the moderation dashboard (see
//! `handlers::moderation`).
//!
//! A report keeps a copy of what was reported: the post may be deleted, by
//! its author or by the staff, and the report must still be readable.

use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    author::{Author, RemoteAuthor, host_of, remote_author_columns},
    post,
    schema::{Column, Index, Table},
    user::{User, user_columns},
};

pub const COMMENT_MAX_CHARS: usize = 1_000;
/// Reports shown at once on the dashboard.
pub const PAGE_SIZE: i64 = 50;
const SNAPSHOT_CHARS: usize = 500;

pub const TABLE: Table = Table {
    name: "reports",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // Who reported: a local user, or a server of the Fediverse (its
        // account sending the Flag). The report stays if they go.
        Column::new("reporter_user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE SET NULL"),
        Column::new("reporter_actor_id", "BIGINT")
            .nullable()
            .references("federation_actors(id) ON DELETE SET NULL"),
        // What is reported: a post or a comment (then also its author), or
        // only an account.
        Column::new("post_id", "BIGINT").nullable().references("posts(id) ON DELETE SET NULL"),
        Column::new("comment_id", "BIGINT")
            .nullable()
            .references("comments(id) ON DELETE SET NULL"),
        Column::new("target_user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE SET NULL"),
        Column::new("target_actor_id", "BIGINT")
            .nullable()
            .references("federation_actors(id) ON DELETE SET NULL"),
        // `post`, `comment` or `account`: what was reported, also once it's
        // deleted.
        Column::new("kind", "TEXT").default("'account'"),
        // Copies at report time, see the module.
        Column::new("snapshot", "TEXT").default("''"),
        Column::new("target_label", "TEXT").default("''"),
        Column::new("reason", "TEXT"),
        Column::new("comment", "TEXT").default("''"),
        // `open`, `resolved` or `dismissed`.
        Column::new("status", "TEXT").default("'open'"),
        Column::new("handled_by", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE SET NULL"),
        Column::new("handled_at", "TIMESTAMPTZ").nullable(),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::new("reports_status_created_idx", "status, created_at DESC"),
        Index::new("reports_target_user_idx", "target_user_id, status"),
        Index::new("reports_target_actor_idx", "target_actor_id, status"),
        Index::new("reports_reporter_idx", "reporter_user_id, status"),
    ],
};

/// Why something is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Spam,
    Harassment,
    Hate,
    Illegal,
    Sensitive,
    Other,
}

impl Reason {
    pub const ALL: [Self; 6] = [Self::Spam, Self::Harassment, Self::Hate, Self::Illegal, Self::Sensitive, Self::Other];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spam => "spam",
            Self::Harassment => "harassment",
            Self::Hate => "hate",
            Self::Illegal => "illegal",
            Self::Sensitive => "sensitive",
            Self::Other => "other",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Spam => "Spam or scam",
            Self::Harassment => "Harassment or bullying",
            Self::Hate => "Hate speech",
            Self::Illegal => "Illegal content",
            Self::Sensitive => "Sensitive content not marked",
            Self::Other => "Something else",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Open,
    Resolved,
    Dismissed,
}

impl Status {
    pub const ALL: [Self; 3] = [Self::Open, Self::Resolved, Self::Dismissed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Dismissed => "dismissed",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Resolved => "Resolved",
            Self::Dismissed => "Dismissed",
        }
    }

    /// Open when missing or unknown.
    pub fn parse(value: Option<&str>) -> Self {
        Self::ALL
            .into_iter()
            .find(|status| Some(status.as_str()) == value)
            .unwrap_or(Self::Open)
    }
}

/// What a report is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Post(i64),
    Comment(i64),
    User(i32),
    /// A row of `federation_actors`.
    Remote(i64),
}

/// A new report: what it's about, copied as it is now.
pub struct NewReport {
    pub target: Target,
    /// The author: always set for posts and comments.
    pub account: Option<Target>,
    pub snapshot: String,
    pub target_label: String,
    pub reason: Reason,
    pub comment: String,
}

/// Cuts a copy of the reported text.
pub fn snapshot(text: &str) -> String {
    match text.char_indices().nth(SNAPSHOT_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

impl Target {
    fn kind(self) -> &'static str {
        match self {
            Self::Post(_) => "post",
            Self::Comment(_) => "comment",
            Self::User(_) | Self::Remote(_) => "account",
        }
    }
}

fn ids(target: Target, account: Option<Target>) -> (Option<i64>, Option<i64>, Option<i32>, Option<i64>) {
    let (mut post_id, mut comment_id, mut user_id, mut actor_id) = (None, None, None, None);
    for part in std::iter::once(target).chain(account) {
        match part {
            Target::Post(id) => post_id = Some(id),
            Target::Comment(id) => comment_id = Some(id),
            Target::User(id) => user_id = Some(id),
            Target::Remote(id) => actor_id = Some(id),
        }
    }
    (post_id, comment_id, user_id, actor_id)
}

/// Saves a report by a local user. Reporting the same thing again while the
/// first report is open updates it instead.
pub async fn create(pool: &PgPool, reporter_id: i32, report: &NewReport) -> Result<i64, sqlx::Error> {
    let (post_id, comment_id, user_id, actor_id) = ids(report.target, report.account);
    let updated: Option<i64> = sqlx::query_scalar(
        "UPDATE reports SET reason = $6, comment = $7, created_at = NOW()
         WHERE reporter_user_id = $1 AND status = 'open'
           AND post_id IS NOT DISTINCT FROM $2 AND comment_id IS NOT DISTINCT FROM $3
           AND target_user_id IS NOT DISTINCT FROM $4 AND target_actor_id IS NOT DISTINCT FROM $5
         RETURNING id",
    )
    .bind(reporter_id)
    .bind(post_id)
    .bind(comment_id)
    .bind(user_id)
    .bind(actor_id)
    .bind(report.reason.as_str())
    .bind(&report.comment)
    .fetch_optional(pool)
    .await?;
    if let Some(id) = updated {
        return Ok(id);
    }
    insert(pool, Some(reporter_id), None, report).await
}

/// Saves a report sent by another server (a Fediverse `Flag`).
pub async fn create_remote(pool: &PgPool, reporter_actor_id: i64, report: &NewReport) -> Result<i64, sqlx::Error> {
    insert(pool, None, Some(reporter_actor_id), report).await
}

async fn insert(pool: &PgPool, reporter_user_id: Option<i32>, reporter_actor_id: Option<i64>, report: &NewReport) -> Result<i64, sqlx::Error> {
    let (post_id, comment_id, user_id, actor_id) = ids(report.target, report.account);
    sqlx::query_scalar(
        "INSERT INTO reports (reporter_user_id, reporter_actor_id, post_id, comment_id, target_user_id,
                              target_actor_id, snapshot, target_label, reason, comment, kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
    )
    .bind(reporter_user_id)
    .bind(reporter_actor_id)
    .bind(post_id)
    .bind(comment_id)
    .bind(user_id)
    .bind(actor_id)
    .bind(&report.snapshot)
    .bind(&report.target_label)
    .bind(report.reason.as_str())
    .bind(&report.comment)
    .bind(report.target.kind())
    .fetch_one(pool)
    .await
}

/// Open reports, for the count next to "Moderation".
pub async fn open_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM reports WHERE status = 'open'")
        .fetch_one(pool)
        .await
}

/// Who reported: a local user, or a server of the Fediverse.
pub enum Reporter {
    /// The username.
    User(String),
    /// The domain of the server, and its account sending the report.
    Server(String),
    /// Account gone since.
    Gone,
}

/// The account a report is about, with what the staff did to it.
pub struct Account {
    pub author: Author,
    /// Suspended (local) or blocked (remote).
    pub restricted: bool,
    /// Its open reports, this one included.
    pub open_reports: i64,
}

/// A report as shown on the dashboard.
pub struct Report {
    pub id: i64,
    pub reason: Reason,
    pub comment: String,
    pub status: Status,
    pub created_at: DateTime<Utc>,
    pub handled_at: Option<DateTime<Utc>>,
    pub handled_by: Option<String>,
    pub reporter: Reporter,
    pub post_id: Option<i64>,
    pub comment_id: Option<i64>,
    /// The post of a reported comment, to link to it.
    pub comment_post_id: Option<i64>,
    /// The text now, `None` when it was deleted: then `snapshot` shows.
    pub current_text: Option<String>,
    pub snapshot: String,
    pub target_label: String,
    pub account: Option<Account>,
    /// What was reported: "post", "comment" or "account".
    pub kind: &'static str,
}

impl Report {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        let kind = match row.try_get::<String, _>("kind")?.as_str() {
            "post" => "post",
            "comment" => "comment",
            _ => "account",
        };
        let reporter = if let Some(username) = row.try_get::<Option<String>, _>("reporter_username")? {
            Reporter::User(username)
        } else if let Some(actor) = row.try_get::<Option<String>, _>("reporter_actor")? {
            Reporter::Server(host_of(&actor))
        } else {
            Reporter::Gone
        };
        let account = if row.try_get::<Option<i32>, _>("id")?.is_some() || row.try_get::<Option<i64>, _>("ra_id")?.is_some() {
            let author = if row.try_get::<Option<i32>, _>("id")?.is_some() {
                Author::Local(User::from_row(row)?)
            } else {
                Author::Remote(RemoteAuthor::from_row(row)?)
            };
            Some(Account {
                restricted: row.try_get("restricted")?,
                open_reports: row.try_get("account_open_reports")?,
                author,
            })
        } else {
            None
        };
        Ok(Self {
            id: row.try_get("report_id")?,
            reason: Reason::parse(&row.try_get::<String, _>("reason")?).unwrap_or(Reason::Other),
            comment: row.try_get("report_comment")?,
            status: Status::parse(Some(&row.try_get::<String, _>("status")?)),
            created_at: row.try_get("reported_at")?,
            handled_at: row.try_get("handled_at")?,
            handled_by: row.try_get("handler_username")?,
            reporter,
            post_id: row.try_get("post_id")?,
            comment_id: row.try_get("comment_id")?,
            comment_post_id: row.try_get("comment_post_id")?,
            current_text: row.try_get("current_text")?,
            snapshot: row.try_get("snapshot")?,
            target_label: row.try_get("target_label")?,
            account,
            kind,
        })
    }

    pub fn created_at_iso(&self) -> String {
        post::time_iso(&self.created_at)
    }

    pub fn created_at_label(&self) -> String {
        post::time_label(&self.created_at)
    }

    pub fn handled_at_iso(&self) -> Option<String> {
        self.handled_at.as_ref().map(post::time_iso)
    }

    pub fn handled_at_label(&self) -> Option<String> {
        self.handled_at.as_ref().map(post::time_label)
    }

    /// The reported text: as it is now, else the copy.
    pub fn text(&self) -> &str {
        self.current_text.as_deref().unwrap_or(&self.snapshot)
    }

    /// The post or comment was reported but doesn't exist anymore.
    pub fn content_deleted(&self) -> bool {
        self.kind != "account" && self.current_text.is_none()
    }

    /// Where to see what was reported.
    pub fn link(&self) -> Option<String> {
        match (self.post_id, self.comment_id, self.comment_post_id) {
            (_, Some(comment_id), Some(post_id)) => Some(format!("/posts/{post_id}#comment-{comment_id}")),
            (Some(post_id), None, _) => Some(format!("/posts/{post_id}")),
            _ => self.account.as_ref().map(|account| account.author.profile_url()),
        }
    }
}

/// Reports of a status, newest first, older than `before` when given.
pub async fn list(pool: &PgPool, status: Status, before: Option<i64>) -> Result<Vec<Report>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT r.id AS report_id, r.reason, r.comment AS report_comment, r.status, r.created_at AS reported_at,
                r.handled_at, handler.username AS handler_username,
                reporter.username AS reporter_username, ra_reporter.actor_id AS reporter_actor,
                r.post_id, r.comment_id, r.kind, r.snapshot, r.target_label,
                c.post_id AS comment_post_id,
                CASE r.kind WHEN 'comment' THEN c.body WHEN 'post' THEN p.body END AS current_text,
                (u.suspended_at IS NOT NULL OR ra.blocked_at IS NOT NULL) AS restricted,
                (SELECT COUNT(*) FROM reports other WHERE other.status = 'open'
                   AND (other.target_user_id = r.target_user_id OR other.target_actor_id = r.target_actor_id))
                AS account_open_reports, ",
        user_columns!("u."),
        ", ",
        remote_author_columns!(),
        " FROM reports r
          LEFT JOIN users reporter ON reporter.id = r.reporter_user_id
          LEFT JOIN federation_actors ra_reporter ON ra_reporter.id = r.reporter_actor_id
          LEFT JOIN users handler ON handler.id = r.handled_by
          LEFT JOIN posts p ON p.id = r.post_id
          LEFT JOIN comments c ON c.id = r.comment_id
          LEFT JOIN users u ON u.id = r.target_user_id
          LEFT JOIN federation_actors ra ON ra.id = r.target_actor_id
         WHERE r.status = $1 AND ($2::bigint IS NULL OR r.id < $2)
         ORDER BY r.id DESC
         LIMIT $3"
    ))
    .bind(status.as_str())
    .bind(before)
    .bind(PAGE_SIZE)
    .fetch_all(pool)
    .await?
    .iter()
    .map(Report::from_row)
    .collect()
}

/// Counts of each status, for the tabs.
pub async fn counts(pool: &PgPool) -> Result<[i64; 3], sqlx::Error> {
    let row = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE status = 'open') AS open,
                COUNT(*) FILTER (WHERE status = 'resolved') AS resolved,
                COUNT(*) FILTER (WHERE status = 'dismissed') AS dismissed
         FROM reports",
    )
    .fetch_one(pool)
    .await?;
    Ok([row.try_get("open")?, row.try_get("resolved")?, row.try_get("dismissed")?])
}

/// What a report is about, to act on it: (post, comment, local user, remote account).
pub async fn targets(pool: &PgPool, id: i64) -> Result<Option<(Option<i64>, Option<i64>, Option<i32>, Option<i64>)>, sqlx::Error> {
    sqlx::query("SELECT post_id, comment_id, target_user_id, target_actor_id FROM reports WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(|row| {
            Ok((
                row.try_get("post_id")?,
                row.try_get("comment_id")?,
                row.try_get("target_user_id")?,
                row.try_get("target_actor_id")?,
            ))
        })
        .transpose()
}

/// Changes the status of a report, by `moderator_id`.
pub async fn set_status(pool: &PgPool, id: i64, status: Status, moderator_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE reports SET status = $2,
                handled_by = CASE WHEN $2 = 'open' THEN NULL ELSE $3 END,
                handled_at = CASE WHEN $2 = 'open' THEN NULL ELSE NOW() END
         WHERE id = $1",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(moderator_id)
    .execute(pool)
    .await?;
    Ok(())
}
