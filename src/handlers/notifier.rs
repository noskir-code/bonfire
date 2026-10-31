//! Tells the Fediverse what local users do: new and edited posts and
//! comments, deletions, likes, shares and follows are queued for the servers
//! concerned (see `API::linker` for the sending). Private posts, posts of
//! private communities, and everything on them, stay here.
//!
//! In a public community, a post also goes to the community's followers,
//! shared (Announce) by the community; a removed post is unshared. A reaction
//! or reply to a post of another server goes to its author there. On a
//! followers-only post of another server, only its author is told.
//!
//! Never fails the user's action: a problem is only logged, the post or
//! comment is saved anyway.

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    AppState,
    API::{Federation, activities, federation::in_reply_to, linker},
    entities::{
        author::Author,
        comment::{self, Comment},
        community::{self, Community},
        federation::{self as store, RemoteActor, Signer},
        message,
        post::{self, Post, Reaction, Toggled},
        user::User,
    },
};

/// Who an activity goes to: followers of local users or communities, and
/// remote authors.
#[derive(Default)]
struct Audience {
    users: Vec<i32>,
    communities: Vec<i64>,
    /// ActivityPub ids of remote actors, reached at their own inbox.
    remote_actors: Vec<String>,
}

impl Audience {
    /// The author's followers, or the author if they're on another server.
    fn add_author(&mut self, author: &Author) {
        match author {
            Author::Local(user) => self.users.push(user.id),
            Author::Remote(remote) => self.remote_actors.push(remote.actor_id.clone()),
        }
    }

    async fn inboxes(&self, pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
        let mut inboxes = store::follower_inboxes(pool, &self.users).await?;
        for community_id in &self.communities {
            inboxes.extend(store::community_follower_inboxes(pool, *community_id).await?);
        }
        for actor_id in &self.remote_actors {
            inboxes.extend(store::actor_inbox(pool, actor_id).await?);
        }
        inboxes.sort();
        inboxes.dedup();
        Ok(inboxes)
    }
}

/// Queues `activity`, signed by `signer`, for `inboxes`.
async fn queue_to(federation: &Federation, pool: &PgPool, signer: Signer, inboxes: &[String], activity: &Value) {
    if inboxes.is_empty() {
        return;
    }
    match store::enqueue(pool, signer, inboxes, &activity.to_string()).await {
        Ok(()) => federation.wake_deliveries(),
        Err(err) => tracing::warn!("failed to queue Fediverse activity: {}", err),
    }
}

/// Queues `activity`, signed by `signer`, for `audience`.
async fn queue(federation: &Federation, pool: &PgPool, signer: Signer, audience: &Audience, activity: &Value) {
    match audience.inboxes(pool).await {
        Ok(inboxes) => queue_to(federation, pool, signer, &inboxes, activity).await,
        Err(err) => tracing::warn!("failed to load Fediverse followers: {}", err),
    }
}

/// The post if it can go to the Fediverse: visible to visitors (public, not
/// in a private or banned community) or followers-only from another server,
/// and if written here, by a federated user.
async fn federable_post(pool: &PgPool, post_id: i64) -> Option<Post> {
    match post::find_federable(pool, post_id).await {
        Ok(post) => post.filter(|post| post.author.local().is_none_or(|author| author.is_federated())),
        Err(err) => {
            tracing::warn!("failed to load post {} for the Fediverse: {}", post_id, err);
            None
        }
    }
}

/// A comment and its post if they can go to the Fediverse, see `federable_post`.
async fn federable_comment(pool: &PgPool, comment_id: i64) -> Option<(Comment, Post)> {
    let (comment, post_id) = match comment::find_any(pool, comment_id).await {
        Ok(Some(found)) => found,
        Ok(None) => return None,
        Err(err) => {
            tracing::warn!("failed to load comment {} for the Fediverse: {}", comment_id, err);
            return None;
        }
    };
    let post = federable_post(pool, post_id).await?;
    Some((comment, post))
}

/// Where a post written here goes: its author's followers, the community's
/// followers, and the remote accounts that commented on it.
async fn post_audience(pool: &PgPool, post: &Post, author: &User) -> Audience {
    let mut audience = Audience {
        users: vec![author.id],
        ..Audience::default()
    };
    if let Some(community) = &post.community {
        audience.communities.push(community.id);
    }
    match store::commenters_of(pool, post.id).await {
        Ok(commenters) => audience.remote_actors.extend(commenters),
        Err(err) => tracing::warn!("failed to load commenters of post {}: {}", post.id, err),
    }
    audience
}

/// Where a comment written here goes: the commenter's followers and those of
/// the post's author (who have the post), the community's followers, and the
/// authors on other servers of the post and of the comment answered. On a
/// followers-only post, only the authors.
async fn comment_audience(pool: &PgPool, comment: &Comment, commenter: &User, post: &Post) -> Audience {
    let mut audience = Audience::default();
    if !post.is_followers_only() {
        audience.users.push(commenter.id);
        if let Some(community) = &post.community {
            audience.communities.push(community.id);
        }
    }
    audience.add_author(&post.author);
    if let Some(parent_id) = comment.parent_id
        && let Ok(Some((parent, _))) = comment::find_any(pool, parent_id).await
    {
        if post.is_followers_only() {
            if let Author::Remote(remote) = &parent.author {
                audience.remote_actors.push(remote.actor_id.clone());
            }
        } else {
            audience.add_author(&parent.author);
        }
    }
    audience
}

/// The Note of a comment written here, with the Note it answers.
async fn comment_note(federation: &Federation, pool: &PgPool, comment: &Comment, post: &Post) -> Option<Value> {
    let in_reply_to = match in_reply_to(federation, pool, comment, post).await {
        Ok(in_reply_to) => in_reply_to,
        Err(err) => {
            tracing::warn!("failed to load the comment answered by {}: {}", comment.id, err);
            return None;
        }
    };
    activities::comment_note(federation, comment, post, &in_reply_to)
}

pub async fn post_created(state: &AppState, pool: &PgPool, post_id: i64) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    let Some(post) = federable_post(pool, post_id).await else {
        return;
    };
    let (Some(author), Some(note)) = (post.author.local(), activities::post_note(federation, &post)) else {
        return;
    };
    let audience = Audience {
        users: vec![author.id],
        ..Audience::default()
    };
    queue(federation, pool, Signer::User(author.id), &audience, &activities::create(&note)).await;

    // In a community: also shared by it with its followers.
    if let Some(community) = &post.community {
        let audience = Audience {
            communities: vec![community.id],
            ..Audience::default()
        };
        let announce = activities::community_announce(federation, community.public_id, &activities::note_id(federation, &post), post.id);
        queue(federation, pool, Signer::Community(community.id), &audience, &announce).await;
    }
}

/// An edited post goes where the post went.
pub async fn post_updated(state: &AppState, pool: &PgPool, post_id: i64) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    let Some(post) = federable_post(pool, post_id).await else {
        return;
    };
    let (Some(author), Some(note)) = (post.author.local(), activities::post_note(federation, &post)) else {
        return;
    };
    let audience = post_audience(pool, &post, author).await;
    queue(federation, pool, Signer::User(author.id), &audience, &activities::update(&note)).await;
}

pub async fn comment_created(state: &AppState, pool: &PgPool, comment_id: i64) {
    comment_sent(state, pool, comment_id, false).await;
}

pub async fn comment_updated(state: &AppState, pool: &PgPool, comment_id: i64) {
    comment_sent(state, pool, comment_id, true).await;
}

/// A comment written here, new or `edited`, see `comment_audience`.
async fn comment_sent(state: &AppState, pool: &PgPool, comment_id: i64, edited: bool) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    let Some((comment, post)) = federable_comment(pool, comment_id).await else {
        return;
    };
    let Some(commenter) = comment.author.local().filter(|author| author.is_federated()) else {
        return;
    };
    let Some(note) = comment_note(federation, pool, &comment, &post).await else {
        return;
    };
    let audience = comment_audience(pool, &comment, commenter, &post).await;
    let activity = if edited { activities::update(&note) } else { activities::create(&note) };
    queue(federation, pool, Signer::User(commenter.id), &audience, &activity).await;
}

/// Activities telling other servers that something is gone. Prepared while it
/// still exists (the addresses are read from it), sent once it's deleted.
#[derive(Default)]
#[must_use]
pub struct Deletions {
    activities: Vec<(Signer, Vec<String>, Value)>,
}

impl Deletions {
    async fn add(&mut self, pool: &PgPool, signer: Signer, audience: &Audience, activity: Value) {
        match audience.inboxes(pool).await {
            Ok(inboxes) if !inboxes.is_empty() => self.activities.push((signer, inboxes, activity)),
            Ok(_) => {}
            Err(err) => tracing::warn!("failed to load Fediverse followers: {}", err),
        }
    }

    fn extend(&mut self, other: Deletions) {
        self.activities.extend(other.activities);
    }

    /// Call once the rows are deleted.
    pub async fn send(self, state: &AppState, pool: &PgPool) {
        let Some(federation) = state.federation.as_deref() else {
            return;
        };
        for (signer, inboxes, activity) in self.activities {
            queue_to(federation, pool, signer, &inboxes, &activity).await;
        }
    }
}

/// A post about to be deleted, by its author or a moderator: its Note is
/// deleted (if written here), the community unshares it, and the comments
/// written here on it are deleted too.
pub async fn post_deleting(state: &AppState, pool: &PgPool, post_id: i64) -> Deletions {
    post_deletions(state, pool, post_id, true).await
}

/// `unshare` is `false` when the community goes too: it can't sign anymore.
async fn post_deletions(state: &AppState, pool: &PgPool, post_id: i64, unshare: bool) -> Deletions {
    let mut deletions = Deletions::default();
    let Some(federation) = state.federation.as_deref() else {
        return deletions;
    };
    let Some(post) = federable_post(pool, post_id).await else {
        return deletions;
    };
    match comment::local_ids_of_post(pool, post_id).await {
        Ok(comment_ids) => {
            for comment_id in comment_ids {
                deletions.extend(comment_deletions(federation, pool, comment_id).await);
            }
        }
        Err(err) => tracing::warn!("failed to load comments of post {}: {}", post_id, err),
    }
    if let (Some(author), Some(note)) = (post.author.local(), activities::post_note(federation, &post)) {
        let audience = post_audience(pool, &post, author).await;
        let delete = activities::delete(&federation.actor_url(author.public_id), &activities::note_id(federation, &post), &note["to"], &note["cc"]);
        deletions.add(pool, Signer::User(author.id), &audience, delete).await;
    }
    if unshare && let Some(community) = &post.community {
        let audience = Audience {
            communities: vec![community.id],
            ..Audience::default()
        };
        let announce = activities::community_announce(federation, community.public_id, &activities::note_id(federation, &post), post.id);
        deletions.add(pool, Signer::Community(community.id), &audience, activities::undo(&announce)).await;
    }
    deletions
}

/// A comment about to be deleted, with the replies below it: the Notes of
/// those written here are deleted.
pub async fn comment_deleting(state: &AppState, pool: &PgPool, comment_id: i64) -> Deletions {
    let mut deletions = Deletions::default();
    let Some(federation) = state.federation.as_deref() else {
        return deletions;
    };
    match comment::local_thread(pool, comment_id).await {
        Ok(comment_ids) => {
            for comment_id in comment_ids {
                deletions.extend(comment_deletions(federation, pool, comment_id).await);
            }
        }
        Err(err) => tracing::warn!("failed to load replies of comment {}: {}", comment_id, err),
    }
    deletions
}

/// The Delete of one comment written here, sent where the comment went.
async fn comment_deletions(federation: &Federation, pool: &PgPool, comment_id: i64) -> Deletions {
    let mut deletions = Deletions::default();
    let Some((comment, post)) = federable_comment(pool, comment_id).await else {
        return deletions;
    };
    let Some(commenter) = comment.author.local().filter(|author| author.is_federated()) else {
        return deletions;
    };
    let Some(note) = comment_note(federation, pool, &comment, &post).await else {
        return deletions;
    };
    let audience = comment_audience(pool, &comment, commenter, &post).await;
    let delete = activities::delete(
        &federation.actor_url(commenter.public_id),
        &activities::comment_note_id(federation, &comment),
        &note["to"],
        &note["cc"],
    );
    deletions.add(pool, Signer::User(commenter.id), &audience, delete).await;
    deletions
}

/// A public community becomes private, or is closed by the staff: its posts
/// are unshared, and its followers on other servers are removed (Reject of
/// their Follow), so nothing reaches them anymore.
pub async fn community_left_fediverse(state: &AppState, pool: &PgPool, community: &Community) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    let signer = Signer::Community(community.id);
    let inboxes = match store::community_follower_inboxes(pool, community.id).await {
        Ok(inboxes) => inboxes,
        Err(err) => {
            tracing::warn!("failed to load followers of {}: {}", community.slug, err);
            return;
        }
    };
    if !inboxes.is_empty() {
        match community::posts(pool, community.id).await {
            Ok(posts) => {
                for post in posts {
                    let note_id = post.ap_id.unwrap_or_else(|| federation.post_object_url(post.id));
                    let announce = activities::community_announce(federation, community.public_id, &note_id, post.id);
                    queue_to(federation, pool, signer, &inboxes, &activities::undo(&announce)).await;
                }
            }
            Err(err) => tracing::warn!("failed to load posts of {}: {}", community.slug, err),
        }
    }
    let group = federation.community_url(community.public_id);
    match store::community_follows(pool, community.id).await {
        Ok(follows) => {
            for (follower, inbox, follow_id) in follows {
                let reject = activities::reject_follow(&group, &follower, &follow_id);
                queue_to(federation, pool, signer, std::slice::from_ref(&inbox), &reject).await;
            }
        }
        Err(err) => tracing::warn!("failed to load followers of {}: {}", community.slug, err),
    }
    if let Err(err) = store::remove_community_followers(pool, community.id).await {
        tracing::warn!("failed to remove followers of {}: {}", community.slug, err);
    }
    tracing::info!("community {} left the Fediverse", community.slug);
}

/// A community about to be deleted: the Notes of its posts written here are
/// deleted, and its Group too.
///
/// The Group's Delete is sent at once, not queued: queued deliveries signed by
/// the community are deleted with it. Returns what to send after the deletion.
pub async fn community_deleting(state: &AppState, pool: &PgPool, community: &Community) -> Deletions {
    let mut deletions = Deletions::default();
    let Some(federation) = state.federation.clone() else {
        return deletions;
    };
    if !community.is_federated() {
        return deletions;
    }
    match community::posts(pool, community.id).await {
        Ok(posts) => {
            for post in posts.iter().filter(|post| post.is_local) {
                deletions.extend(post_deletions(state, pool, post.id, false).await);
            }
        }
        Err(err) => tracing::warn!("failed to load posts of {}: {}", community.slug, err),
    }
    let inboxes = store::community_follower_inboxes(pool, community.id).await.unwrap_or_default();
    if inboxes.is_empty() {
        return deletions;
    }
    let key = match linker::local_key(&federation, pool, Signer::Community(community.id)).await {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!("failed to load key of {}: {}", community.slug, err);
            return deletions;
        }
    };
    let delete = activities::delete_group(&federation, community).to_string();
    let slug = community.slug.clone();
    tokio::spawn(async move {
        for inbox in inboxes {
            if let Err(linker::SendError::Retry(err) | linker::SendError::GiveUp(err)) = linker::send(&federation, &key, &inbox, &delete).await {
                tracing::info!("failed to tell {} that community {} was deleted: {}", inbox, slug, err);
            }
        }
    });
    deletions
}

/// A local user follows an account of another server. The follow waits for
/// its Accept, see `API::activities::receive`.
pub async fn follow(federation: &Federation, pool: &PgPool, user: &User, actor: &RemoteActor) -> Result<(), String> {
    let follow_id = federation.follow_url(user.public_id, actor.id);
    store::add_following(pool, user.id, actor.id, &follow_id)
        .await
        .map_err(|err| format!("failed to save follow: {err}"))?;
    let follow = activities::follow(federation, user, &actor.actor_id, &follow_id);
    queue_to(federation, pool, Signer::User(user.id), std::slice::from_ref(&actor.inbox), &follow).await;
    Ok(())
}

pub async fn unfollow(federation: &Federation, pool: &PgPool, user: &User, actor: &RemoteActor) -> Result<(), String> {
    let removed = store::remove_following(pool, user.id, actor.id)
        .await
        .map_err(|err| format!("failed to remove follow: {err}"))?;
    if let Some(follow_id) = removed {
        let follow = activities::follow(federation, user, &actor.actor_id, &follow_id);
        let mut undo = activities::undo(&follow);
        undo["to"] = json!([actor.actor_id]);
        queue_to(federation, pool, Signer::User(user.id), std::slice::from_ref(&actor.inbox), &undo).await;
    }
    Ok(())
}

/// A private message written here to an account of another server: sent to
/// its own inbox only (see `activities::direct_message`).
pub async fn message_sent(
    state: &AppState,
    pool: &PgPool,
    sender: &User,
    conversation: &message::Conversation,
    message_id: i64,
    body: &str,
    sent_at: &sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>,
) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    let Some(to) = message::remote_of(conversation) else {
        return;
    };
    let Some(inbox) = remote_inbox(pool, &to.actor_id).await else {
        return;
    };
    let activity = activities::direct_message(federation, sender, to, message_id, body, sent_at, conversation.remote_kind == "chat");
    queue_to(federation, pool, Signer::User(sender.id), &[inbox], &activity).await;
}

/// A private message deleted by its author: the other server deletes it too.
pub async fn message_deleted(state: &AppState, pool: &PgPool, sender: &User, conversation: &message::Conversation, message_id: i64) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    let Some(to) = message::remote_of(conversation) else {
        return;
    };
    let Some(inbox) = remote_inbox(pool, &to.actor_id).await else {
        return;
    };
    let delete = activities::delete(
        &federation.actor_url(sender.public_id),
        &federation.message_object_url(message_id),
        &json!([to.actor_id]),
        &json!([]),
    );
    queue_to(federation, pool, Signer::User(sender.id), &[inbox], &delete).await;
}

/// The personal inbox of an account: messages aren't for its whole server.
async fn remote_inbox(pool: &PgPool, actor_id: &str) -> Option<String> {
    match store::find_actor(pool, actor_id).await {
        Ok(found) => found.map(|(actor, _)| actor.inbox),
        Err(err) => {
            tracing::warn!("failed to load actor {}: {}", actor_id, err);
            None
        }
    }
}

/// A like of a comment added or removed by `user`: sent to the liker's
/// followers, and to the comment's author (their followers if they're here).
pub async fn comment_like_changed(state: &AppState, pool: &PgPool, user: &User, comment_id: i64, toggled: Toggled) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    if !user.is_federated() {
        return;
    }
    let Some((comment, post)) = federable_comment(pool, comment_id).await else {
        return;
    };
    if comment.author.local().is_some_and(|author| !author.is_federated()) {
        return;
    }
    let mut audience = Audience::default();
    if post.is_followers_only() {
        if let Author::Remote(remote) = &comment.author {
            audience.remote_actors.push(remote.actor_id.clone());
        }
    } else {
        audience.users.push(user.id);
        audience.add_author(&comment.author);
    }
    let like = activities::like_comment(federation, user, &comment, &toggled.reacted_at);
    let activity = if toggled.active {
        activities::with_context(like)
    } else {
        activities::undo(&like)
    };
    queue(federation, pool, Signer::User(user.id), &audience, &activity).await;
}

/// A like or share added or removed by `user`.
pub async fn reaction_changed(state: &AppState, pool: &PgPool, user: &User, post_id: i64, reaction: Reaction, toggled: Toggled) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    if !user.is_federated() {
        return;
    }
    let Some(post) = federable_post(pool, post_id).await else {
        return;
    };
    let mut audience = Audience::default();
    let activity = match reaction {
        // Likes go where the post is known: its author (their followers if
        // they're here), and the liker's followers. Only to the author for a
        // followers-only post.
        Reaction::Like => {
            if !post.is_followers_only() {
                audience.users.push(user.id);
            }
            audience.add_author(&post.author);
            activities::like(federation, user, &post, &toggled.reacted_at)
        }
        // A share appears on the sharer's profile, for their followers; a
        // remote author is told, like Mastodon tells about boosts.
        Reaction::Share => {
            audience.users.push(user.id);
            if let Author::Remote(remote) = &post.author {
                audience.remote_actors.push(remote.actor_id.clone());
            }
            activities::announce(federation, user, &post, &toggled.reacted_at)
        }
    };
    let activity = if toggled.active {
        activities::with_context(activity)
    } else {
        activities::undo(&activity)
    };
    queue(federation, pool, Signer::User(user.id), &audience, &activity).await;
}
