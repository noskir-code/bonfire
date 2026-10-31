use sqlx::{
    PgPool, Row,
    postgres::PgRow,
    types::chrono::{DateTime, NaiveDate, Utc},
};

use crate::entities::schema::{Column, Index, Table};

pub const USERNAME_MIN_LEN: usize = 3;
pub const USERNAME_MAX_LEN: usize = 30;
pub const NAME_MAX_CHARS: usize = 64;
/// Same limit as Mastodon.
pub const BIO_MAX_CHARS: usize = 500;
pub const EMAIL_MAX_LEN: usize = 254;
pub const PASSWORD_MIN_CHARS: usize = 10;
/// Argon2 accepts more, this only stops absurdly long inputs.
pub const PASSWORD_MAX_CHARS: usize = 128;
pub const MIN_AGE_YEARS: u32 = 13;
pub const MAX_AGE_YEARS: u32 = 130;

const EMAIL_INDEX: &str = "users_email_lower_idx";

/// Columns loaded into `User`. A macro so it can be used in `concat!`,
/// sqlx only accepts SQL known at compile time.
/// `user_columns!("u.")` prefixes every column, for queries joining `users AS u`.
macro_rules! user_columns {
    () => {
        user_columns!("")
    };
    ($prefix:literal) => {
        concat!(
            $prefix, "id, ", $prefix, "username, ", $prefix, "displayname, ",
            $prefix, "firstname, ", $prefix, "lastname, ", $prefix, "birthday, ",
            $prefix, "gender, ", $prefix, "bio, ", $prefix, "firstname_public, ",
            $prefix, "lastname_public, ", $prefix, "birthday_public, ",
            $prefix, "photo_url, ", $prefix, "background_url, ", $prefix, "created_at, ",
            $prefix, "session_version, ", $prefix, "public_id, ",
            $prefix, "federation_enabled, ", $prefix, "role, ", $prefix, "suspended_at"
        )
    };
}
pub(crate) use user_columns;

/// Kept up to date automatically at startup, see `entities::schema`.
pub const TABLE: Table = Table {
    name: "users",
    columns: &[
        Column::new("id", "SERIAL").primary_key(),
        // Random 16-digit id that can be shown publicly, unlike `id`.
        Column::new("public_id", "BIGINT")
            .default("1000000000000000 + (random() * 8999999999999999)::bigint"),
        Column::new("username", "TEXT"),
        Column::new("displayname", "TEXT").default("''"),
        Column::new("firstname", "TEXT").default("''"),
        Column::new("lastname", "TEXT").default("''"),
        // Nullable for accounts created before it was asked at signup.
        Column::new("birthday", "DATE").nullable(),
        Column::new("gender", "TEXT").default("''"),
        Column::new("bio", "TEXT").default("''"),
        // Privacy: what other users can see on the profile.
        Column::new("firstname_public", "BOOLEAN").default("TRUE"),
        Column::new("lastname_public", "BOOLEAN").default("TRUE"),
        Column::new("birthday_public", "BOOLEAN").default("TRUE"),
        // `user`, `moderator` or `admin`, see `Role`. The first account made
        // is the admin, see `create_account`.
        Column::new("role", "TEXT").default("'user'"),
        // Nullable for accounts created before login existed, they can't sign in.
        Column::new("email", "TEXT").nullable(),
        Column::new("password_hash", "TEXT").nullable(),
        // Increased when the password changes: sessions signed in with an
        // older value are signed out, see `handlers::session::signed_in`.
        Column::new("session_version", "INTEGER").default("0"),
        Column::new("email_verified", "BOOLEAN").default("FALSE"),
        Column::new("email_verification_token", "TEXT").nullable(),
        Column::new("email_verification_sent_at", "TIMESTAMPTZ").nullable(),
        Column::new("email_verification_expires_at", "TIMESTAMPTZ").nullable(),
        Column::new("email_verified_at", "TIMESTAMPTZ").nullable(),
        Column::new("photo_url", "TEXT").nullable(),
        // CSS applied to the image, to crop or move it.
        Column::new("photo_style", "TEXT").default("''"),
        Column::new("background_url", "TEXT").nullable(),
        Column::new("background_style", "TEXT").default("''"),
        // ActivityPub actor. Nullable until the keys and URLs are generated.
        Column::new("ap_id", "TEXT").nullable(),
        Column::new("ap_inbox", "TEXT").nullable(),
        Column::new("ap_outbox", "TEXT").nullable(),
        Column::new("ap_public_key", "TEXT").nullable(),
        Column::new("ap_private_key", "TEXT").nullable(),
        // FALSE for users from other servers.
        Column::new("ap_local", "BOOLEAN").default("TRUE"),
        Column::new("ap_last_refreshed_at", "TIMESTAMPTZ").default("NOW()"),
        Column::new("federation_enabled", "BOOLEAN").default("TRUE"),
        Column::new("federation_display_name_mode", "TEXT").default("'full_name'"),
        // Set by the site's staff: can't sign in, posts and profile hidden.
        Column::new("suspended_at", "TIMESTAMPTZ").nullable(),
        Column::new("created_at", "TIMESTAMPTZ").default("NOW()"),
    ],
    indexes: &[
        // Like Mastodon: case is kept, but "Alice" and "alice" can't both exist.
        Index::unique("users_username_lower_idx", "LOWER(username)"),
        Index::unique(EMAIL_INDEX, "LOWER(email)"),
        Index::unique("users_public_id_idx", "public_id"),
        Index::unique("users_email_verification_token_idx", "email_verification_token"),
        // Searching users by name.
        Index::new("users_displayname_lower_idx", "LOWER(displayname)"),
        Index::new("users_role_idx", "role"),
    ],
};

/// What a user can do on the site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    User,
    /// Handles reports, see `handlers::moderation`.
    Moderator,
    /// A moderator who made the site: shown as its creator, and the only
    /// one naming moderators.
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Moderator => "moderator",
            Self::Admin => "admin",
        }
    }

    /// Unknown values are users: a typo never gives powers.
    pub fn parse(value: &str) -> Self {
        match value {
            "moderator" => Self::Moderator,
            "admin" => Self::Admin,
            _ => Self::User,
        }
    }

    /// Shown next to the name, `None` for users.
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Self::User => None,
            Self::Moderator => Some("Moderator"),
            Self::Admin => Some("Creator"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: i32,
    /// Case is kept, but unique ignoring case, see `TABLE`.
    pub username: String,
    /// Name shown on the profile, can be empty.
    pub displayname: String,
    pub firstname: String,
    pub lastname: String,
    /// Asked at signup, `None` for accounts created before that.
    pub birthday: Option<NaiveDate>,
    pub gender: String,
    pub bio: String,
    // Privacy: whether other users can see these on the profile.
    pub firstname_public: bool,
    pub lastname_public: bool,
    pub birthday_public: bool,
    pub photo_url: Option<String>,
    pub background_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub session_version: i32,
    /// Never changes, unlike the username: used in Fediverse addresses.
    pub public_id: i64,
    /// Whether other Fediverse servers can find and follow the account.
    pub federation_enabled: bool,
    pub role: Role,
    /// Suspended by the staff, see `set_suspended`.
    pub suspended_at: Option<DateTime<Utc>>,
}

impl User {
    pub fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            displayname: row.try_get("displayname")?,
            firstname: row.try_get("firstname")?,
            lastname: row.try_get("lastname")?,
            birthday: row.try_get("birthday")?,
            gender: row.try_get("gender")?,
            bio: row.try_get("bio")?,
            firstname_public: row.try_get("firstname_public")?,
            lastname_public: row.try_get("lastname_public")?,
            birthday_public: row.try_get("birthday_public")?,
            photo_url: row.try_get("photo_url")?,
            background_url: row.try_get("background_url")?,
            created_at: row.try_get("created_at")?,
            session_version: row.try_get("session_version")?,
            public_id: row.try_get("public_id")?,
            federation_enabled: row.try_get("federation_enabled")?,
            role: Role::parse(&row.try_get::<String, _>("role")?),
            suspended_at: row.try_get("suspended_at")?,
        })
    }

    /// "First Last", skipping the empty and private parts.
    pub fn full_name(&self) -> String {
        [
            (self.firstname.as_str(), self.firstname_public),
            (self.lastname.as_str(), self.lastname_public),
        ]
        .into_iter()
        .filter(|(part, public)| *public && !part.is_empty())
        .map(|(part, _)| part)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The display name, else the public parts of the full name, else the username.
    /// Shown to everyone, so it never contains a private name.
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

    /// Site moderators and admins: they can ban or delete any community.
    /// Moderators and the admin: they handle reports.
    pub fn is_staff(&self) -> bool {
        matches!(self.role, Role::Moderator | Role::Admin)
    }

    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended_at.is_some()
    }

    /// Other Fediverse servers can see and reach the account: on the
    /// Fediverse, and not suspended.
    pub fn is_federated(&self) -> bool {
        self.federation_enabled && !self.is_suspended()
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

/// Checks the username only uses `A-Z`, `a-z`, `0-9` and `_`. Case is kept.
pub fn validate_username(raw: &str) -> Option<String> {
    let username = raw.trim();
    let valid_length = (USERNAME_MIN_LEN..=USERNAME_MAX_LEN).contains(&username.len());
    let valid_chars = username
        .bytes()
        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
    (valid_length && valid_chars).then(|| username.to_string())
}

/// Trims the bio, keeps its line breaks, and rejects other control characters
/// or bios that are too long. An empty bio is valid: it removes the bio.
pub fn validate_bio(raw: &str) -> Option<String> {
    // Browsers send line breaks as \r\n.
    let bio = raw.trim().replace("\r\n", "\n");
    let valid = bio.chars().count() <= BIO_MAX_CHARS
        && !bio
            .chars()
            .any(|letter| letter.is_control() && !matches!(letter, '\n' | '\t'));
    valid.then_some(bio)
}

/// Trims the name and rejects control characters or names that are too long.
/// An empty name is valid: the username is displayed instead.
pub fn validate_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let valid = name.chars().count() <= NAME_MAX_CHARS && !name.chars().any(char::is_control);
    valid.then(|| name.to_string())
}

/// Lowercases the email and checks it looks like `name@domain.tld`.
/// Only a basic check: the address is not verified to exist.
pub fn validate_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_lowercase();
    let (local, domain) = email.split_once('@')?;
    let valid = email.len() <= EMAIL_MAX_LEN
        && !local.is_empty()
        && !domain.contains('@')
        && !email.chars().any(char::is_whitespace)
        && domain
            .split_once('.')
            .is_some_and(|(name, _)| !name.is_empty())
        && !domain.ends_with('.')
        && !domain.contains("..");
    valid.then_some(email)
}

/// Same rules as the old Instavox signup: at least `PASSWORD_MIN_CHARS` characters,
/// with an uppercase letter, a lowercase letter, a digit and a symbol.
/// Spaces are allowed only one at a time, between words.
pub fn validate_password(password: &str) -> bool {
    let length = password.chars().count();
    if !(PASSWORD_MIN_CHARS..=PASSWORD_MAX_CHARS).contains(&length)
        || password.starts_with(' ')
        || password.ends_with(' ')
        || password.contains("  ")
    {
        return false;
    }

    let (mut upper, mut lower, mut digit, mut symbol) = (false, false, false, false);
    for letter in password.chars() {
        match letter {
            ' ' => {}
            _ if letter.is_whitespace() || letter.is_control() => return false,
            _ if letter.is_uppercase() => upper = true,
            _ if letter.is_lowercase() => lower = true,
            _ if letter.is_ascii_digit() => digit = true,
            _ => symbol = true,
        }
    }
    upper && lower && digit && symbol
}

/// Parses a `YYYY-MM-DD` date (what `<input type="date">` sends) and checks
/// the user is at least `MIN_AGE_YEARS` old and the date is plausible.
pub fn validate_birthday(raw: &str, today: NaiveDate) -> Option<NaiveDate> {
    let birthday = NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok()?;
    let age = today.years_since(birthday)?;
    (MIN_AGE_YEARS..=MAX_AGE_YEARS)
        .contains(&age)
        .then_some(birthday)
}

/// Fields of a new account, already validated.
pub struct NewAccount {
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub firstname: String,
    pub lastname: String,
    pub birthday: NaiveDate,
}

#[derive(Debug)]
pub enum CreateAccountError {
    UsernameTaken,
    EmailTaken,
    Database(sqlx::Error),
}

pub async fn create_account(pool: &PgPool, account: &NewAccount) -> Result<User, CreateAccountError> {
    // Not a community's name either: they share the Fediverse names. The
    // first account of the site is its admin, whoever installed it.
    let result = sqlx::query(concat!(
        "INSERT INTO users (username, email, password_hash, firstname, lastname, birthday, role)
         SELECT $1, $2, $3, $4, $5, $6, CASE WHEN EXISTS (SELECT 1 FROM users) THEN 'user' ELSE 'admin' END
         WHERE NOT EXISTS (SELECT 1 FROM communities WHERE LOWER(slug) = LOWER($1))
         RETURNING ",
        user_columns!()
    ))
    .bind(&account.username)
    .bind(&account.email)
    .bind(&account.password_hash)
    .bind(&account.firstname)
    .bind(&account.lastname)
    .bind(account.birthday)
    .fetch_optional(pool)
    .await;

    match result {
        Ok(Some(row)) => User::from_row(&row).map_err(CreateAccountError::Database),
        Ok(None) => Err(CreateAccountError::UsernameTaken),
        Err(err) if is_unique_violation(&err) => {
            let constraint = err.as_database_error().and_then(|db_err| db_err.constraint());
            if constraint == Some(EMAIL_INDEX) {
                Err(CreateAccountError::EmailTaken)
            } else {
                Err(CreateAccountError::UsernameTaken)
            }
        }
        Err(err) => Err(CreateAccountError::Database(err)),
    }
}

/// Id and password hash of the account matching a username or an email, ignoring case.
/// The hash is `None` for accounts created before passwords existed.
pub async fn find_credentials(
    pool: &PgPool,
    login: &str,
) -> Result<Option<(i32, Option<String>)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, password_hash FROM users
         WHERE LOWER(username) = LOWER($1) OR LOWER(email) = LOWER($1)
         LIMIT 1",
    )
    .bind(login.trim())
    .fetch_optional(pool)
    .await?;

    row.map(|row| Ok((row.try_get("id")?, row.try_get("password_hash")?)))
        .transpose()
}

pub async fn find_by_id(pool: &PgPool, id: i32) -> Result<Option<User>, sqlx::Error> {
    sqlx::query(concat!("SELECT ", user_columns!(), " FROM users WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(User::from_row)
        .transpose()
}

pub async fn find_by_public_id(pool: &PgPool, public_id: i64) -> Result<Option<User>, sqlx::Error> {
    sqlx::query(concat!("SELECT ", user_columns!(), " FROM users WHERE public_id = $1"))
        .bind(public_id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(User::from_row)
        .transpose()
}

pub async fn find_by_username(pool: &PgPool, username: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users WHERE LOWER(username) = LOWER($1)"
    ))
    .bind(username.trim())
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(User::from_row)
    .transpose()
}

/// Gives a role to a user.
pub async fn set_role(pool: &PgPool, id: i32, role: Role) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET role = $2 WHERE id = $1")
        .bind(id)
        .bind(role.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

/// The site's staff: the admin first, then the moderators by name.
pub async fn staff(pool: &PgPool) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users WHERE role IN ('admin', 'moderator')
         ORDER BY role = 'admin' DESC, LOWER(username)"
    ))
    .fetch_all(pool)
    .await?
    .iter()
    .map(User::from_row)
    .collect()
}

/// Suspends or reinstates an account. A suspension also signs it out
/// everywhere (`session_version`).
pub async fn set_suspended(pool: &PgPool, id: i32, suspended: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET suspended_at = CASE WHEN $2 THEN NOW() END,
                          session_version = session_version + CASE WHEN $2 THEN 1 ELSE 0 END
         WHERE id = $1",
    )
    .bind(id)
    .bind(suspended)
    .execute(pool)
    .await?;
    Ok(())
}

/// Users shown at once in search results.
pub const SEARCH_SIZE: i64 = 30;

/// `%` and `_` typed by the user are searched as is in a LIKE pattern.
pub fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Users whose username, display name, or public first or last name contains
/// `search`, ignoring case. Most relevant first: the exact username, then
/// names starting with it, then names with a word starting with it.
pub async fn search(pool: &PgPool, search: &str, limit: i64) -> Result<Vec<User>, sqlx::Error> {
    let search = search.trim().trim_start_matches('@').to_lowercase();
    let escaped = escape_like(&search);
    sqlx::query(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users,
              LATERAL (SELECT LOWER(displayname) AS display,
                              CASE WHEN firstname_public THEN LOWER(firstname) ELSE '' END AS first,
                              CASE WHEN lastname_public THEN LOWER(lastname) ELSE '' END AS last) n
         WHERE suspended_at IS NULL
           AND (LOWER(username) LIKE '%' || $1 || '%' OR n.display LIKE '%' || $1 || '%'
            OR n.first LIKE '%' || $1 || '%' OR n.last LIKE '%' || $1 || '%')
         ORDER BY CASE
                    WHEN LOWER(username) = $2 THEN 0
                    WHEN LOWER(username) LIKE $1 || '%' THEN 1
                    WHEN n.display LIKE $1 || '%' OR n.first LIKE $1 || '%' OR n.last LIKE $1 || '%' THEN 2
                    WHEN n.display LIKE '% ' || $1 || '%' THEN 3
                    ELSE 4
                  END,
                  LENGTH(username), LOWER(username)
         LIMIT $3"
    ))
    .bind(escaped)
    .bind(search)
    .bind(limit)
    .fetch_all(pool)
    .await?
    .iter()
    .map(User::from_row)
    .collect()
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

/// `None` for accounts created before passwords existed.
pub async fn password_hash(pool: &PgPool, id: i32) -> Result<Option<String>, sqlx::Error> {
    Ok(sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .flatten())
}

/// Saves the new password and signs out every session of the user
/// (they keep the old `session_version`). Returns the updated user.
pub async fn update_password(pool: &PgPool, id: i32, password_hash: &str) -> Result<User, sqlx::Error> {
    let row = sqlx::query(concat!(
        "UPDATE users SET password_hash = $1, session_version = session_version + 1
         WHERE id = $2 RETURNING ",
        user_columns!()
    ))
    .bind(password_hash)
    .bind(id)
    .fetch_one(pool)
    .await?;
    User::from_row(&row)
}

/// Id and stored email of the account using `email`, ignoring case.
pub async fn find_by_email(pool: &PgPool, email: &str) -> Result<Option<(i32, String)>, sqlx::Error> {
    let row = sqlx::query("SELECT id, email FROM users WHERE LOWER(email) = LOWER($1)")
        .bind(email)
        .fetch_optional(pool)
        .await?;
    row.map(|row| Ok((row.try_get("id")?, row.try_get("email")?)))
        .transpose()
}

pub async fn update_bio(pool: &PgPool, id: i32, bio: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET bio = $1 WHERE id = $2")
        .bind(bio)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns `false` when the username is already used by someone else, or by
/// a community, ignoring case.
pub async fn update_username(pool: &PgPool, id: i32, username: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE users SET username = $1 WHERE id = $2
         AND NOT EXISTS (SELECT 1 FROM communities WHERE LOWER(slug) = LOWER($1))",
    )
    .bind(username)
    .bind(id)
    .execute(pool)
    .await;

    match result {
        Ok(done) => Ok(done.rows_affected() > 0),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_read_safely() {
        assert_eq!(Role::parse("admin"), Role::Admin);
        assert_eq!(Role::parse("moderator"), Role::Moderator);
        // A typo or an old value never gives powers.
        assert_eq!(Role::parse("Admin"), Role::User);
        assert_eq!(Role::parse("superuser"), Role::User);
        assert_eq!(Role::Admin.badge(), Some("Creator"));
        assert_eq!(Role::User.badge(), None);
    }
}
