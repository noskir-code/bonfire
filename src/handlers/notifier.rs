//! Tells the Fediverse what local users do: new posts, comments, likes and
//! shares are queued for the followers on other servers (see `API::linker`
//! for the sending). Private posts, posts of private communities, and
//! everything on them, stay here.
//!
//! In a public community, a post also goes to the community's followers,
//! shared (Announce) by the community. A reaction or reply to a post of
//! another server goes to its author there.
//!
//! Never fails the user's action: a problem is only logged, the post or
//! comment is saved anyway.

use sqlx::PgPool;

use crate::{
    AppState,
    API::{Federation, activities, federation::in_reply_to},
    entities::{
        author::Author,
        comment,
        federation::{self as store, Signer},
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

/// Queues `activity`, signed by `signer`, for `audience`.
async fn queue(federation: &Federation, pool: &PgPool, signer: Signer, audience: &Audience, activity: &serde_json::Value) {
    let inboxes = match audience.inboxes(pool).await {
        Ok(inboxes) => inboxes,
        Err(err) => {
            tracing::warn!("failed to load Fediverse followers: {}", err);
            return;
        }
    };
    if inboxes.is_empty() {
        return;
    }
    match store::enqueue(pool, signer, &inboxes, &activity.to_string()).await {
        Ok(()) => federation.wake_deliveries(),
        Err(err) => tracing::warn!("failed to queue Fediverse activity: {}", err),
    }
}

/// The post if it can go to the Fediverse: visible to visitors (public, not
/// in a private or banned community), and if written here, by a federated user.
async fn public_post(pool: &PgPool, post_id: i64) -> Option<Post> {
    match post::find(pool, post_id, None).await {
        Ok(post) => post.filter(|post| post.author.local().is_none_or(|author| author.federation_enabled)),
        Err(err) => {
            tracing::warn!("failed to load post {} for the Fediverse: {}", post_id, err);
            None
        }
    }
}

pub async fn post_created(state: &AppState, pool: &PgPool, post_id: i64) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    let Some(post) = public_post(pool, post_id).await else {
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

/// Sent to the commenter's followers and those of the post's author (who
/// have the post), the community's followers, and the authors on other
/// servers of the post and of the comment answered.
pub async fn comment_created(state: &AppState, pool: &PgPool, comment_id: i64) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    let (comment, post_id) = match comment::find_public(pool, comment_id).await {
        Ok(Some(found)) => found,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!("failed to load comment {} for the Fediverse: {}", comment_id, err);
            return;
        }
    };
    let Some(commenter) = comment.author.local().filter(|author| author.federation_enabled) else {
        return;
    };
    let Some(post) = public_post(pool, post_id).await else {
        return;
    };
    let in_reply_to = match in_reply_to(federation, pool, &comment, &post).await {
        Ok(in_reply_to) => in_reply_to,
        Err(err) => {
            tracing::warn!("failed to load the comment answered by {}: {}", comment_id, err);
            return;
        }
    };
    let Some(note) = activities::comment_note(federation, &comment, &post, &in_reply_to) else {
        return;
    };

    let mut audience = Audience {
        users: vec![commenter.id],
        ..Audience::default()
    };
    audience.add_author(&post.author);
    if let Some(community) = &post.community {
        audience.communities.push(community.id);
    }
    if let Some(parent_id) = comment.parent_id
        && let Ok(Some((parent, _))) = comment::find_public(pool, parent_id).await
    {
        audience.add_author(&parent.author);
    }
    queue(federation, pool, Signer::User(commenter.id), &audience, &activities::create(&note)).await;
}

/// A like of a comment added or removed by `user`: sent to the liker's
/// followers, and to the comment's author (their followers if they're here).
pub async fn comment_like_changed(state: &AppState, pool: &PgPool, user: &User, comment_id: i64, toggled: Toggled) {
    let Some(federation) = state.federation.as_deref() else {
        return;
    };
    if !user.federation_enabled {
        return;
    }
    // Only comments of posts everyone can see.
    let comment = match comment::find_public(pool, comment_id).await {
        Ok(Some((comment, _))) => comment,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!("failed to load comment {} for the Fediverse: {}", comment_id, err);
            return;
        }
    };
    if comment.author.local().is_some_and(|author| !author.federation_enabled) {
        return;
    }
    let mut audience = Audience {
        users: vec![user.id],
        ..Audience::default()
    };
    audience.add_author(&comment.author);
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
    if !user.federation_enabled {
        return;
    }
    let Some(post) = public_post(pool, post_id).await else {
        return;
    };
    let mut audience = Audience {
        users: vec![user.id],
        ..Audience::default()
    };
    let activity = match reaction {
        // Likes go where the post is known: its author (their followers if
        // they're here), and the liker's followers.
        Reaction::Like => {
            audience.add_author(&post.author);
            activities::like(federation, user, &post, &toggled.reacted_at)
        }
        // A share appears on the sharer's profile, for their followers; a
        // remote author is told, like Mastodon tells about boosts.
        Reaction::Share => {
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
