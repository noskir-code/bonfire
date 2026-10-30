//! Fediverse support (ActivityPub), compatible with Mastodon, Pixelfed and the
//! other servers using it:
//! - `federation`: the pages other servers read (WebFinger, actors, posts) and
//!   the inbox where they send activities,
//! - `activities`: the JSON of actors, notes and activities, and what Bonfire
//!   does with the activities it receives,
//! - `linker`: requests to other servers, signed with HTTP Signatures, and the
//!   delivery queue.
//!
//! Local users get a stable address based on `public_id`, since usernames can
//! change: `https://example.com/ap/users/1234567890123456`. Other servers find
//! it from `@username@example.com` through WebFinger.
//!
//! Public communities are Fediverse groups, the way Lemmy and Guppe groups
//! work, so Misskey, Mastodon... users can take part:
//! `https://example.com/ap/communities/1234567890123456`, found from
//! `@slug@example.com`. Following it is joining: its posts reach the
//! followers as shares (Announce) by the community, and a public post of a
//! follower mentioning it is published in it.

pub mod activities;
mod base64;
pub mod federation;
pub mod linker;

use reqwest::Url;
use tokio::sync::Notify;

/// Content type of ActivityPub JSON.
pub const ACTIVITY_JSON: &str = "application/activity+json";

pub struct Federation {
    /// `PUBLIC_BASE_URL`, like `https://example.com`.
    pub base_url: String,
    /// Domain in `@username@domain`, with the port when it isn't the default one.
    pub domain: String,
    /// Allows `http://` and private addresses for other servers: only to test
    /// two Bonfire servers on one machine, see `FEDERATION_ALLOW_INSECURE`.
    pub allow_insecure: bool,
    client: reqwest::Client,
    /// Wakes the delivery worker when something is queued.
    deliveries: Notify,
}

impl Federation {
    pub fn new(base_url: &str, allow_insecure: bool) -> Result<Self, String> {
        let url = Url::parse(base_url).map_err(|err| format!("invalid PUBLIC_BASE_URL: {err}"))?;
        let host = url
            .host_str()
            .ok_or_else(|| "PUBLIC_BASE_URL has no domain".to_string())?;
        let domain = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self {
            client: linker::client(&base_url, allow_insecure)?,
            base_url,
            domain: domain.to_ascii_lowercase(),
            allow_insecure,
            deliveries: Notify::new(),
        })
    }

    /// Starts sending what was just queued.
    pub fn wake_deliveries(&self) {
        self.deliveries.notify_one();
    }

    pub fn actor_url(&self, public_id: i64) -> String {
        format!("{}/ap/users/{public_id}", self.base_url)
    }

    pub fn key_id(&self, public_id: i64) -> String {
        format!("{}#main-key", self.actor_url(public_id))
    }

    pub fn followers_url(&self, public_id: i64) -> String {
        format!("{}/followers", self.actor_url(public_id))
    }

    pub fn shared_inbox_url(&self) -> String {
        format!("{}/ap/inbox", self.base_url)
    }

    pub fn community_url(&self, public_id: i64) -> String {
        format!("{}/ap/communities/{public_id}", self.base_url)
    }

    pub fn community_key_id(&self, public_id: i64) -> String {
        format!("{}#main-key", self.community_url(public_id))
    }

    pub fn community_followers_url(&self, public_id: i64) -> String {
        format!("{}/followers", self.community_url(public_id))
    }

    /// The community's page for people.
    pub fn community_page_url(&self, slug: &str) -> String {
        format!("{}/community/{slug}", self.base_url)
    }

    /// Public id of the local community whose actor address is `url`.
    pub fn local_community_public_id(&self, url: &str) -> Option<i64> {
        url.strip_prefix(&self.base_url)?
            .strip_prefix("/ap/communities/")?
            .parse()
            .ok()
    }

    /// Id of a local post from its Note address, `/ap/posts/{id}`.
    pub fn local_post_id(&self, url: &str) -> Option<i64> {
        url.strip_prefix(&self.base_url)?.strip_prefix("/ap/posts/")?.parse().ok()
    }

    /// Id of a local comment from its Note address, `/ap/comments/{id}`.
    pub fn local_comment_id(&self, url: &str) -> Option<i64> {
        url.strip_prefix(&self.base_url)?.strip_prefix("/ap/comments/")?.parse().ok()
    }

    pub fn post_object_url(&self, post_id: i64) -> String {
        format!("{}/ap/posts/{post_id}", self.base_url)
    }

    pub fn comment_object_url(&self, comment_id: i64) -> String {
        format!("{}/ap/comments/{comment_id}", self.base_url)
    }

    /// Pages for people, linked from the ActivityPub objects.
    pub fn profile_url(&self, username: &str) -> String {
        format!("{}/user/{username}", self.base_url)
    }

    pub fn post_page_url(&self, post_id: i64) -> String {
        format!("{}/posts/{post_id}", self.base_url)
    }

    /// Makes a site path like `/public/uploads/a.webp` a full address.
    pub fn absolute_url(&self, path: &str) -> String {
        if path.starts_with("https://") || path.starts_with("http://") {
            path.to_string()
        } else {
            format!("{}{path}", self.base_url)
        }
    }

    /// Public id of the local user whose actor address is `url`.
    pub fn local_public_id(&self, url: &str) -> Option<i64> {
        url.strip_prefix(&self.base_url)?
            .strip_prefix("/ap/users/")?
            .parse()
            .ok()
    }
}
