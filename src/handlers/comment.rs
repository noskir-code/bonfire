use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use tower_sessions::Session;

use crate::{
    AppState,
    entities::{
        comment::{self, BODY_MAX_CHARS, Thread},
        community,
        notification,
        post::{self, BodyError, Post},
    },
    handlers::{
        notifier,
        post::EditTemplate,
        system::{Layout, error_page, render_template},
        user::signed_in_user,
    },
};

/// Errors sent back to the post page as `?error=<code>`.
#[derive(Clone, Copy)]
enum CommentError {
    Empty,
    TooLong,
    InvalidCharacters,
    /// The comment replied to doesn't exist, or is on another post.
    ReplyTarget,
    Server,
}

impl CommentError {
    const ALL: [Self; 5] = [
        Self::Empty,
        Self::TooLong,
        Self::InvalidCharacters,
        Self::ReplyTarget,
        Self::Server,
    ];

    fn code(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::InvalidCharacters => "invalid-characters",
            Self::ReplyTarget => "reply-target",
            Self::Server => "server",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|error| error.code() == code)
    }

    fn message(self) -> String {
        match self {
            Self::Empty => "Write something before commenting.".to_string(),
            Self::TooLong => format!("A comment can be at most {BODY_MAX_CHARS} characters."),
            Self::InvalidCharacters => "The comment contains characters that are not allowed.".to_string(),
            Self::ReplyTarget => "The comment you reply to doesn't exist anymore.".to_string(),
            Self::Server => "Something went wrong, try again later.".to_string(),
        }
    }
}

impl From<BodyError> for CommentError {
    fn from(err: BodyError) -> Self {
        match err {
            BodyError::Empty => Self::Empty,
            BodyError::TooLong => Self::TooLong,
            BodyError::InvalidCharacters => Self::InvalidCharacters,
        }
    }
}

#[derive(Template)]
#[template(path = "post.html")]
struct PostTemplate {
    title: String,
    layout: Layout,
    post: Post,
    threads: Vec<Thread>,
    /// The comment answered by the form, from `?reply_to=`: the reply link
    /// works without JavaScript.
    reply_to: Option<ReplyTarget>,
    /// Where the like and share buttons come back to.
    return_to: String,
    error: Option<String>,
    body_max_chars: usize,
    /// The post for Fediverse servers, see `templates/base.html`.
    activitypub_url: Option<String>,
    /// The viewer moderates the post's community: shows "Remove".
    can_moderate: bool,
}

struct ReplyTarget {
    id: i64,
    author: String,
}

/// Whether `viewer` may remove posts of the community: its owner and
/// moderators, and the site's staff.
async fn can_moderate(pool: &sqlx::PgPool, community_id: i64, viewer: &crate::entities::user::User) -> bool {
    if viewer.is_staff() {
        return true;
    }
    match community::find_by_id(pool, community_id).await {
        Ok(Some(found)) => community::membership(pool, &found, viewer.id)
            .await
            .is_ok_and(|membership| membership.can_manage()),
        _ => false,
    }
}

fn not_found(layout: Layout) -> Response {
    error_page(layout, StatusCode::NOT_FOUND, "This post does not exist.")
}

/// A post with its comments.
pub async fn post_page(
    State(state): State<AppState>,
    session: Session,
    Path(post_id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    let Some(pool) = &state.pool else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };
    let viewer_id = layout.viewer.as_ref().map(|viewer| viewer.id);

    let loaded = async {
        let Some(post) = post::find(pool, post_id, viewer_id).await? else {
            return Ok(None);
        };
        let comments = comment::for_post(pool, post_id, viewer_id).await?;
        Ok::<_, sqlx::Error>(Some((post, comments)))
    }
    .await;

    let (post, comments) = match loaded {
        Ok(Some(loaded)) => loaded,
        Ok(None) => return not_found(layout),
        Err(err) => {
            tracing::error!("failed to load post {}: {}", post_id, err);
            return error_page(layout, StatusCode::INTERNAL_SERVER_ERROR, "Failed to load this post.");
        }
    };

    let reply_to = query
        .get("reply_to")
        .and_then(|id| id.parse::<i64>().ok())
        .and_then(|id| comments.iter().find(|comment| comment.id == id))
        .map(|comment| ReplyTarget {
            id: comment.id,
            author: comment.author.mention(),
        });
    let activitypub_url = state
        .federation
        .as_ref()
        .filter(|_| {
            !post.is_private()
                && post.community.as_ref().is_none_or(|community| community.is_public)
                && post.author.local().is_some_and(|author| author.federation_enabled)
        })
        .map(|federation| federation.post_object_url(post.id));
    let can_moderate = match (&post.community, &layout.viewer) {
        (Some(community), Some(viewer)) => can_moderate(pool, community.id, viewer).await,
        _ => false,
    };
    render_template(&PostTemplate {
        title: format!("Post by {}", post.author.handle()),
        can_moderate,
        activitypub_url,
        layout,
        return_to: format!("/posts/{post_id}"),
        post,
        threads: comment::threads(comments),
        reply_to,
        error: query
            .get("error")
            .and_then(|code| CommentError::from_code(code))
            .map(CommentError::message),
        body_max_chars: BODY_MAX_CHARS,
    })
}

pub async fn create_comment(
    State(state): State<AppState>,
    session: Session,
    Path(post_id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };

    // Only posts the author can see can be commented.
    match post::find(&pool, post_id, Some(me.id)).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(Layout::load(&state, &session).await),
        Err(err) => {
            tracing::error!("failed to load post {}: {}", post_id, err);
            return Redirect::to(&format!("/posts/{post_id}?error=server#comment-form")).into_response();
        }
    }

    let result = async {
        let body = comment::validate_body(form.get("body").map(String::as_str).unwrap_or(""))?;
        let server_error = |err: sqlx::Error| {
            tracing::error!("failed to create comment on post {}: {}", post_id, err);
            CommentError::Server
        };
        // Empty when commenting the post itself.
        let parent = match form.get("parent_id").map(|id| id.trim()).filter(|id| !id.is_empty()) {
            None => None,
            Some(id) => {
                let id = id.parse::<i64>().map_err(|_| CommentError::ReplyTarget)?;
                // `None` inside for a comment from another server: nobody here to notify.
                let author = comment::parent_author(&pool, post_id, id)
                    .await
                    .map_err(server_error)?
                    .ok_or(CommentError::ReplyTarget)?;
                Some((id, author))
            }
        };
        let comment_id = comment::create(&pool, post_id, me.id, parent.map(|(id, _)| id), &body)
            .await
            .map_err(server_error)?;
        Ok::<_, CommentError>((comment_id, parent.and_then(|(_, author)| author)))
    }
    .await;

    if let Ok((comment_id, parent_author)) = &result {
        if let Err(err) = notification::comment_added(&pool, post_id, *comment_id, me.id, *parent_author).await {
            tracing::warn!("failed to notify comment {}: {}", comment_id, err);
        }
        notifier::comment_created(&state, &pool, *comment_id).await;
    }
    let location = match result {
        Ok((comment_id, _)) => format!("/posts/{post_id}#comment-{comment_id}"),
        Err(error) => format!("/posts/{post_id}?error={}#comment-form", error.code()),
    };
    Redirect::to(&location).into_response()
}

/// Likes or unlikes a comment, then goes back to it.
pub async fn like_comment(State(state): State<AppState>, session: Session, Path(comment_id): Path<i64>) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let failed = |err: sqlx::Error| {
        tracing::error!("failed to like comment {}: {}", comment_id, err);
        StatusCode::INTERNAL_SERVER_ERROR
    };
    let result = async {
        // Only comments of posts the user can see.
        let Some(post_id) = comment::post_of(&pool, comment_id).await.map_err(failed)? else {
            return Err(StatusCode::NOT_FOUND);
        };
        if post::find(&pool, post_id, Some(me.id)).await.map_err(failed)?.is_none() {
            return Err(StatusCode::NOT_FOUND);
        }
        let toggled = comment::toggle_like(&pool, comment_id, me.id).await.map_err(failed)?;
        Ok((post_id, toggled))
    }
    .await;

    match result {
        Ok((post_id, toggled)) => {
            let notified = if toggled.active {
                notification::comment_like_added(&pool, comment_id, me.id).await
            } else {
                notification::comment_like_removed(&pool, comment_id, me.id).await
            };
            if let Err(err) = notified {
                tracing::warn!("failed to update notification of like on comment {}: {}", comment_id, err);
            }
            notifier::comment_like_changed(&state, &pool, &me, comment_id, toggled).await;
            Redirect::to(&format!("/posts/{post_id}#comment-{comment_id}")).into_response()
        }
        Err(status) => {
            let layout = Layout::load(&state, &session).await;
            let message = if status == StatusCode::NOT_FOUND {
                "This comment does not exist."
            } else {
                "Something went wrong, try again later."
            };
            error_page(layout, status, message)
        }
    }
}

/// The comment's post, if `me` can see it, and whether `me` wrote the
/// comment (only then can they edit it) or may remove it (the post's author,
/// the community's moderators, the site's staff).
struct CommentRights {
    post_id: i64,
    is_author: bool,
    can_remove: bool,
}

async fn comment_rights(pool: &sqlx::PgPool, comment_id: i64, me: &crate::entities::user::User) -> Result<Option<CommentRights>, sqlx::Error> {
    let Some(owners) = comment::owners(pool, comment_id).await? else {
        return Ok(None);
    };
    let Some(post) = post::find(pool, owners.post_id, Some(me.id)).await? else {
        return Ok(None);
    };
    let is_author = owners.author_id == Some(me.id);
    let moderates = match &post.community {
        Some(community) => can_moderate(pool, community.id, me).await,
        None => me.is_staff(),
    };
    Ok(Some(CommentRights {
        post_id: owners.post_id,
        is_author,
        can_remove: is_author || post.is_own || moderates,
    }))
}

fn comment_not_found(layout: Layout) -> Response {
    error_page(layout, StatusCode::NOT_FOUND, "This comment does not exist or isn't yours.")
}

fn comment_failed(layout: Layout, comment_id: i64, err: sqlx::Error) -> Response {
    tracing::error!("failed to change comment {}: {}", comment_id, err);
    error_page(layout, StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong, try again later.")
}

pub async fn edit_page(
    State(state): State<AppState>,
    session: Session,
    Path(comment_id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let layout = Layout::load(&state, &session).await;
    let loaded = async {
        let Some(rights) = comment_rights(&pool, comment_id, &me).await?.filter(|rights| rights.is_author) else {
            return Ok(None);
        };
        Ok(comment::body_of(&pool, comment_id).await?.map(|body| (rights, body)))
    }
    .await;
    let (rights, body) = match loaded {
        Ok(Some(found)) => found,
        Ok(None) => return comment_not_found(layout),
        Err(err) => return comment_failed(layout, comment_id, err),
    };
    render_template(&EditTemplate {
        title: "Edit the comment".to_string(),
        layout,
        action: format!("/comments/{comment_id}/edit"),
        back: format!("/posts/{}#comment-{comment_id}", rights.post_id),
        body,
        body_max_chars: BODY_MAX_CHARS,
        error: query
            .get("error")
            .and_then(|code| CommentError::from_code(code))
            .map(CommentError::message),
        federated: state.federation.is_some(),
    })
}

pub async fn update_comment(
    State(state): State<AppState>,
    session: Session,
    Path(comment_id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let rights = match comment_rights(&pool, comment_id, &me).await {
        Ok(Some(rights)) if rights.is_author => rights,
        Ok(_) => return comment_not_found(Layout::load(&state, &session).await),
        Err(err) => return comment_failed(Layout::load(&state, &session).await, comment_id, err),
    };
    let body = match comment::validate_body(form.get("body").map(String::as_str).unwrap_or("")) {
        Ok(body) => body,
        Err(err) => {
            let error = CommentError::from(err);
            return Redirect::to(&format!("/comments/{comment_id}/edit?error={}", error.code())).into_response();
        }
    };
    if let Err(err) = comment::update_own(&pool, comment_id, me.id, &body).await {
        return comment_failed(Layout::load(&state, &session).await, comment_id, err);
    }
    notifier::comment_updated(&state, &pool, comment_id).await;
    Redirect::to(&format!("/posts/{}#comment-{comment_id}", rights.post_id)).into_response()
}

/// Deletes a comment with its replies: by its author, or removed by the
/// post's author or a moderator.
pub async fn delete_comment(State(state): State<AppState>, session: Session, Path(comment_id): Path<i64>) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let rights = match comment_rights(&pool, comment_id, &me).await {
        Ok(Some(rights)) if rights.can_remove => rights,
        Ok(_) => return comment_not_found(Layout::load(&state, &session).await),
        Err(err) => return comment_failed(Layout::load(&state, &session).await, comment_id, err),
    };
    let deletions = notifier::comment_deleting(&state, &pool, comment_id).await;
    if let Err(err) = comment::delete(&pool, comment_id).await {
        return comment_failed(Layout::load(&state, &session).await, comment_id, err);
    }
    deletions.send(&state, &pool).await;
    tracing::info!(
        "@{} {} comment {}",
        me.username,
        if rights.is_author { "deleted" } else { "removed" },
        comment_id
    );
    Redirect::to(&format!("/posts/{}#comments", rights.post_id)).into_response()
}
