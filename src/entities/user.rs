use sqlx::{PgPool, Row, postgres::PgRow, types::chrono::NaiveDate};

pub const USERNAME_MIN_LEN: usize = 3;
pub const USERNAME_MAX_LEN: usize = 30;
pub const NAME_MAX_CHARS: usize = 64;
pub const EMAIL_MAX_LEN: usize = 254;
pub const PASSWORD_MIN_CHARS: usize = 10;
/// Argon2 accepts more, this only stops absurdly long inputs.
pub const PASSWORD_MAX_CHARS: usize = 128;
pub const MIN_AGE_YEARS: u32 = 13;
pub const MAX_AGE_YEARS: u32 = 130;

#[derive(Clone, Debug)]
pub struct User {
    pub id: i32,
    /// Case is kept, but unique ignoring case, see `migrations/`.
    pub username: String,
    /// Name shown on the profile, can be empty.
    pub displayname: String,
    pub firstname: String,
    pub lastname: String,
    /// Asked at signup, `None` for accounts created before that. Not displayed or edited yet.
    #[allow(dead_code)]
    pub birthday: Option<NaiveDate>,
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
    let result = sqlx::query(
        "INSERT INTO users (username, email, password_hash, firstname, lastname, birthday)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, username, displayname, firstname, lastname, birthday, photo_url, background_url",
    )
    .bind(&account.username)
    .bind(&account.email)
    .bind(&account.password_hash)
    .bind(&account.firstname)
    .bind(&account.lastname)
    .bind(account.birthday)
    .fetch_one(pool)
    .await;

    match result {
        Ok(row) => User::from_row(&row).map_err(CreateAccountError::Database),
        Err(err) if is_unique_violation(&err) => {
            let constraint = err.as_database_error().and_then(|db_err| db_err.constraint());
            if constraint == Some("users_email_lower_idx") {
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
