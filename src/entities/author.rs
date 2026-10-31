//! Who wrote a post or a comment: a local user, or an account of another
//! Fediverse server (a Misskey user posting in a community, for example).

use sqlx::{Row, postgres::PgRow};

use crate::entities::{media, user::User};

/// Columns of the remote author read by `Author::from_row`, from
/// `federation_actors` joined as `ra`. A macro so it can be used in `concat!`.
macro_rules! remote_author_columns {
    () => {
        "ra.id AS ra_id, ra.actor_id AS ra_actor_id, ra.username AS ra_username,
         ra.display_name AS ra_display_name, ra.icon_url AS ra_icon_url, ra.profile_url AS ra_profile_url"
    };
}
pub(crate) use remote_author_columns;

/// An account of another server, as shown next to what it wrote.
#[derive(Clone, Debug)]
pub struct RemoteAuthor {
    /// Row of `federation_actors`, its page here is `/remote/{id}`.
    pub id: i64,
    /// Its ActivityPub id.
    pub actor_id: String,
    pub username: String,
    pub display_name: String,
    pub icon_url: Option<String>,
    /// Its profile page on its server.
    pub external_url: String,
    pub host: String,
}

impl RemoteAuthor {
    /// Reads `remote_author_columns!()`.
    pub fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        let actor_id: String = row.try_get("ra_actor_id")?;
        Ok(Self {
            id: row.try_get("ra_id")?,
            username: row.try_get("ra_username")?,
            display_name: row.try_get("ra_display_name")?,
            icon_url: row.try_get("ra_icon_url")?,
            external_url: row
                .try_get::<Option<String>, _>("ra_profile_url")?
                .unwrap_or_else(|| actor_id.clone()),
            host: host_of(&actor_id),
            actor_id,
        })
    }

    pub fn display_name(&self) -> String {
        if self.display_name.trim().is_empty() {
            self.username.clone()
        } else {
            self.display_name.clone()
        }
    }

    /// `@alice@misskey.io`.
    pub fn handle(&self) -> String {
        format!("@{}@{}", self.username, self.host)
    }

    /// The avatar, through the media proxy.
    pub fn photo_url(&self) -> Option<String> {
        self.icon_url.as_deref().map(media::proxy_url)
    }

    pub fn page_url(&self) -> String {
        format!("/remote/{}", self.id)
    }

    pub fn initial(&self) -> String {
        first_letter(&self.display_name())
    }
}

#[derive(Clone, Debug)]
pub enum Author {
    Local(User),
    Remote(RemoteAuthor),
}

impl Author {
    /// Reads `user_columns!("u.")` when the user is there, else
    /// `remote_author_columns!()`.
    pub fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        if row.try_get::<Option<i32>, _>("id")?.is_some() {
            return Ok(Self::Local(User::from_row(row)?));
        }
        Ok(Self::Remote(RemoteAuthor::from_row(row)?))
    }

    pub fn local(&self) -> Option<&User> {
        match self {
            Self::Local(user) => Some(user),
            Self::Remote(_) => None,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// "Creator" or "Moderator" for the site's staff, shown by the name.
    pub fn role_badge(&self) -> Option<&'static str> {
        self.local().and_then(|user| user.role.badge())
    }

    /// `admin` or `moderator`, for the badge's style.
    pub fn role_name(&self) -> &'static str {
        self.local().map_or("user", |user| user.role.as_str())
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Local(user) => user.display_name(),
            Self::Remote(remote) => remote.display_name(),
        }
    }

    /// `alice` for a local user, `alice@misskey.io` for a remote one.
    pub fn mention(&self) -> String {
        match self {
            Self::Local(user) => user.username.clone(),
            Self::Remote(remote) => format!("{}@{}", remote.username, remote.host),
        }
    }

    /// `@alice` or `@alice@misskey.io`.
    pub fn handle(&self) -> String {
        format!("@{}", self.mention())
    }

    /// Their profile here: a local user's, or the page of a remote account.
    pub fn profile_url(&self) -> String {
        match self {
            Self::Local(user) => format!("/user/{}", user.username),
            Self::Remote(remote) => remote.page_url(),
        }
    }

    pub fn photo_url(&self) -> Option<String> {
        match self {
            Self::Local(user) => user.photo_url.clone(),
            Self::Remote(remote) => remote.photo_url(),
        }
    }

    /// First letter of the display name, shown when there is no photo.
    pub fn initial(&self) -> String {
        first_letter(&self.display_name())
    }
}

fn first_letter(name: &str) -> String {
    name.chars()
        .next()
        .map(|letter| letter.to_uppercase().collect())
        .unwrap_or_default()
}

/// `misskey.io` from `https://misskey.io/users/9abc`.
pub fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}
