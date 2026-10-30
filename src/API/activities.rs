//! ActivityPub JSON: the actors of local users and communities, posts and
//! comments as notes, the activities sent when something happens, and the
//! handling of the activities other servers send.
//!
//! Built with `json!` rather than typed structs: the format has many optional
//! fields and Bonfire only reads a few of them.

use serde_json::{Value, json};
use sqlx::{
    PgPool,
    types::chrono::{DateTime, Utc},
};

use super::{
    Federation,
    linker::{self, LocalKey},
};
use crate::entities::{
    author::Author,
    comment::{self, Comment},
    community::{self, Community},
    federation::{self as store, RemoteActor, Signer},
    notification,
    post::{self, BODY_MAX_CHARS, Post, RemoteObject},
    user::{self, User},
};

/// Addressing a post to this makes it public.
pub const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
const ACTIVITY_STREAMS: &str = "https://www.w3.org/ns/activitystreams";
const SECURITY: &str = "https://w3id.org/security/v1";

/// Like `2026-10-30T17:21:00Z`, the format Mastodon expects.
fn iso(time: &DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Adds `@context`, only needed on the outermost object.
pub fn with_context(mut object: Value) -> Value {
    if let Value::Object(fields) = &mut object {
        fields.insert("@context".to_string(), json!(ACTIVITY_STREAMS));
    }
    object
}

/// Bonfire text is plain: escaped, blank lines become paragraphs and line
/// breaks `<br>`, which every Fediverse server displays.
pub fn text_to_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    escaped
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .map(|paragraph| format!("<p>{}</p>", paragraph.replace('\n', "<br>")))
        .collect()
}

/// The reverse, for notes of other servers: line breaks and paragraphs kept,
/// other tags removed (links keep their text), entities decoded. Bonfire
/// shows the result as plain text, escaped, so nothing here can inject HTML.
pub fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        let name: String = tag.trim_start_matches('/').chars().take_while(char::is_ascii_alphanumeric).collect();
        match (name.as_str(), tag.starts_with('/')) {
            ("br", _) => text.push('\n'),
            ("p" | "div" | "blockquote" | "li", true) => text.push_str("\n\n"),
            _ => {}
        }
        rest = &rest[start + end + 1..];
    }
    text.push_str(rest);
    let decoded = decode_entities(&text);
    // At most one blank line in a row.
    let mut cleaned = String::with_capacity(decoded.len());
    for line in decoded.split('\n') {
        if line.trim().is_empty() && cleaned.ends_with("\n\n") {
            continue;
        }
        cleaned.push_str(line.trim_end());
        cleaned.push('\n');
    }
    cleaned.trim().to_string()
}

fn decode_entities(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        rest = &rest[start..];
        let entity = rest[1..].find(';').filter(|&end| end <= 10).map(|end| &rest[1..end + 1]);
        let letter = entity.and_then(|entity| match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|number| number.parse().ok()))
                .and_then(char::from_u32),
        });
        match (entity, letter) {
            (Some(entity), Some(letter)) => {
                decoded.push(letter);
                rest = &rest[entity.len() + 2..];
            }
            _ => {
                decoded.push('&');
                rest = &rest[1..];
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

/// Ids in `to`, `cc`, `audience` and mentions of an object: who it's for.
fn audience(object: &Value) -> Vec<&str> {
    let mut ids = Vec::new();
    for field in ["to", "cc", "audience"] {
        match &object[field] {
            Value::String(id) => ids.push(id.as_str()),
            Value::Array(items) => ids.extend(items.iter().filter_map(object_id)),
            _ => {}
        }
    }
    if let Value::Array(tags) = &object["tag"] {
        ids.extend(
            tags.iter()
                .filter(|tag| tag["type"] == "Mention")
                .filter_map(|tag| tag["href"].as_str()),
        );
    }
    ids
}

/// Public, or "home" on Misskey (unlisted): addressed to everyone.
/// Followers-only and direct notes are never published in a community.
fn is_public(object: &Value) -> bool {
    audience(object)
        .iter()
        .any(|id| matches!(*id, PUBLIC | "as:Public" | "Public"))
}

// --- Objects ---

/// The actor of a local user: what other servers show as their profile.
pub fn person(federation: &Federation, user: &User, public_key_pem: &str) -> Value {
    let id = federation.actor_url(user.public_id);
    let mut person = json!({
        "@context": [
            ACTIVITY_STREAMS,
            SECURITY,
            { "manuallyApprovesFollowers": "as:manuallyApprovesFollowers" },
        ],
        "id": id,
        "type": "Person",
        "preferredUsername": user.username,
        "name": user.display_name(),
        "summary": text_to_html(&user.bio),
        "url": federation.profile_url(&user.username),
        "inbox": format!("{id}/inbox"),
        "outbox": format!("{id}/outbox"),
        "followers": format!("{id}/followers"),
        "following": format!("{id}/following"),
        "endpoints": { "sharedInbox": federation.shared_inbox_url() },
        // Every follow request is accepted.
        "manuallyApprovesFollowers": false,
        "published": iso(&user.created_at),
        "publicKey": {
            "id": federation.key_id(user.public_id),
            "owner": id,
            "publicKeyPem": public_key_pem,
        },
    });
    if let Some(photo) = &user.photo_url {
        person["icon"] = json!({ "type": "Image", "url": federation.absolute_url(photo) });
    }
    if let Some(background) = &user.background_url {
        person["image"] = json!({ "type": "Image", "url": federation.absolute_url(background) });
    }
    person
}

/// The actor of a public community: a Group, like Lemmy's communities.
pub fn group(federation: &Federation, community: &Community, public_key_pem: &str) -> Value {
    let id = federation.community_url(community.public_id);
    let mut group = json!({
        "@context": [
            ACTIVITY_STREAMS,
            SECURITY,
            {
                "manuallyApprovesFollowers": "as:manuallyApprovesFollowers",
                "lemmy": "https://join-lemmy.org/ns#",
                "postingRestrictedToMods": "lemmy:postingRestrictedToMods",
            },
        ],
        "id": id,
        "type": "Group",
        "preferredUsername": community.slug,
        "name": community.name,
        "summary": text_to_html(&community.description),
        "url": federation.community_page_url(&community.slug),
        "inbox": format!("{id}/inbox"),
        "outbox": format!("{id}/outbox"),
        "followers": format!("{id}/followers"),
        "attributedTo": format!("{id}/moderators"),
        "endpoints": { "sharedInbox": federation.shared_inbox_url() },
        "manuallyApprovesFollowers": false,
        "postingRestrictedToMods": false,
        "published": iso(&community.created_at),
        "publicKey": {
            "id": federation.community_key_id(community.public_id),
            "owner": id,
            "publicKeyPem": public_key_pem,
        },
    });
    if let Some(photo) = &community.photo_url {
        group["icon"] = json!({ "type": "Image", "url": federation.absolute_url(photo) });
    }
    group
}

/// The Note of a post: its own address for posts written here, the one of its
/// server for posts received.
pub fn note_id(federation: &Federation, post: &Post) -> String {
    match &post.ap_id {
        Some(ap_id) => ap_id.clone(),
        None => federation.post_object_url(post.id),
    }
}

/// The Note of a comment: same rule as `note_id`.
pub fn comment_note_id(federation: &Federation, comment: &Comment) -> String {
    match &comment.ap_id {
        Some(ap_id) => ap_id.clone(),
        None => federation.comment_object_url(comment.id),
    }
}

/// A public post written here as a Note. `None` for posts of other servers.
pub fn post_note(federation: &Federation, post: &Post) -> Option<Value> {
    let author = post.author.local()?;
    let mut cc = vec![federation.followers_url(author.public_id)];
    let mut note = json!({
        "id": federation.post_object_url(post.id),
        "type": "Note",
        "attributedTo": federation.actor_url(author.public_id),
        "content": text_to_html(&post.body),
        "url": federation.post_page_url(post.id),
        "published": iso(&post.created_at),
        "to": [PUBLIC],
        "sensitive": false,
        // Counts shown by servers that read them, like Mastodon 4.3+.
        "likes": { "type": "Collection", "totalItems": post.like_count },
        "shares": { "type": "Collection", "totalItems": post.share_count },
    });
    if let Some(community) = &post.community {
        // Lemmy's way (FEP-1b12) to say which group it's in.
        let group = federation.community_url(community.public_id);
        cc.push(group.clone());
        note["audience"] = json!(group);
    }
    note["cc"] = json!(cc);
    Some(note)
}

/// A comment written here as a Note replying to `in_reply_to` (the post's
/// Note, or the comment it answers). `None` for comments of other servers.
pub fn comment_note(federation: &Federation, comment: &Comment, post: &Post, in_reply_to: &str) -> Option<Value> {
    let author = comment.author.local()?;
    let mut cc = vec![federation.followers_url(author.public_id)];
    match &post.author {
        Author::Local(post_author) => cc.push(federation.actor_url(post_author.public_id)),
        Author::Remote(remote) => cc.push(remote.actor_id.clone()),
    }
    let mut note = json!({
        "id": federation.comment_object_url(comment.id),
        "type": "Note",
        "attributedTo": federation.actor_url(author.public_id),
        "inReplyTo": in_reply_to,
        "content": text_to_html(&comment.body),
        "url": format!("{}#comment-{}", federation.post_page_url(post.id), comment.id),
        "published": iso(&comment.created_at),
        "to": [PUBLIC],
        "sensitive": false,
    });
    if let Some(community) = &post.community {
        let group = federation.community_url(community.public_id);
        cc.push(group.clone());
        note["audience"] = json!(group);
    }
    note["cc"] = json!(cc);
    Some(note)
}

/// A collection only giving its size: enough for other servers to show counts.
pub fn counted_collection(id: &str, total: i64) -> Value {
    with_context(json!({ "id": id, "type": "OrderedCollection", "totalItems": total }))
}

// --- Activities ---

/// Publishes a Note, addressed like the note itself.
pub fn create(note: &Value) -> Value {
    with_context(json!({
        "id": format!("{}/activity", note["id"].as_str().unwrap_or_default()),
        "type": "Create",
        "actor": note["attributedTo"],
        "published": note["published"],
        "to": note["to"],
        "cc": note["cc"],
        "object": note,
    }))
}

/// Where a reaction is addressed: the post's author.
fn author_id(federation: &Federation, post: &Post) -> String {
    match &post.author {
        Author::Local(author) => federation.actor_url(author.public_id),
        Author::Remote(remote) => remote.actor_id.clone(),
    }
}

/// A like of a post. `liked_at` makes the id unique: liking again after
/// removing the like is a new activity.
pub fn like(federation: &Federation, liker: &User, post: &Post, liked_at: &DateTime<Utc>) -> Value {
    let actor = federation.actor_url(liker.public_id);
    json!({
        "id": format!("{actor}/likes/{}/{}", post.id, liked_at.timestamp_micros()),
        "type": "Like",
        "actor": actor,
        "object": note_id(federation, post),
        "to": [author_id(federation, post)],
        "cc": [PUBLIC],
    })
}

/// A share (boost, renote) of a post, shown on the sharer's profile.
pub fn announce(federation: &Federation, sharer: &User, post: &Post, shared_at: &DateTime<Utc>) -> Value {
    let actor = federation.actor_url(sharer.public_id);
    json!({
        "id": format!("{actor}/shares/{}/{}", post.id, shared_at.timestamp_micros()),
        "type": "Announce",
        "actor": actor,
        "published": iso(shared_at),
        "object": note_id(federation, post),
        "to": [PUBLIC],
        "cc": [author_id(federation, post), federation.followers_url(sharer.public_id)],
    })
}

/// A community sharing one of its posts with its followers: how they see the
/// community's posts in their timeline (a renote on Misskey, a boost on
/// Mastodon). The Note itself is announced, not its Create: Misskey and
/// Mastodon only show announced notes.
pub fn community_announce(federation: &Federation, community_public_id: i64, note_id: &str, post_id: i64) -> Value {
    let actor = federation.community_url(community_public_id);
    with_context(json!({
        "id": format!("{actor}/announces/{post_id}"),
        "type": "Announce",
        "actor": actor,
        "published": iso(&Utc::now()),
        "object": note_id,
        "to": [PUBLIC],
        "cc": [federation.community_followers_url(community_public_id)],
    }))
}

/// Cancels `activity` (a Like or an Announce), addressed the same way.
pub fn undo(activity: &Value) -> Value {
    with_context(json!({
        "id": format!("{}/undo", activity["id"].as_str().unwrap_or_default()),
        "type": "Undo",
        "actor": activity["actor"],
        "to": activity["to"],
        "cc": activity["cc"],
        "object": activity,
    }))
}

fn accept(actor: &str, follow: &Value) -> Value {
    with_context(json!({
        "id": format!("{actor}/accepts/{}", Utc::now().timestamp_micros()),
        "type": "Accept",
        "actor": actor,
        "object": follow,
    }))
}

// --- Receiving ---

/// Id of an object given either as an address or as an embedded object.
pub fn object_id(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| value.get("id")?.as_str())
}

/// Who an incoming activity is for, when it's one Bonfire acts on.
pub enum Recipient {
    User(User),
    Community(Community),
}

impl Recipient {
    /// Signs the requests made while handling it, like fetching the sender's key.
    pub fn signer(&self) -> Signer {
        match self {
            Self::User(user) => Signer::User(user.id),
            Self::Community(community) => Signer::Community(community.id),
        }
    }
}

/// The local user or community with actor address `url`, if other servers can
/// see it.
async fn local_actor(federation: &Federation, pool: &PgPool, url: &str) -> Option<Recipient> {
    if let Some(public_id) = federation.local_public_id(url) {
        return match user::find_by_public_id(pool, public_id).await {
            Ok(user) => user.filter(|user| user.federation_enabled).map(Recipient::User),
            Err(err) => {
                tracing::warn!("failed to load user {}: {}", public_id, err);
                None
            }
        };
    }
    let public_id = federation.local_community_public_id(url)?;
    match community::find_by_public_id(pool, public_id).await {
        Ok(community) => community.filter(Community::is_federated).map(Recipient::Community),
        Err(err) => {
            tracing::warn!("failed to load community {}: {}", public_id, err);
            None
        }
    }
}

/// A local post or comment a note replies to: (post id, comment id).
async fn reply_target(federation: &Federation, pool: &PgPool, in_reply_to: &str) -> Option<(i64, Option<i64>)> {
    if let Some(post_id) = federation.local_post_id(in_reply_to) {
        return Some((post_id, None));
    }
    if let Some(comment_id) = federation.local_comment_id(in_reply_to) {
        let (_, post_id) = comment::find_public(pool, comment_id).await.ok()??;
        return Some((post_id, Some(comment_id)));
    }
    // A post or comment received from another server and stored here.
    if let Ok(Some(post_id)) = post::find_by_ap_id(pool, in_reply_to).await {
        return Some((post_id, None));
    }
    let (comment_id, post_id) = comment::find_by_ap_id(pool, in_reply_to).await.ok()??;
    Some((post_id, Some(comment_id)))
}

/// The recipient of an incoming activity, `None` when Bonfire ignores it:
/// - a Follow of a local user or community, or the Undo of one,
/// - a public Create of a Note mentioning a local community, or replying to
///   a post or comment stored here.
pub async fn recipient(federation: &Federation, pool: &PgPool, activity: &Value) -> Option<Recipient> {
    match activity["type"].as_str()? {
        "Follow" => local_actor(federation, pool, object_id(&activity["object"])?).await,
        "Undo" if activity["object"]["type"] == "Follow" => {
            local_actor(federation, pool, object_id(&activity["object"]["object"])?).await
        }
        "Create" => {
            let note = &activity["object"];
            if note["type"] != "Note" && note["type"] != "Page" && note["type"] != "Article" {
                return None;
            }
            for id in audience(note) {
                if federation.local_community_public_id(id).is_some()
                    && let Some(recipient) = local_actor(federation, pool, id).await
                {
                    return Some(recipient);
                }
            }
            // A reply: for the community of the post, else its local author.
            let (post_id, _) = reply_target(federation, pool, note["inReplyTo"].as_str()?).await?;
            let post = post::find(pool, post_id, None).await.ok()??;
            if let Some(community) = &post.community {
                return community::find_by_id(pool, community.id)
                    .await
                    .ok()?
                    .filter(Community::is_federated)
                    .map(Recipient::Community);
            }
            post.author.local().cloned().filter(|user| user.federation_enabled).map(Recipient::User)
        }
        _ => None,
    }
}

/// The text of a note of another server. Misskey sends its original text
/// (MFM, close to plain text), which reads better than the HTML. In a
/// community, the mention of the community starting the text is removed,
/// see `strip_mention`.
fn note_text(note: &Value, community: Option<(&str, &str)>) -> String {
    let source = note["_misskey_content"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            note["source"]["content"]
                .as_str()
                .filter(|_| note["source"]["mediaType"].as_str().is_none_or(|kind| kind.starts_with("text/")))
                .filter(|_| note["source"]["mediaType"] != "text/html")
                .map(str::to_string)
        })
        .unwrap_or_else(|| html_to_text(note["content"].as_str().unwrap_or_default()));
    let source = match community {
        Some((slug, domain)) => strip_mention(&source, slug, domain),
        None => source,
    };
    let text = match note["summary"].as_str().map(str::trim).filter(|summary| !summary.is_empty()) {
        // A content warning.
        Some(summary) => format!("⚠ {summary}\n\n{source}"),
        None => source,
    };
    // Control characters other than line breaks and tabs are dropped,
    // like for posts written here.
    let text: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter(|letter| !letter.is_control() || matches!(letter, '\n' | '\t'))
        .collect();
    let text = text.trim();
    match text.char_indices().nth(BODY_MAX_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// Removes the mention of the community at the start of a post: on Misskey
/// one writes `@cooking@example.com Hello`, the community shows `Hello`.
fn strip_mention(text: &str, slug: &str, domain: &str) -> String {
    let mut rest = text.trim_start();
    for mention in [format!("@{slug}@{domain}"), format!("@{slug}")] {
        if rest.len() >= mention.len()
            && rest.is_char_boundary(mention.len())
            && rest[..mention.len()].eq_ignore_ascii_case(&mention)
            && rest[mention.len()..].chars().next().is_none_or(char::is_whitespace)
        {
            rest = rest[mention.len()..].trim_start();
            break;
        }
    }
    rest.to_string()
}

fn published(note: &Value) -> Option<DateTime<Utc>> {
    let published = DateTime::parse_from_rfc3339(note["published"].as_str()?).ok()?.with_timezone(&Utc);
    // Never in the future: it would stay first on every page.
    Some(published.min(Utc::now()))
}

/// Queues `activity` signed by `signer` for `inboxes`, and starts sending.
async fn send(federation: &Federation, pool: &PgPool, signer: Signer, inboxes: &[String], activity: &Value) -> Result<(), String> {
    store::enqueue(pool, signer, inboxes, &activity.to_string())
        .await
        .map_err(|err| format!("failed to queue activity: {err}"))?;
    federation.wake_deliveries();
    Ok(())
}

/// Acts on a verified activity from `sender`. Activities Bonfire doesn't
/// handle yet (likes of local posts...) are accepted and ignored.
pub async fn receive(
    federation: &Federation,
    pool: &PgPool,
    sender: &RemoteActor,
    activity: &Value,
    recipient: Option<(&Recipient, &LocalKey)>,
) -> Result<(), String> {
    let kind = activity["type"].as_str().unwrap_or_default();
    match (kind, recipient) {
        ("Follow", Some((Recipient::User(user), _))) => {
            let follow_id = activity["id"].as_str().unwrap_or_default();
            let is_new = store::add_follower(pool, user.id, sender, follow_id)
                .await
                .map_err(|err| format!("failed to save follower: {err}"))?;
            if is_new && let Err(err) = notification::followed(pool, user.id, sender.id).await {
                tracing::warn!("failed to notify @{} of a follower: {}", user.username, err);
            }
            // Sent to their own inbox, not the shared one: it's for them only.
            let accept = accept(&federation.actor_url(user.public_id), activity);
            send(federation, pool, Signer::User(user.id), std::slice::from_ref(&sender.inbox), &accept).await?;
            tracing::info!("{} now follows @{}", sender.actor_id, user.username);
        }
        ("Follow", Some((Recipient::Community(community), _))) => {
            let follow_id = activity["id"].as_str().unwrap_or_default();
            store::add_community_follower(pool, community.id, sender, follow_id)
                .await
                .map_err(|err| format!("failed to save community follower: {err}"))?;
            let accept = accept(&federation.community_url(community.public_id), activity);
            let signer = Signer::Community(community.id);
            send(federation, pool, signer, std::slice::from_ref(&sender.inbox), &accept).await?;
            tracing::info!("{} now follows community {}", sender.actor_id, community.slug);
        }
        ("Undo", Some((Recipient::User(user), _))) => {
            store::remove_follower(pool, user.id, sender)
                .await
                .map_err(|err| format!("failed to remove follower: {err}"))?;
            tracing::info!("{} stopped following @{}", sender.actor_id, user.username);
        }
        ("Undo", Some((Recipient::Community(community), _))) => {
            store::remove_community_follower(pool, community.id, sender)
                .await
                .map_err(|err| format!("failed to remove community follower: {err}"))?;
            tracing::info!("{} stopped following community {}", sender.actor_id, community.slug);
        }
        // Undo giving only the Follow's id.
        ("Undo", None) => {
            if let Some(follow_id) = activity["object"].as_str() {
                store::remove_follow(pool, sender, follow_id)
                    .await
                    .map_err(|err| format!("failed to remove follow: {err}"))?;
            }
        }
        ("Create", Some((recipient, _))) => receive_note(federation, pool, sender, &activity["object"], recipient).await?,
        // Their account was deleted.
        ("Delete", _) if object_id(&activity["object"]) == Some(sender.actor_id.as_str()) => {
            store::delete_actor(pool, &sender.actor_id)
                .await
                .map_err(|err| format!("failed to delete actor: {err}"))?;
            tracing::info!("forgot deleted account {}", sender.actor_id);
        }
        // One of their posts or comments was deleted.
        ("Delete", _) => {
            let Some(id) = object_id(&activity["object"]) else {
                return Ok(());
            };
            let found = post::find_remote_object(pool, sender.id, id)
                .await
                .map_err(|err| format!("failed to find deleted note: {err}"))?;
            if let Some(object) = found {
                post::delete_remote(pool, &object)
                    .await
                    .map_err(|err| format!("failed to delete note: {err}"))?;
                tracing::info!("deleted {} on request of its author", id);
            }
        }
        // Their profile changed, maybe their key or inbox.
        ("Update", _) if object_id(&activity["object"]) == Some(sender.actor_id.as_str()) => {
            if activity["object"].get("publicKey").is_some() {
                let actor = linker::parse_actor(&activity["object"])?;
                store::save_actor(pool, &actor)
                    .await
                    .map_err(|err| format!("failed to update actor: {err}"))?;
            }
        }
        // One of their posts or comments was edited.
        ("Update", _) => {
            let note = &activity["object"];
            let Some(id) = object_id(note) else {
                return Ok(());
            };
            let found = post::find_remote_object(pool, sender.id, id)
                .await
                .map_err(|err| format!("failed to find edited note: {err}"))?;
            if let Some(object) = found {
                let community = match &object {
                    RemoteObject::Post(_, Some(community_id)) => community::find_by_id(pool, *community_id).await.ok().flatten(),
                    _ => None,
                };
                let text = note_text(note, community.as_ref().map(|community| (community.slug.as_str(), federation.domain.as_str())));
                if !text.is_empty() {
                    post::update_remote_body(pool, &object, &text)
                        .await
                        .map_err(|err| format!("failed to update note: {err}"))?;
                }
            }
        }
        _ => tracing::debug!("ignored {} activity from {}", kind, sender.actor_id),
    }
    Ok(())
}

/// A public note of another server: a post in a community, or a comment
/// replying to a post or comment stored here.
async fn receive_note(
    federation: &Federation,
    pool: &PgPool,
    sender: &RemoteActor,
    note: &Value,
    recipient: &Recipient,
) -> Result<(), String> {
    let Some(ap_id) = note["id"].as_str() else {
        return Ok(());
    };
    // Only their own notes, from their own server.
    if object_id(&note["attributedTo"]) != Some(sender.actor_id.as_str()) || !linker::same_host(ap_id, &sender.actor_id) {
        tracing::info!("refused note {} not written by its sender {}", ap_id, sender.actor_id);
        return Ok(());
    }
    if !is_public(note) {
        tracing::debug!("ignored non-public note {} from {}", ap_id, sender.actor_id);
        return Ok(());
    }

    if let Some(in_reply_to) = note["inReplyTo"].as_str() {
        return receive_reply(federation, pool, sender, note, ap_id, in_reply_to).await;
    }

    let Recipient::Community(community) = recipient else {
        return Ok(());
    };
    // Following the community is joining it, see `COMMUNITY_FOLLOWERS_TABLE`.
    let follows = store::follows_community(pool, community.id, sender)
        .await
        .map_err(|err| format!("failed to check follower: {err}"))?;
    if !follows {
        tracing::info!("refused post of {} in {}: not a follower", sender.actor_id, community.slug);
        return Ok(());
    }
    let text = note_text(note, Some((&community.slug, &federation.domain)));
    if text.is_empty() {
        return Ok(());
    }
    let page = note.get("url").and_then(|url| match url {
        Value::String(url) => Some(url.clone()),
        other => other.get("href").and_then(Value::as_str).map(str::to_string),
    });
    let created = post::create_remote(pool, sender.id, community.id, ap_id, page.as_deref(), &text, published(note))
        .await
        .map_err(|err| format!("failed to save post: {err}"))?;
    let Some(post_id) = created else {
        // Already received.
        return Ok(());
    };
    tracing::info!("published {} from {} in community {}", ap_id, sender.actor_id, community.slug);

    // Shared with the community's followers, like its other posts.
    let inboxes = store::community_follower_inboxes(pool, community.id)
        .await
        .map_err(|err| format!("failed to load community followers: {err}"))?;
    let announce = community_announce(federation, community.public_id, ap_id, post_id);
    send(federation, pool, Signer::Community(community.id), &inboxes, &announce).await
}

/// A reply to a post or comment stored here: saved as a comment.
async fn receive_reply(
    federation: &Federation,
    pool: &PgPool,
    sender: &RemoteActor,
    note: &Value,
    ap_id: &str,
    in_reply_to: &str,
) -> Result<(), String> {
    let Some((post_id, parent_id)) = reply_target(federation, pool, in_reply_to).await else {
        return Ok(());
    };
    // Only on posts everyone can see.
    let visible = post::find(pool, post_id, None)
        .await
        .map_err(|err| format!("failed to load post: {err}"))?;
    if visible.is_none() {
        return Ok(());
    }
    let text = note_text(note, None);
    if text.is_empty() {
        return Ok(());
    }
    let created = comment::create_remote(pool, post_id, sender.id, parent_id, ap_id, &text, published(note))
        .await
        .map_err(|err| format!("failed to save comment: {err}"))?;
    let Some(comment_id) = created else {
        return Ok(());
    };
    let parent_author = match parent_id {
        Some(parent_id) => comment::parent_author(pool, post_id, parent_id)
            .await
            .map_err(|err| format!("failed to load parent comment: {err}"))?
            .flatten(),
        None => None,
    };
    if let Err(err) = notification::remote_comment_added(pool, post_id, comment_id, sender.id, parent_author).await {
        tracing::warn!("failed to notify comment {}: {}", comment_id, err);
    }
    tracing::info!("saved reply {} from {} on post {}", ap_id, sender.actor_id, post_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_text() {
        assert_eq!(
            html_to_text(
                "<p><span class=\"h-card\"><a href=\"https://x\">@<span>cooking</span></a></span> Hello &amp; welcome<br>line two</p><p>Next&#33; &#x1F600; &lt;b&gt;</p>"
            ),
            "@cooking Hello & welcome\nline two\n\nNext! 😀 <b>"
        );
        assert_eq!(html_to_text("a & b &unknown; c"), "a & b &unknown; c");
    }

    #[test]
    fn community_mention_is_removed() {
        assert_eq!(strip_mention("@Cooking@example.com Pasta tonight", "cooking", "example.com"), "Pasta tonight");
        assert_eq!(strip_mention("@cooking\nPasta", "cooking", "example.com"), "Pasta");
        assert_eq!(strip_mention("@cookingclub hi", "cooking", "example.com"), "@cookingclub hi");
        assert_eq!(strip_mention("Pasta @cooking", "cooking", "example.com"), "Pasta @cooking");
    }

    #[test]
    fn only_public_notes() {
        assert!(is_public(&json!({ "to": [PUBLIC] })));
        // Misskey's "home": public in cc.
        assert!(is_public(&json!({ "to": ["https://m.example/users/a/followers"], "cc": [PUBLIC] })));
        assert!(!is_public(&json!({ "to": ["https://m.example/users/a/followers"] })));
        assert!(!is_public(&json!({ "to": ["https://b.example/ap/communities/1"] })));
    }

    #[test]
    fn misskey_text_is_preferred() {
        let note = json!({
            "content": "<p>html</p>",
            "_misskey_content": "@cooking@example.com **pasta**",
            "summary": "food",
        });
        assert_eq!(note_text(&note, None), "⚠ food\n\n@cooking@example.com **pasta**");
        assert_eq!(note_text(&note, Some(("cooking", "example.com"))), "⚠ food\n\n**pasta**");
    }
}
