//! The search bar of every page (`/search`): users and communities of this
//! server by name, ignoring case, most relevant first (`/search/suggest`
//! gives the first ones while typing, see `assets/system.js`), or an
//! account of another Fediverse server from
//! `@name@server` or the link of its profile, to follow it. Also the page of
//! those accounts here (`/remote/{id}`) with their posts received by Tison.

use std::collections::HashMap;

use askama::Template;
use axum::{
    Form, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use serde_json::{Value, json};
use tower_sessions::Session;

use crate::{
    AppState,
    API::linker,
    entities::{
        author::RemoteAuthor,
        community::{self, Community},
        federation::{self as store, Signer},
        post::{self, Post},
        user::{self, User},
    },
    handlers::{
        notifier,
        system::{Layout, error_page, local_path, render_template},
        user::signed_in_user,
    },
};

/// A remote account with what the viewer does with it.
pub(crate) struct Account {
    pub(crate) author: RemoteAuthor,
    summary: String,
    /// `None` when not followed, else whether the follow was accepted.
    following: Option<bool>,
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    title: String,
    layout: Layout,
    query: String,
    /// What a search for `@name@server` found.
    found: Option<Account>,
    users: Vec<User>,
    communities: Vec<Community>,
    error: Option<String>,
    /// `@name@domain` of the viewer, for people on other servers.
    own_handle: Option<String>,
    /// Federation on: `@name@server` can be searched.
    fediverse: bool,
    /// Where the follow button comes back to: this search.
    return_to: String,
}

/// Whether `search` names an account of another server rather than words to
/// look for here: `@name@server`, `name@server`, or a link.
pub(crate) fn is_address(search: &str) -> bool {
    search.starts_with("https://")
        || search.starts_with("http://")
        || search
            .trim_start_matches(['@', '!'])
            .split_once('@')
            .is_some_and(|(name, domain)| !name.is_empty() && (domain.contains('.') || domain.contains(':')))
}

pub async fn search_page(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut layout = Layout::load(&state, &session).await;
    let Some(pool) = state.pool.clone() else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };
    let search = query.get("q").map(|search| search.trim().to_string()).unwrap_or_default();
    let mut error = query.get("error").map(|error| match error.as_str() {
        "follow" => "Following failed, try again later.".to_string(),
        _ => "Something went wrong, try again later.".to_string(),
    });

    let mut found = None;
    let mut users = Vec::new();
    let mut communities = Vec::new();
    if is_address(&search) {
        match &layout.viewer {
            Some(viewer) => match find_account(&state, &pool, viewer, &search).await {
                Ok(Found::Remote(account)) => found = Some(account),
                Ok(Found::Local(url)) => return Redirect::to(&url).into_response(),
                Err(message) => error = Some(message),
            },
            None => error = Some("Login to find accounts of other servers.".to_string()),
        }
    } else if !search.is_empty() {
        users = user::search(&pool, &search, user::SEARCH_SIZE).await.unwrap_or_else(|err| {
            tracing::error!("failed to search users: {}", err);
            Vec::new()
        });
        communities = community::list(&pool, Some(search.trim_start_matches(['@', '!'])))
            .await
            .unwrap_or_else(|err| {
                tracing::error!("failed to search communities: {}", err);
                Vec::new()
            });
    }
    let own_handle = match (&layout.viewer, &state.federation) {
        (Some(viewer), Some(federation)) if viewer.federation_enabled => {
            Some(format!("@{}@{}", viewer.username, federation.domain))
        }
        _ => None,
    };
    layout.search = search.clone();
    let query_for_return = search.clone();
    render_template(&SearchTemplate {
        title: if search.is_empty() { "Search".to_string() } else { format!("Search: {search}") },
        layout,
        query: search,
        found,
        users,
        communities,
        error,
        own_handle,
        fediverse: state.federation.is_some(),
        return_to: format!("/search?q={}", form_urlencode(&query_for_return)),
    })
}

/// Suggestions shown while typing, from `SUGGEST_MIN_CHARS` characters.
const SUGGEST_MIN_CHARS: usize = 3;
/// Of each kind.
const SUGGEST_SIZE: i64 = 5;

/// `{"users": [...], "communities": [...]}`, each with `name`, `detail`,
/// `url` and `photo` (or `initial`). Empty for short texts and for
/// addresses of other servers, which are only looked up on Enter.
pub async fn suggest(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Response {
    let search = query.get("q").map(|search| search.trim()).unwrap_or_default();
    let empty = || Json(json!({ "users": [], "communities": [] })).into_response();
    let Some(pool) = &state.pool else {
        return empty();
    };
    if search.trim_start_matches(['@', '!']).chars().count() < SUGGEST_MIN_CHARS || is_address(search) {
        return empty();
    }
    let users = user::search(pool, search, SUGGEST_SIZE).await;
    let communities = community::search_limited(pool, Some(search.trim_start_matches(['@', '!'])), SUGGEST_SIZE).await;
    let (users, communities) = match (users, communities) {
        (Ok(users), Ok(communities)) => (users, communities),
        (Err(err), _) | (_, Err(err)) => {
            tracing::error!("failed to suggest for the search: {}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let entry = |name: String, detail: String, url: String, photo: Option<&String>, initial: String| -> Value {
        json!({ "name": name, "detail": detail, "url": url, "photo": photo, "initial": initial })
    };
    Json(json!({
        "users": users.iter().map(|user| entry(
            user.display_name(),
            format!("@{}", user.username),
            format!("/user/{}", user.username),
            user.photo_url.as_ref(),
            user.initial(),
        )).collect::<Vec<_>>(),
        "communities": communities.iter().map(|community| entry(
            community.name.clone(),
            format!(
                "{} member{}{}",
                community.member_count,
                if community.member_count == 1 { "" } else { "s" },
                if community.is_public() { "" } else { " · Private" }
            ),
            format!("/community/{}", community.slug),
            community.photo_url.as_ref(),
            community.initial(),
        )).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// The old address of the search of other servers.
pub async fn old_fediverse_page(Query(query): Query<HashMap<String, String>>) -> Response {
    match query.get("q") {
        Some(search) => {
            let encoded: String = form_urlencode(search);
            Redirect::permanent(&format!("/search?q={encoded}")).into_response()
        }
        None => Redirect::permanent("/search").into_response(),
    }
}

/// Encodes a query value: letters and digits kept, the rest as `%XX`.
pub(crate) fn form_urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

pub(crate) enum Found {
    Remote(Account),
    /// A user or community of this server: their page.
    Local(String),
}

/// Looks up `search`, an address like `@alice@mastodon.social` or the link of
/// a profile, asking its server.
pub(crate) async fn find_account(state: &AppState, pool: &sqlx::PgPool, me: &User, search: &str) -> Result<Found, String> {
    let Some(federation) = state.federation.as_deref() else {
        return Err("The Fediverse is turned off on this server.".to_string());
    };
    let url = if search.starts_with("https://") || search.starts_with("http://") {
        search.to_string()
    } else {
        let account = search.trim_start_matches(['@', '!']);
        if let Some((name, domain)) = account.split_once('@')
            && domain.eq_ignore_ascii_case(&federation.domain)
        {
            return local_page(pool, name).await;
        }
        linker::webfinger(federation, account).await.map_err(|err| {
            tracing::info!("failed to find {}: {}", search, err);
            format!("No account found for {search}.")
        })?
    };
    if url.starts_with(&federation.base_url) {
        return Err("This is an address of this server.".to_string());
    }
    let key = linker::local_key(federation, pool, Signer::User(me.id))
        .await
        .map_err(|err| {
            tracing::error!("failed to load key of {}: {}", me.username, err);
            "Something went wrong, try again later.".to_string()
        })?;
    let actor = linker::fetch_actor(federation, pool, &url, &key).await.map_err(|err| {
        tracing::info!("failed to fetch {}: {}", url, err);
        format!("No account found for {search}.")
    })?;
    account(pool, actor.id, me.id)
        .await
        .map_err(|err| {
            tracing::error!("failed to load account {}: {}", actor.actor_id, err);
            "Something went wrong, try again later.".to_string()
        })?
        .map(Found::Remote)
        .ok_or_else(|| format!("No account found for {search}."))
}

/// `@name@this-domain`: the page of the user or community.
async fn local_page(pool: &sqlx::PgPool, name: &str) -> Result<Found, String> {
    let failed = |err: sqlx::Error| {
        tracing::error!("failed to find {}: {}", name, err);
        "Something went wrong, try again later.".to_string()
    };
    if let Some(found) = user::find_by_username(pool, name).await.map_err(failed)? {
        return Ok(Found::Local(format!("/user/{}", found.username)));
    }
    if let Some(found) = community::find_by_slug(pool, name).await.map_err(failed)? {
        return Ok(Found::Local(format!("/community/{}", found.slug)));
    }
    Err(format!("Nobody is called {name} here."))
}

async fn account(pool: &sqlx::PgPool, actor_id: i64, viewer_id: i32) -> Result<Option<Account>, sqlx::Error> {
    let Some((author, summary)) = store::actor_profile(pool, actor_id).await? else {
        return Ok(None);
    };
    let following = store::following_state(pool, viewer_id, actor_id).await?;
    Ok(Some(Account { author, summary, following }))
}

/// Follows or unfollows the account `actor` (a row of `federation_actors`),
/// then goes back to `return`.
async fn change_follow(state: AppState, session: Session, form: HashMap<String, String>, follow: bool) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let back = local_path(form.get("return"), "/search");
    let Some(federation) = state.federation.as_deref() else {
        return Redirect::to(&back).into_response();
    };
    let actor_id = form.get("actor").and_then(|id| id.parse::<i64>().ok());
    let actor = match actor_id {
        Some(actor_id) => match store::actor_profile(&pool, actor_id).await {
            Ok(Some((author, _))) => store::find_actor(&pool, &author.actor_id).await.ok().flatten().map(|(actor, _)| actor),
            _ => None,
        },
        None => None,
    };
    let Some(actor) = actor else {
        return error_page(Layout::load(&state, &session).await, StatusCode::NOT_FOUND, "This account is unknown here.");
    };
    if !me.federation_enabled {
        return error_page(
            Layout::load(&state, &session).await,
            StatusCode::FORBIDDEN,
            "Your account isn't on the Fediverse, so it can't follow accounts of other servers.",
        );
    }
    let result = if follow {
        notifier::follow(federation, &pool, &me, &actor).await
    } else {
        notifier::unfollow(federation, &pool, &me, &actor).await
    };
    match result {
        Ok(()) => {
            tracing::info!("@{} {} {}", me.username, if follow { "follows" } else { "unfollowed" }, actor.actor_id);
            Redirect::to(&back).into_response()
        }
        Err(err) => {
            tracing::error!("failed to change the follow of {}: {}", actor.actor_id, err);
            Redirect::to("/search?error=follow").into_response()
        }
    }
}

pub async fn follow(State(state): State<AppState>, session: Session, Form(form): Form<HashMap<String, String>>) -> Response {
    change_follow(state, session, form, true).await
}

pub async fn unfollow(State(state): State<AppState>, session: Session, Form(form): Form<HashMap<String, String>>) -> Response {
    change_follow(state, session, form, false).await
}

#[derive(Template)]
#[template(path = "remote_profile.html")]
struct RemoteProfileTemplate {
    title: String,
    layout: Layout,
    account: Account,
    posts: Vec<Post>,
    return_to: String,
}

/// An account of another server, with its posts stored here.
pub async fn remote_profile(State(state): State<AppState>, session: Session, Path(id): Path<i64>) -> Response {
    let layout = Layout::load(&state, &session).await;
    let Some(pool) = &state.pool else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };
    let viewer_id = layout.viewer.as_ref().map(|viewer| viewer.id);
    let loaded = async {
        let Some((author, summary)) = store::actor_profile(pool, id).await? else {
            return Ok(None);
        };
        let following = match viewer_id {
            Some(viewer_id) => store::following_state(pool, viewer_id, id).await?,
            None => None,
        };
        let posts = post::by_remote_author(pool, id, viewer_id).await?;
        Ok::<_, sqlx::Error>(Some((Account { author, summary, following }, posts)))
    }
    .await;
    match loaded {
        Ok(Some((account, posts))) => render_template(&RemoteProfileTemplate {
            title: format!("{} ({})", account.author.display_name(), account.author.handle()),
            layout,
            return_to: format!("/remote/{id}"),
            account,
            posts,
        }),
        Ok(None) => error_page(layout, StatusCode::NOT_FOUND, "This account is unknown here."),
        Err(err) => {
            tracing::error!("failed to load remote account {}: {}", id, err);
            error_page(layout, StatusCode::INTERNAL_SERVER_ERROR, "Failed to load this account.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_told_from_names() {
        assert!(is_address("@bob@mastodon.social"));
        assert!(is_address("bob@127.0.0.1:9100"));
        assert!(is_address("https://misskey.io/@bob"));
        assert!(!is_address("bob"));
        assert!(!is_address("@bob"));
        assert!(!is_address("cooking club"));
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(form_urlencode("@bob@m.example"), "%40bob%40m.example");
    }
}
