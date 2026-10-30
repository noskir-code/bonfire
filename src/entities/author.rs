//! Who wrote a post or a comment: a local user, or an account of another
//! Fediverse server (a Misskey user posting in a community, for example).

use sqlx::{Row, postgres::PgRow};

use crate::entities::user::User;

/// Columns of the remote author read by `Author::from_row`, from
/// `federation_actors` joined as `ra`. A macro so it can be used in `concat!`.
macro_rules! remote_author_columns {
    () => {
        "ra.actor_id AS ra_actor_id, ra.username AS ra_username, ra.display_name AS ra_display_name,
         ra.icon_url AS ra_icon_url, ra.profile_url AS ra_profile_url"
    };
}
pub(crate) use remote_author_columns;

/// An account of another server, as shown next to what it wrote.
#[derive(Clone, Debug)]
pub struct RemoteAuthor {
    /// Its ActivityPub id.
    pub actor_id: String,
    pub username: String,
    pub display_name: String,
    pub icon_url: Option<String>,
    /// Its profile page on its server.
    pub profile_url: String,
    pub host: String,
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
        let actor_id: String = row.try_get("ra_actor_id")?;
        let host = host_of(&actor_id);
        Ok(Self::Remote(RemoteAuthor {
            username: row.try_get("ra_username")?,
            display_name: row.try_get("ra_display_name")?,
            icon_url: row.try_get("ra_icon_url")?,
            profile_url: row
                .try_get::<Option<String>, _>("ra_profile_url")?
                .unwrap_or_else(|| actor_id.clone()),
            host,
            actor_id,
        }))
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

    pub fn display_name(&self) -> String {
        match self {
            Self::Local(user) => user.display_name(),
            Self::Remote(remote) if !remote.display_name.trim().is_empty() => remote.display_name.clone(),
            Self::Remote(remote) => remote.username.clone(),
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

    /// Their profile: here, or on their server.
    pub fn profile_url(&self) -> String {
        match self {
            Self::Local(user) => format!("/user/{}", user.username),
            Self::Remote(remote) => remote.profile_url.clone(),
        }
    }

    pub fn photo_url(&self) -> Option<&str> {
        match self {
            Self::Local(user) => user.photo_url.as_deref(),
            Self::Remote(remote) => remote.icon_url.as_deref(),
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
}

/// `misskey.io` from `https://misskey.io/users/9abc`.
pub fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}
