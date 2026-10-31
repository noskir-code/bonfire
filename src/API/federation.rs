//! Pages for other Fediverse servers: WebFinger to find a user or community
//! from `@name@domain`, NodeInfo to describe the server, actors, notes, and
//! the inboxes where activities arrive.

use std::collections::HashMap;

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{
    ACTIVITY_JSON, Federation,
    activities::{self, object_id},
    linker::{self, KeySource},
};
use crate::{
    AppState,
    entities::{
        comment,
        community::{self, Community},
        federation::{self as store, Signer},
        follow, post, user,
    },
};

/// An activity is a few kilobytes, more is refused before reading it as JSON.
const MAX_ACTIVITY_BYTES: usize = 256 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/.well-known/webfinger", get(webfinger))
        .route("/.well-known/nodeinfo", get(nodeinfo_links))
        .route("/nodeinfo/2.1", get(nodeinfo))
        .route("/ap/inbox", post(inbox))
        .route("/ap/users/{public_id}", get(actor))
        .route("/ap/users/{public_id}/inbox", post(inbox))
        .route("/ap/users/{public_id}/outbox", get(outbox))
        .route("/ap/users/{public_id}/followers", get(followers))
        .route("/ap/users/{public_id}/following", get(following))
        .route("/ap/communities/{public_id}", get(community_actor))
        .route("/ap/communities/{public_id}/inbox", post(inbox))
        .route("/ap/communities/{public_id}/outbox", get(community_outbox))
        .route("/ap/communities/{public_id}/followers", get(community_followers))
        .route("/ap/communities/{public_id}/moderators", get(community_moderators))
        .route("/ap/posts/{post_id}", get(post_object))
        .route("/ap/comments/{comment_id}", get(comment_object))
}

fn json_response(content_type: &'static str, value: &Value) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // Readable from web apps of other sites, like Mastodon's own pages do.
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*")),
        ],
        value.to_string(),
    )
        .into_response()
}

fn activity_response(value: &Value) -> Response {
    json_response(ACTIVITY_JSON, value)
}

/// Federation off, or no database.
fn enabled(state: &AppState) -> Option<(&Federation, &PgPool)> {
    Some((state.federation.as_deref()?, state.pool.as_ref()?))
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn server_error(context: &str, err: impl std::fmt::Display) -> Response {
    tracing::error!("{}: {}", context, err);
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// A local user other servers can see, from their public id.
async fn federated_user(pool: &PgPool, public_id: i64) -> Result<Option<user::User>, Response> {
    match user::find_by_public_id(pool, public_id).await {
        Ok(user) => Ok(user.filter(|user| user.is_federated())),
        Err(err) => Err(server_error("failed to load user", err)),
    }
}

/// `?resource=acct:alice@example.com`, or the address of the actor or profile.
async fn webfinger(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let Some(resource) = query.get("resource").map(|resource| resource.trim()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // Users and communities share the names, see `entities::community`.
    let (user, community) = if let Some(account) = resource.strip_prefix("acct:") {
        // Lemmy's clients write `!community@domain`.
        let (name, domain) = account
            .trim_start_matches(['@', '!'])
            .split_once('@')
            .unwrap_or((account, ""));
        if !domain.eq_ignore_ascii_case(&federation.domain) {
            return not_found();
        }
        (user::find_by_username(pool, name).await, community::find_by_slug(pool, name).await)
    } else if let Some(public_id) = federation.local_public_id(resource) {
        (user::find_by_public_id(pool, public_id).await, Ok(None))
    } else if let Some(public_id) = federation.local_community_public_id(resource) {
        (Ok(None), community::find_by_public_id(pool, public_id).await)
    } else if let Some(username) = resource.strip_prefix(&federation.profile_url("")) {
        (user::find_by_username(pool, username).await, Ok(None))
    } else if let Some(slug) = resource.strip_prefix(&federation.community_page_url("")) {
        (Ok(None), community::find_by_slug(pool, slug).await)
    } else {
        return not_found();
    };
    let (name, profile, actor) = match (user, community) {
        (Ok(Some(user)), _) if user.is_federated() => (
            user.username.clone(),
            federation.profile_url(&user.username),
            federation.actor_url(user.public_id),
        ),
        (_, Ok(Some(community))) if community.is_federated() => (
            community.slug.clone(),
            federation.community_page_url(&community.slug),
            federation.community_url(community.public_id),
        ),
        (Err(err), _) | (_, Err(err)) => return server_error("failed to load account for WebFinger", err),
        _ => return not_found(),
    };

    json_response(
        "application/jrd+json",
        &json!({
            "subject": format!("acct:{name}@{}", federation.domain),
            "aliases": [profile, actor],
            "links": [
                { "rel": "http://webfinger.net/rel/profile-page", "type": "text/html", "href": profile },
                { "rel": "self", "type": ACTIVITY_JSON, "href": actor },
            ],
        }),
    )
}

async fn nodeinfo_links(State(state): State<AppState>) -> Response {
    let Some((federation, _)) = enabled(&state) else {
        return not_found();
    };
    json_response(
        "application/json",
        &json!({
            "links": [{
                "rel": "http://nodeinfo.diaspora.software/ns/schema/2.1",
                "href": format!("{}/nodeinfo/2.1", federation.base_url),
            }],
        }),
    )
}

async fn nodeinfo(State(state): State<AppState>) -> Response {
    let Some((_, pool)) = enabled(&state) else {
        return not_found();
    };
    let (users, posts) = match store::instance_counts(pool).await {
        Ok(counts) => counts,
        Err(err) => return server_error("failed to count for NodeInfo", err),
    };
    json_response(
        "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.1#\"",
        &json!({
            "version": "2.1",
            "software": { "name": "bonfire", "version": env!("CARGO_PKG_VERSION") },
            "protocols": ["activitypub"],
            "services": { "inbound": [], "outbound": [] },
            "openRegistrations": true,
            "usage": { "users": { "total": users }, "localPosts": posts },
            "metadata": {},
        }),
    )
}

async fn actor(State(state): State<AppState>, Path(public_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let user = match federated_user(pool, public_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found(),
        Err(response) => return response,
    };
    match linker::keys(pool, Signer::User(user.id)).await {
        Ok((_, public_pem, _)) => activity_response(&activities::person(federation, &user, &public_pem)),
        Err(err) => server_error("failed to load keys", err),
    }
}

/// A community other servers can see, from its public id.
async fn federated_community(pool: &PgPool, public_id: i64) -> Result<Option<Community>, Response> {
    match community::find_by_public_id(pool, public_id).await {
        Ok(community) => Ok(community.filter(Community::is_federated)),
        Err(err) => Err(server_error("failed to load community", err)),
    }
}

async fn community_actor(State(state): State<AppState>, Path(public_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let community = match community::find_by_public_id(pool, public_id).await {
        Ok(Some(community)) => community,
        Ok(None) => return not_found(),
        Err(err) => return server_error("failed to load community", err),
    };
    // Private or closed: it left the Fediverse. Only its key stays, so other
    // servers can check the Undo and Reject it sent when leaving.
    if !community.is_federated() {
        return match store::local_keys(pool, Signer::Community(community.id)).await {
            Ok(Some((_, public_pem, _))) => activity_response(&activities::group_without_details(federation, &community, &public_pem)),
            Ok(None) => not_found(),
            Err(err) => server_error("failed to load keys", err),
        };
    }
    match linker::keys(pool, Signer::Community(community.id)).await {
        Ok((_, public_pem, _)) => activity_response(&activities::group(federation, &community, &public_pem)),
        Err(err) => server_error("failed to load keys", err),
    }
}

/// Only the number of posts, like the outbox of users.
async fn community_outbox(State(state): State<AppState>, uri: Uri, Path(public_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let community = match federated_community(pool, public_id).await {
        Ok(Some(community)) => community,
        Ok(None) => return not_found(),
        Err(response) => return response,
    };
    let count: Result<i64, _> = sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE community_id = $1")
        .bind(community.id)
        .fetch_one(pool)
        .await;
    match count {
        Ok(total) => activity_response(&activities::counted_collection(
            &format!("{}{}", federation.base_url, uri.path()),
            total,
        )),
        Err(err) => server_error("failed to count community posts", err),
    }
}

async fn community_followers(State(state): State<AppState>, uri: Uri, Path(public_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let community = match federated_community(pool, public_id).await {
        Ok(Some(community)) => community,
        Ok(None) => return not_found(),
        Err(response) => return response,
    };
    match store::community_follower_count(pool, community.id).await {
        Ok(total) => activity_response(&activities::counted_collection(
            &format!("{}{}", federation.base_url, uri.path()),
            total,
        )),
        Err(err) => server_error("failed to count community followers", err),
    }
}

/// The owner and moderators, as Lemmy lists them (`attributedTo` of the Group).
async fn community_moderators(State(state): State<AppState>, uri: Uri, Path(public_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let community = match federated_community(pool, public_id).await {
        Ok(Some(community)) => community,
        Ok(None) => return not_found(),
        Err(response) => return response,
    };
    let moderators = match community::moderators(pool, &community).await {
        Ok(moderators) => moderators,
        Err(err) => return server_error("failed to load moderators", err),
    };
    let items: Vec<String> = moderators
        .iter()
        .filter(|moderator| moderator.user.is_federated())
        .map(|moderator| federation.actor_url(moderator.user.public_id))
        .collect();
    activity_response(&activities::with_context(json!({
        "id": format!("{}{}", federation.base_url, uri.path()),
        "type": "OrderedCollection",
        "totalItems": items.len(),
        "orderedItems": items,
    })))
}

/// Only the number of public posts: Bonfire doesn't list them for other servers yet.
async fn outbox(State(state): State<AppState>, uri: Uri, Path(public_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let user = match federated_user(pool, public_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found(),
        Err(response) => return response,
    };
    match store::public_post_count(pool, user.id).await {
        Ok(total) => activity_response(&activities::counted_collection(
            &format!("{}{}", federation.base_url, uri.path()),
            total,
        )),
        Err(err) => server_error("failed to count posts", err),
    }
}

async fn followers(State(state): State<AppState>, uri: Uri, Path(public_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let user = match federated_user(pool, public_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found(),
        Err(response) => return response,
    };
    match follow::counts(pool, user.id).await.map(|counts| counts.followers) {
        Ok(total) => activity_response(&activities::counted_collection(
            &format!("{}{}", federation.base_url, uri.path()),
            total,
        )),
        Err(err) => server_error("failed to count followers", err),
    }
}

/// Only the number of accounts followed on other servers.
async fn following(State(state): State<AppState>, uri: Uri, Path(public_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let user = match federated_user(pool, public_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found(),
        Err(response) => return response,
    };
    match follow::counts(pool, user.id).await.map(|counts| counts.following) {
        Ok(total) => activity_response(&activities::counted_collection(
            &format!("{}{}", federation.base_url, uri.path()),
            total,
        )),
        Err(err) => server_error("failed to count followed accounts", err),
    }
}

/// A public post, fetched by servers when someone shares or searches it.
async fn post_object(State(state): State<AppState>, Path(post_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    // Viewed as a visitor: only public posts, and only those written here.
    match post::find(pool, post_id, None).await {
        Ok(Some(post)) if post.author.local().is_some_and(|author| author.is_federated()) => {
            match activities::post_note(federation, &post) {
                Some(note) => activity_response(&activities::with_context(note)),
                None => not_found(),
            }
        }
        Ok(_) => not_found(),
        Err(err) => server_error("failed to load post", err),
    }
}

async fn comment_object(State(state): State<AppState>, Path(comment_id): Path<i64>) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    let loaded = async {
        let Some((comment, post_id)) = comment::find_public(pool, comment_id).await? else {
            return Ok(None);
        };
        let Some(post) = post::find(pool, post_id, None).await? else {
            return Ok(None);
        };
        let in_reply_to = in_reply_to(federation, pool, &comment, &post).await?;
        Ok::<_, sqlx::Error>(Some((comment, post, in_reply_to)))
    }
    .await;
    match loaded {
        Ok(Some((comment, post, in_reply_to))) if comment.author.local().is_some_and(|author| author.is_federated()) => {
            match activities::comment_note(federation, &comment, &post, &in_reply_to) {
                Some(note) => activity_response(&activities::with_context(note)),
                None => not_found(),
            }
        }
        Ok(_) => not_found(),
        Err(err) => server_error("failed to load comment", err),
    }
}

/// The Note a comment answers: the comment it replies to, else its post.
pub async fn in_reply_to(
    federation: &Federation,
    pool: &PgPool,
    comment: &comment::Comment,
    post: &post::Post,
) -> Result<String, sqlx::Error> {
    if let Some(parent_id) = comment.parent_id
        && let Some((parent, _)) = comment::find_any(pool, parent_id).await?
    {
        return Ok(activities::comment_note_id(federation, &parent));
    }
    Ok(activities::note_id(federation, post))
}

/// Where other servers POST activities. The personal inboxes and the shared
/// one work the same: the activity says who it's for.
async fn inbox(State(state): State<AppState>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let Some((federation, pool)) = enabled(&state) else {
        return not_found();
    };
    if body.len() > MAX_ACTIVITY_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let Ok(activity) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let kind = activity["type"].as_str().unwrap_or_default();

    // Only activities Bonfire acts on are checked, the others are accepted and
    // dropped: checking means fetching the sender's key, for nothing.
    let recipient = activities::recipient(federation, pool, &activity).await;
    let local_key = match &recipient {
        Some(recipient) => match linker::local_key(federation, pool, recipient.signer()).await {
            Ok(key) => Some(key),
            Err(err) => return server_error("failed to load key", err),
        },
        None if matches!(kind, "Undo" | "Delete" | "Update") => None,
        None => {
            tracing::debug!("ignored {} activity sent to {}", kind, uri.path());
            return StatusCode::ACCEPTED.into_response();
        }
    };
    let key_source = match &local_key {
        Some(key) => KeySource::Fetch(key),
        None => KeySource::Known,
    };

    let sender = match linker::verify_request(federation, pool, &method, &uri, &headers, &body, key_source).await {
        Ok(sender) => sender,
        // A deleted account can't be fetched anymore, and if it never
        // followed anyone here there is nothing to forget.
        Err(err) if kind == "Delete" => {
            tracing::debug!("ignored unverifiable Delete: {}", err);
            return StatusCode::ACCEPTED.into_response();
        }
        Err(err) => {
            tracing::info!("refused {} activity on {}: {}", kind, uri.path(), err);
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    // Signed by someone else than the author: refused, it could be forged.
    if object_id(&activity["actor"]) != Some(sender.actor_id.as_str()) {
        tracing::info!("refused {} activity: signed by {} for another actor", kind, sender.actor_id);
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Blocked by the site's staff: accepted, so their server stops sending,
    // and ignored.
    if sender.blocked {
        tracing::debug!("ignored {} activity of blocked {}", kind, sender.actor_id);
        return StatusCode::ACCEPTED.into_response();
    }

    let recipient = recipient.as_ref().zip(local_key.as_ref());
    match activities::receive(federation, pool, &sender, &activity, recipient).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(err) => server_error("failed to handle activity", err),
    }
}
