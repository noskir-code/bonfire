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
        comment::{self, BODY_MAX_CHARS, Comment},
        post::{self, BodyError, Post},
    },
    handlers::{
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
    Server,
}

impl CommentError {
    const ALL: [Self; 4] = [Self::Empty, Self::TooLong, Self::InvalidCharacters, Self::Server];

    fn code(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::InvalidCharacters => "invalid-characters",
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
    comments: Vec<Comment>,
    /// Where the like and share buttons come back to.
    return_to: String,
    error: Option<String>,
    body_max_chars: usize,
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
        let comments = comment::for_post(pool, post_id).await?;
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

    render_template(&PostTemplate {
        title: format!("Post by @{}", post.author.username),
        layout,
        return_to: format!("/posts/{post_id}"),
        post,
        comments,
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
        comment::create(&pool, post_id, me.id, &body)
            .await
            .map_err(|err| {
                tracing::error!("failed to create comment on post {}: {}", post_id, err);
                CommentError::Server
            })
    }
    .await;

    let location = match result {
        Ok(comment_id) => format!("/posts/{post_id}#comment-{comment_id}"),
        Err(error) => format!("/posts/{post_id}?error={}#comment-form", error.code()),
    };
    Redirect::to(&location).into_response()
}
