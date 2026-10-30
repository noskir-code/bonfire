//! Communities: pages with members, moderators and rules, where members
//! publish posts. Public ones are also Fediverse groups (see `src/API/`):
//! people on Misskey, Mastodon... follow them and post in them by mentioning
//! `@slug@domain`.
//!
//! The slug is the address, `/community/slug`, and the Fediverse name: it
//! shares the namespace of usernames, so `@cooking@domain` is either a user
//! or a community, never both (see `create` and `user::create_account`).

use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, Utc},
};

use crate::entities::{
    schema::{Column, Index, Table},
    user::{User, user_columns},
};

/// Same limits as usernames, since both are Fediverse names.
pub const SLUG_MIN_LEN: usize = 3;
pub const SLUG_MAX_LEN: usize = 30;
pub const NAME_MAX_CHARS: usize = 120;
pub const DESCRIPTION_MAX_CHARS: usize = 1_200;
pub const RULE_TITLE_MAX_CHARS: usize = 140;
pub const RULE_BODY_MAX_CHARS: usize = 2_000;
pub const MAX_RULES: i64 = 30;
/// Communities shown at once in the directory.
pub const PAGE_SIZE: i64 = 50;

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "communities",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        // Never changes, used in Fediverse addresses, like `users.public_id`.
        Column::new("public_id", "BIGINT")
            .default("1000000000000000 + (random() * 8999999999999999)::bigint"),
        Column::new("slug", "TEXT"),
        Column::new("name", "TEXT"),
        Column::new("description", "TEXT").default("''"),
        Column::new("photo_url", "TEXT").nullable(),
        // The community stays when its owner's account is deleted.
        Column::new("owner_user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE SET NULL"),
        Column::new("visibility", "TEXT").default("'public'"),
        Column::new("status", "TEXT").default("'active'"),
        // Its own keys: the community signs what it sends to other servers.
        Column::new("ap_public_key", "TEXT").nullable(),
        Column::new("ap_private_key", "TEXT").nullable(),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("updated_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("communities_slug_lower_idx", "LOWER(slug)"),
        Index::unique("communities_public_id_idx", "public_id"),
        Index::new("communities_status_idx", "status, id DESC"),
        Index::new("communities_name_lower_idx", "LOWER(name)"),
    ],
};

pub const MEMBERS_TABLE: Table = Table {
    name: "community_members",
    columns: &[
        Column::new("community_id", "BIGINT").references("communities(id) ON DELETE CASCADE"),
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("joined_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("community_members_community_user_idx", "community_id, user_id"),
        Index::new("community_members_user_idx", "user_id, joined_at DESC"),
    ],
};

/// Moderators besides the owner, who is always one.
pub const MODERATORS_TABLE: Table = Table {
    name: "community_moderators",
    columns: &[
        Column::new("community_id", "BIGINT").references("communities(id) ON DELETE CASCADE"),
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("granted_by_user_id", "INTEGER")
            .nullable()
            .references("users(id) ON DELETE SET NULL"),
        Column::new("granted_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("community_moderators_community_user_idx", "community_id, user_id"),
        Index::new("community_moderators_user_idx", "user_id, granted_at DESC"),
    ],
};

pub const RULES_TABLE: Table = Table {
    name: "community_rules",
    columns: &[
        Column::new("id", "BIGSERIAL").primary_key(),
        Column::new("community_id", "BIGINT").references("communities(id) ON DELETE CASCADE"),
        Column::new("title", "TEXT"),
        Column::new("body", "TEXT").default("''"),
        Column::new("sort_order", "INTEGER").default("0"),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("updated_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[Index::new("community_rules_order_idx", "community_id, sort_order, id")],
};

/// Communities a user doesn't want on their home timeline.
pub const IGNORES_TABLE: Table = Table {
    name: "community_ignores",
    columns: &[
        Column::new("community_id", "BIGINT").references("communities(id) ON DELETE CASCADE"),
        Column::new("user_id", "INTEGER").references("users(id) ON DELETE CASCADE"),
        Column::new("ignored_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        Index::unique("community_ignores_community_user_idx", "community_id, user_id"),
        Index::new("community_ignores_user_idx", "user_id, ignored_at DESC"),
    ],
};

/// Who can see the posts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Everyone, here and on the Fediverse.
    Public,
    /// Members only, never sent to other servers.
    Private,
}

impl Visibility {
    pub const ALL: [Self; 2] = [Self::Public, Self::Private];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "Public: everyone can read, also on the Fediverse",
            Self::Private => "Private: only members can read",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|visibility| visibility.as_str() == value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Active,
    /// Closed by the site's staff: nobody can see it or post in it.
    Banned,
}

impl Status {
    fn parse(value: &str) -> Self {
        if value == "banned" { Self::Banned } else { Self::Active }
    }
}

/// Columns read by `Community::from_row`, the community joined as `c`.
macro_rules! community_columns {
    () => {
        "c.id, c.public_id, c.slug, c.name, c.description, c.photo_url, c.owner_user_id,
         c.visibility, c.status, c.created_at,
         (SELECT COUNT(*) FROM community_members m WHERE m.community_id = c.id) AS member_count
         FROM communities c"
    };
}

#[derive(Clone, Debug)]
pub struct Community {
    pub id: i64,
    pub public_id: i64,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub photo_url: Option<String>,
    pub owner_user_id: Option<i32>,
    pub visibility: Visibility,
    pub status: Status,
    pub created_at: DateTime<Utc>,
    pub member_count: i64,
}

impl Community {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        let visibility: String = row.try_get("visibility")?;
        let status: String = row.try_get("status")?;
        Ok(Self {
            id: row.try_get("id")?,
            public_id: row.try_get("public_id")?,
            slug: row.try_get("slug")?,
            name: row.try_get("name")?,
            description: row.try_get("description")?,
            photo_url: row.try_get("photo_url")?,
            owner_user_id: row.try_get("owner_user_id")?,
            // An unknown value is treated as the most restrictive one.
            visibility: Visibility::parse(&visibility).unwrap_or(Visibility::Private),
            status: Status::parse(&status),
            created_at: row.try_get("created_at")?,
            member_count: row.try_get("member_count")?,
        })
    }

    pub fn is_public(&self) -> bool {
        self.visibility == Visibility::Public
    }

    pub fn is_banned(&self) -> bool {
        self.status == Status::Banned
    }

    /// Public and not banned: what other Fediverse servers can see.
    pub fn is_federated(&self) -> bool {
        self.is_public() && !self.is_banned()
    }

    /// First letter of the name, shown when there is no photo.
    pub fn initial(&self) -> String {
        self.name
            .chars()
            .next()
            .map(|letter| letter.to_uppercase().collect())
            .unwrap_or_default()
    }
}

/// What a user is in a community.
#[derive(Clone, Copy, Debug, Default)]
pub struct Membership {
    pub is_member: bool,
    pub is_owner: bool,
    pub is_moderator: bool,
    pub is_ignored: bool,
}

impl Membership {
    /// Owner or moderator: can change the settings and remove posts.
    pub fn can_manage(&self) -> bool {
        self.is_owner || self.is_moderator
    }
}

pub struct Rule {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub sort_order: i32,
}

pub struct Moderator {
    pub user: User,
    pub is_owner: bool,
}

/// Lowercase letters, digits and `_`, like a username but lowercase: it's
/// also the address of the page.
pub fn validate_slug(raw: &str) -> Option<String> {
    let slug = raw.trim().to_lowercase();
    let valid_length = (SLUG_MIN_LEN..=SLUG_MAX_LEN).contains(&slug.len());
    let valid_chars = slug
        .bytes()
        .all(|byte| byte == b'_' || byte.is_ascii_lowercase() || byte.is_ascii_digit());
    (valid_length && valid_chars).then_some(slug)
}

/// Trims and checks the length, no control characters.
pub fn validate_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let valid = !name.is_empty() && name.chars().count() <= NAME_MAX_CHARS && !name.chars().any(char::is_control);
    valid.then(|| name.to_string())
}

/// Like a bio: line breaks kept, may be empty.
pub fn validate_description(raw: &str) -> Option<String> {
    validate_text(raw, DESCRIPTION_MAX_CHARS, true)
}

pub fn validate_rule_title(raw: &str) -> Option<String> {
    validate_text(raw, RULE_TITLE_MAX_CHARS, false).filter(|title| !title.is_empty())
}

pub fn validate_rule_body(raw: &str) -> Option<String> {
    validate_text(raw, RULE_BODY_MAX_CHARS, true)
}

fn validate_text(raw: &str, max_chars: usize, multiline: bool) -> Option<String> {
    let text = raw.trim().replace("\r\n", "\n");
    let valid = text.chars().count() <= max_chars
        && !text
            .chars()
            .any(|letter| letter.is_control() && !(multiline && matches!(letter, '\n' | '\t')));
    valid.then_some(text)
}

#[derive(Debug)]
pub enum CreateError {
    /// Used by another community or a user.
    SlugTaken,
    Database(sqlx::Error),
}

/// Creates the community with `owner_id` as owner and first member.
pub async fn create(
    pool: &PgPool,
    owner_id: i32,
    slug: &str,
    name: &str,
    description: &str,
    visibility: Visibility,
) -> Result<Community, CreateError> {
    let mut transaction = pool.begin().await.map_err(CreateError::Database)?;
    // Not a username either: they share the Fediverse names.
    let inserted: Result<Option<i64>, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO communities (slug, name, description, visibility, owner_user_id)
         SELECT $1, $2, $3, $4, $5
         WHERE NOT EXISTS (SELECT 1 FROM users WHERE LOWER(username) = LOWER($1))
         RETURNING id",
    )
    .bind(slug)
    .bind(name)
    .bind(description)
    .bind(visibility.as_str())
    .bind(owner_id)
    .fetch_optional(&mut *transaction)
    .await;
    let id = match inserted {
        Ok(Some(id)) => id,
        Ok(None) => return Err(CreateError::SlugTaken),
        Err(err) if is_unique_violation(&err) => return Err(CreateError::SlugTaken),
        Err(err) => return Err(CreateError::Database(err)),
    };
    sqlx::query("INSERT INTO community_members (community_id, user_id) VALUES ($1, $2)")
        .bind(id)
        .bind(owner_id)
        .execute(&mut *transaction)
        .await
        .map_err(CreateError::Database)?;
    transaction.commit().await.map_err(CreateError::Database)?;
    find_by_id(pool, id)
        .await
        .map_err(CreateError::Database)?
        .ok_or(CreateError::Database(sqlx::Error::RowNotFound))
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db_err| db_err.code())
        .is_some_and(|code| code == "23505")
}

pub async fn find_by_id(pool: &PgPool, id: i64) -> Result<Option<Community>, sqlx::Error> {
    sqlx::query(concat!("SELECT ", community_columns!(), " WHERE c.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(Community::from_row)
        .transpose()
}

pub async fn find_by_slug(pool: &PgPool, slug: &str) -> Result<Option<Community>, sqlx::Error> {
    sqlx::query(concat!("SELECT ", community_columns!(), " WHERE LOWER(c.slug) = LOWER($1)"))
        .bind(slug.trim())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(Community::from_row)
        .transpose()
}

pub async fn find_by_public_id(pool: &PgPool, public_id: i64) -> Result<Option<Community>, sqlx::Error> {
    sqlx::query(concat!("SELECT ", community_columns!(), " WHERE c.public_id = $1"))
        .bind(public_id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(Community::from_row)
        .transpose()
}

/// The directory: communities not banned, biggest first, matching `search`
/// in their name or slug when given.
pub async fn list(pool: &PgPool, search: Option<&str>) -> Result<Vec<Community>, sqlx::Error> {
    // `%` and `_` typed by the user are searched as is.
    let pattern = search.map(|search| {
        format!(
            "%{}%",
            search.trim().to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
        )
    });
    sqlx::query(concat!(
        "SELECT ",
        community_columns!(),
        " WHERE c.status <> 'banned'
           AND ($1::text IS NULL OR LOWER(c.name) LIKE $1 OR LOWER(c.slug) LIKE $1)
         ORDER BY member_count DESC, c.id DESC
         LIMIT $2"
    ))
    .bind(pattern)
    .bind(PAGE_SIZE)
    .fetch_all(pool)
    .await?
    .iter()
    .map(Community::from_row)
    .collect()
}

/// Communities the user is a member of, most recently joined first.
pub async fn joined(pool: &PgPool, user_id: i32) -> Result<Vec<Community>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT ",
        community_columns!(),
        " JOIN community_members j ON j.community_id = c.id AND j.user_id = $1
         WHERE c.status <> 'banned'
         ORDER BY j.joined_at DESC"
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(Community::from_row)
    .collect()
}

pub async fn membership(pool: &PgPool, community: &Community, user_id: i32) -> Result<Membership, sqlx::Error> {
    let row = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM community_members WHERE community_id = $1 AND user_id = $2) AS member,
                EXISTS (SELECT 1 FROM community_moderators WHERE community_id = $1 AND user_id = $2) AS moderator,
                EXISTS (SELECT 1 FROM community_ignores WHERE community_id = $1 AND user_id = $2) AS ignored",
    )
    .bind(community.id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let is_owner = community.owner_user_id == Some(user_id);
    Ok(Membership {
        is_member: row.try_get("member")?,
        is_owner,
        is_moderator: is_owner || row.try_get("moderator")?,
        is_ignored: row.try_get("ignored")?,
    })
}

pub async fn join(pool: &PgPool, community_id: i64, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO community_members (community_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(community_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Leaving also removes the moderator role. The owner can't leave.
pub async fn quit(pool: &PgPool, community_id: i64, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH removed AS (
            DELETE FROM community_members WHERE community_id = $1 AND user_id = $2
              AND user_id IS DISTINCT FROM (SELECT owner_user_id FROM communities WHERE id = $1)
            RETURNING user_id
         )
         DELETE FROM community_moderators WHERE community_id = $1 AND user_id IN (SELECT user_id FROM removed)",
    )
    .bind(community_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns whether the community is ignored now.
pub async fn toggle_ignore(pool: &PgPool, community_id: i64, user_id: i32) -> Result<bool, sqlx::Error> {
    let removed = sqlx::query("DELETE FROM community_ignores WHERE community_id = $1 AND user_id = $2")
        .bind(community_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0;
    if removed {
        return Ok(false);
    }
    sqlx::query("INSERT INTO community_ignores (community_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(community_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(true)
}

pub async fn update_details(
    pool: &PgPool,
    id: i64,
    name: &str,
    description: &str,
    visibility: Visibility,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE communities SET name = $1, description = $2, visibility = $3, updated_at = NOW() WHERE id = $4",
    )
    .bind(name)
    .bind(description)
    .bind(visibility.as_str())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `None` removes the photo.
pub async fn update_photo(pool: &PgPool, id: i64, url: Option<&str>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE communities SET photo_url = $1, updated_at = NOW() WHERE id = $2")
        .bind(url)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_banned(pool: &PgPool, id: i64, banned: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE communities SET status = $1, updated_at = NOW() WHERE id = $2")
        .bind(if banned { "banned" } else { "active" })
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes the community with its posts, rules and members.
pub async fn delete(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM communities WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// --- Rules ---

pub async fn rules(pool: &PgPool, community_id: i64) -> Result<Vec<Rule>, sqlx::Error> {
    sqlx::query(
        "SELECT id, title, body, sort_order FROM community_rules WHERE community_id = $1
         ORDER BY sort_order, id",
    )
    .bind(community_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|row| {
        Ok(Rule {
            id: row.try_get("id")?,
            title: row.try_get("title")?,
            body: row.try_get("body")?,
            sort_order: row.try_get("sort_order")?,
        })
    })
    .collect()
}

/// Returns `false` when the community already has `MAX_RULES` rules.
pub async fn add_rule(pool: &PgPool, community_id: i64, title: &str, body: &str, sort_order: i32) -> Result<bool, sqlx::Error> {
    let added = sqlx::query(
        "INSERT INTO community_rules (community_id, title, body, sort_order)
         SELECT $1, $2, $3, $4 WHERE (SELECT COUNT(*) FROM community_rules WHERE community_id = $1) < $5",
    )
    .bind(community_id)
    .bind(title)
    .bind(body)
    .bind(sort_order)
    .bind(MAX_RULES)
    .execute(pool)
    .await?;
    Ok(added.rows_affected() > 0)
}

pub async fn update_rule(
    pool: &PgPool,
    community_id: i64,
    rule_id: i64,
    title: &str,
    body: &str,
    sort_order: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE community_rules SET title = $1, body = $2, sort_order = $3, updated_at = NOW()
         WHERE id = $4 AND community_id = $5",
    )
    .bind(title)
    .bind(body)
    .bind(sort_order)
    .bind(rule_id)
    .bind(community_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_rule(pool: &PgPool, community_id: i64, rule_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM community_rules WHERE id = $1 AND community_id = $2")
        .bind(rule_id)
        .bind(community_id)
        .execute(pool)
        .await?;
    Ok(())
}

// --- Moderators ---

/// The owner first, then the moderators in the order they were added.
pub async fn moderators(pool: &PgPool, community: &Community) -> Result<Vec<Moderator>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT ",
        user_columns!("u."),
        " FROM users u
         LEFT JOIN community_moderators cm ON cm.user_id = u.id AND cm.community_id = $1
         WHERE u.id = $2 OR cm.community_id IS NOT NULL
         ORDER BY (u.id = $2) DESC, cm.granted_at"
    ))
    .bind(community.id)
    .bind(community.owner_user_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|row| {
        let user = User::from_row(row)?;
        Ok(Moderator {
            is_owner: Some(user.id) == community.owner_user_id,
            user,
        })
    })
    .collect()
}

#[derive(Debug)]
pub enum AddModeratorError {
    /// No user with that name, or not a member.
    NotMember,
    Database(sqlx::Error),
}

/// Makes a member a moderator, by username.
pub async fn add_moderator(pool: &PgPool, community_id: i64, username: &str, granted_by: i32) -> Result<(), AddModeratorError> {
    let added = sqlx::query(
        "INSERT INTO community_moderators (community_id, user_id, granted_by_user_id)
         SELECT $1, u.id, $3 FROM users u
         JOIN community_members m ON m.user_id = u.id AND m.community_id = $1
         WHERE LOWER(u.username) = LOWER($2)
         ON CONFLICT DO NOTHING",
    )
    .bind(community_id)
    .bind(username.trim().trim_start_matches('@'))
    .bind(granted_by)
    .execute(pool)
    .await
    .map_err(AddModeratorError::Database)?;
    if added.rows_affected() == 0 {
        // Already one, or not a member: only the second is an error.
        let already: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM community_moderators cm JOIN users u ON u.id = cm.user_id
             WHERE cm.community_id = $1 AND LOWER(u.username) = LOWER($2))",
        )
        .bind(community_id)
        .bind(username.trim().trim_start_matches('@'))
        .fetch_one(pool)
        .await
        .map_err(AddModeratorError::Database)?;
        if !already {
            return Err(AddModeratorError::NotMember);
        }
    }
    Ok(())
}

pub async fn remove_moderator(pool: &PgPool, community_id: i64, user_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM community_moderators WHERE community_id = $1 AND user_id = $2")
        .bind(community_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_names() {
        assert_eq!(validate_slug(" Rust_Fans "), Some("rust_fans".to_string()));
        assert_eq!(validate_slug("ab"), None);
        assert_eq!(validate_slug("no-dash"), None);
        assert_eq!(validate_slug("é_letters"), None);
        assert_eq!(validate_slug(&"a".repeat(31)), None);
    }
}
