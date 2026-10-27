use sqlx::{PgPool, Row, postgres::PgRow, types::chrono::NaiveDate};

pub const USERNAME_MIN_LEN: usize = 3;
pub const USERNAME_MAX_LEN: usize = 30;
pub const NAME_MAX_CHARS: usize = 64;

#[derive(Clone, Debug)]
pub struct User {
    pub id: i32,
    /// Case is kept, but unique ignoring case, see `ensure_schema`.
    pub username: String,
    /// Name shown on the profile, can be empty.
    pub displayname: String,
    pub firstname: String,
    pub lastname: String,
    // Not displayed or edited yet, it will be set at signup.
    #[allow(dead_code)]
    pub birthday: NaiveDate,
    pub photo_url: Option<String>,
    pub background_url: Option<String>,
}

impl User {
    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            displayname: row.try_get("displayname")?,
            firstname: row.try_get("firstname")?,
            lastname: row.try_get("lastname")?,
            birthday: row.try_get("birthday")?,
            photo_url: row.try_get("photo_url")?,
            background_url: row.try_get("background_url")?,
        })
    }

    /// "First Last", skipping the empty parts.
    pub fn full_name(&self) -> String {
        [self.firstname.as_str(), self.lastname.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The display name, else the full name, else the username.
    pub fn display_name(&self) -> String {
        if !self.displayname.is_empty() {
            return self.displayname.clone();
        }
        let full_name = self.full_name();
        if full_name.is_empty() {
            self.username.clone()
        } else {
            full_name
        }
    }

    /// First letter of the display name, shown when there is no photo.
    pub fn initial(&self) -> String {
        self.display_name()
            .chars()
            .next()
            .map(|letter| letter.to_uppercase().collect())
            .unwrap_or_default()
    }

    pub fn image_url(&self, image: ProfileImage) -> Option<&str> {
        match image {
            ProfileImage::Photo => self.photo_url.as_deref(),
            ProfileImage::Background => self.background_url.as_deref(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ProfileImage {
    Photo,
    Background,
}

impl ProfileImage {
    /// Used in uploaded file names.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Background => "background",
        }
    }
}

pub async fn ensure_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id SERIAL PRIMARY KEY,
            username TEXT NOT NULL,
            displayname TEXT NOT NULL DEFAULT '',
            firstname TEXT NOT NULL DEFAULT '',
            lastname TEXT NOT NULL DEFAULT '',
            birthday DATE NOT NULL DEFAULT NOW(),
            photo_url TEXT,
            background_url TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Like Mastodon: "Alice" and "alice" can't both exist.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS users_username_lower_idx ON users (LOWER(username))",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Checks the username only uses `A-Z`, `a-z`, `0-9` and `_`. Case is kept.
pub fn validate_username(raw: &str) -> Option<String> {
    let username = raw.trim();
    let valid_length = (USERNAME_MIN_LEN..=USERNAME_MAX_LEN).contains(&username.len());
    let valid_chars = username
        .bytes()
        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
    (valid_length && valid_chars).then(|| username.to_string())
}

/// Trims the name and rejects control characters or names that are too long.
/// An empty name is valid: the username is displayed instead.
pub fn validate_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let valid = name.chars().count() <= NAME_MAX_CHARS && !name.chars().any(char::is_control);
    valid.then(|| name.to_string())
}

// Only used by the dev login until signup is written.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub async fn create(pool: &PgPool, username: &str) -> Result<User, sqlx::Error> {
    let row = sqlx::query(
        "INSERT INTO users (username) VALUES ($1)
         RETURNING id, username, displayname, firstname, lastname, birthday, photo_url, background_url",
    )
    .bind(username)
    .fetch_one(pool)
    .await?;
    User::from_row(&row)
}

pub async fn find_by_id(pool: &PgPool, id: i32) -> Result<Option<User>, sqlx::Error> {
    sqlx::query(
        "SELECT id, username, displayname, firstname, lastname, birthday, photo_url, background_url
         FROM users WHERE id = $1",
    )
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(User::from_row)
        .transpose()
}

pub async fn find_by_username(pool: &PgPool, username: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query(
        "SELECT id, username, displayname, firstname, lastname, birthday, photo_url, background_url
         FROM users WHERE LOWER(username) = LOWER($1)",
    )
    .bind(username.trim())
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(User::from_row)
    .transpose()
}

pub async fn update_names(
    pool: &PgPool,
    id: i32,
    displayname: &str,
    firstname: &str,
    lastname: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET displayname = $1, firstname = $2, lastname = $3 WHERE id = $4")
        .bind(displayname)
        .bind(firstname)
        .bind(lastname)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns `false` when the username is already used by someone else, ignoring case.
pub async fn update_username(pool: &PgPool, id: i32, username: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("UPDATE users SET username = $1 WHERE id = $2")
        .bind(username)
        .bind(id)
        .execute(pool)
        .await;

    match result {
        Ok(_) => Ok(true),
        Err(err) if is_unique_violation(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

/// `None` removes the image.
pub async fn update_image(
    pool: &PgPool,
    id: i32,
    image: ProfileImage,
    url: Option<&str>,
) -> Result<(), sqlx::Error> {
    let query = match image {
        ProfileImage::Photo => "UPDATE users SET photo_url = $1 WHERE id = $2",
        ProfileImage::Background => "UPDATE users SET background_url = $1 WHERE id = $2",
    };
    sqlx::query(query).bind(url).bind(id).execute(pool).await?;
    Ok(())
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db_err| db_err.code())
        .is_some_and(|code| code == "23505")
}
