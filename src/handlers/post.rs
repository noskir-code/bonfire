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
    entities::post::{self, BODY_MAX_CHARS, BodyError, Post, Visibility},
    handlers::{
        notifier,
        system::{Layout, error_page, local_path, render_template},
        user::signed_in_user,
    },
};

/// Pages with the post form. Sent back by the form so the author returns
/// where they posted from, only to these fixed URLs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PostPage {
    Home,
    Feed,
}

impl PostPage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Feed => "feed",
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::Home => "/",
            Self::Feed => "/your-feed",
        }
    }

    fn parse(value: &str) -> Self {
        if value == Self::Feed.as_str() {
            Self::Feed
        } else {
            Self::Home
        }
    }
}

/// Errors sent back to the page as `?error=<code>`.
#[derive(Clone, Copy)]
enum PostError {
    Empty,
    TooLong,
    InvalidCharacters,
    InvalidVisibility,
    Server,
}

impl PostError {
    const ALL: [Self; 5] = [
        Self::Empty,
        Self::TooLong,
        Self::InvalidCharacters,
        Self::InvalidVisibility,
        Self::Server,
    ];

    fn code(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::InvalidCharacters => "invalid-characters",
            Self::InvalidVisibility => "invalid-visibility",
            Self::Server => "server",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|error| error.code() == code)
    }

    fn message(self) -> String {
        match self {
            Self::Empty => "Write something before publishing.".to_string(),
            Self::TooLong => format!("A post can be at most {BODY_MAX_CHARS} characters."),
            Self::InvalidCharacters => "The post contains characters that are not allowed.".to_string(),
            Self::InvalidVisibility => "Choose who can see the post.".to_string(),
            Self::Server => "Something went wrong, try again later.".to_string(),
        }
    }
}

impl From<BodyError> for PostError {
    fn from(err: BodyError) -> Self {
        match err {
            BodyError::Empty => Self::Empty,
            BodyError::TooLong => Self::TooLong,
            BodyError::InvalidCharacters => Self::InvalidCharacters,
        }
    }
}

/// Data used by the post form in `macros/post.html`.
pub struct Composer {
    page: &'static str,
    error: Option<String>,
    visibilities: [Visibility; 2],
    body_max_chars: usize,
}

impl Composer {
    fn new(page: PostPage, query: &HashMap<String, String>) -> Self {
        Self {
            page: page.as_str(),
            error: query
                .get("error")
                .and_then(|code| PostError::from_code(code))
                .map(PostError::message),
            visibilities: Visibility::ALL,
            body_max_chars: BODY_MAX_CHARS,
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct HomeTemplate {
    title: String,
    layout: Layout,
    composer: Composer,
    posts: Vec<Post>,
}

#[derive(Template)]
#[template(path = "feed.html")]
struct FeedTemplate {
    title: String,
    layout: Layout,
    composer: Composer,
    posts: Vec<Post>,
}

/// A page still works without its posts: they are logged and shown as empty.
fn or_empty(result: Result<Vec<Post>, sqlx::Error>) -> Vec<Post> {
    result.unwrap_or_else(|err| {
        tracing::error!("failed to load posts: {}", err);
        Vec::new()
    })
}

/// Public posts of everyone.
pub async fn home_page(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    let posts = match &state.pool {
        Some(pool) => {
            let viewer_id = layout.viewer.as_ref().map(|viewer| viewer.id);
            or_empty(post::timeline(pool, viewer_id).await)
        }
        None => Vec::new(),
    };
    render_template(&HomeTemplate {
        title: "Home".to_string(),
        layout,
        composer: Composer::new(PostPage::Home, &query),
        posts,
    })
}

/// The signed-in user's feed: their posts, and those of the accounts they
/// follow on other servers.
pub async fn feed_page(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    let (Some(viewer_id), Some(pool)) = (layout.viewer.as_ref().map(|viewer| viewer.id), &state.pool)
    else {
        return Redirect::to("/login").into_response();
    };
    let posts = or_empty(post::feed(pool, viewer_id).await);
    render_template(&FeedTemplate {
        title: "Feed".to_string(),
        layout,
        composer: Composer::new(PostPage::Feed, &query),
        posts,
    })
}

pub async fn create_post(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let page = PostPage::parse(form.get("page").map(String::as_str).unwrap_or(""));

    let result = async {
        let body = post::validate_body(form.get("body").map(String::as_str).unwrap_or(""))?;
        let visibility = form
            .get("visibility")
            .and_then(|value| Visibility::parse(value))
            .ok_or(PostError::InvalidVisibility)?;
        post::create(&pool, me.id, &body, visibility, None)
            .await
            .map_err(|err| {
                tracing::error!("failed to create post: {}", err);
                PostError::Server
            })
    }
    .await;

    if let Ok(post_id) = &result {
        notifier::post_created(&state, &pool, *post_id).await;
        state.previews.notify_one();
    }
    let location = match result {
        Ok(_) => page.url().to_string(),
        Err(error) => format!("{}?error={}", page.url(), error.code()),
    };
    Redirect::to(&location).into_response()
}

/// The page editing a post or a comment.
#[derive(Template)]
#[template(path = "edit.html")]
pub struct EditTemplate {
    pub title: String,
    pub layout: Layout,
    pub action: String,
    pub back: String,
    pub body: String,
    pub body_max_chars: usize,
    pub error: Option<String>,
    /// Other servers may have it: they get the edit.
    pub federated: bool,
}

/// The user's own post, written here.
async fn own_post(state: &AppState, session: &Session, post_id: i64) -> Result<(sqlx::PgPool, crate::entities::user::User, Post), Response> {
    let (pool, me) = signed_in_user(state, session).await?;
    match post::find(&pool, post_id, Some(me.id)).await {
        Ok(Some(post)) if post.can_edit() => Ok((pool, me, post)),
        Ok(_) => Err(error_page(Layout::load(state, session).await, StatusCode::NOT_FOUND, "This post does not exist or isn't yours.")),
        Err(err) => {
            tracing::error!("failed to load post {}: {}", post_id, err);
            Err(error_page(Layout::load(state, session).await, StatusCode::INTERNAL_SERVER_ERROR, "Failed to load this post."))
        }
    }
}

pub async fn edit_page(
    State(state): State<AppState>,
    session: Session,
    Path(post_id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (_, _, post) = match own_post(&state, &session, post_id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    render_template(&EditTemplate {
        title: "Edit the post".to_string(),
        layout: Layout::load(&state, &session).await,
        action: format!("/posts/{post_id}/edit"),
        back: format!("/posts/{post_id}"),
        body_max_chars: BODY_MAX_CHARS,
        error: query
            .get("error")
            .and_then(|code| PostError::from_code(code))
            .map(PostError::message),
        federated: state.federation.is_some() && post.visibility == Visibility::Public,
        body: post.body,
    })
}

pub async fn update_post(
    State(state): State<AppState>,
    session: Session,
    Path(post_id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me, _) = match own_post(&state, &session, post_id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let result = async {
        let body = post::validate_body(form.get("body").map(String::as_str).unwrap_or(""))?;
        post::update_own(&pool, post_id, me.id, &body).await.map_err(|err| {
            tracing::error!("failed to update post {}: {}", post_id, err);
            PostError::Server
        })
    }
    .await;
    match result {
        Ok(_) => {
            notifier::post_updated(&state, &pool, post_id).await;
            state.previews.notify_one();
            Redirect::to(&format!("/posts/{post_id}")).into_response()
        }
        Err(error) => Redirect::to(&format!("/posts/{post_id}/edit?error={}", error.code())).into_response(),
    }
}

/// Deletes the user's own post, or any post for the site's staff.
pub async fn delete_post(
    State(state): State<AppState>,
    session: Session,
    Path(post_id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let allowed = match post::find(&pool, post_id, Some(me.id)).await {
        Ok(Some(post)) => post.can_edit() || me.is_staff(),
        Ok(None) => false,
        Err(err) => {
            tracing::error!("failed to load post {}: {}", post_id, err);
            false
        }
    };
    if !allowed {
        return error_page(Layout::load(&state, &session).await, StatusCode::NOT_FOUND, "This post does not exist or isn't yours.");
    }
    let deletions = notifier::post_deleting(&state, &pool, post_id).await;
    match post::delete(&pool, post_id).await {
        Ok(_) => {
            deletions.send(&state, &pool).await;
            tracing::info!("@{} deleted post {}", me.username, post_id);
            // Not back to the post's own page, which is gone.
            let back = local_path(form.get("return"), "/your-feed");
            let back = if back.starts_with(&format!("/posts/{post_id}")) { "/your-feed".to_string() } else { back };
            Redirect::to(&back).into_response()
        }
        Err(err) => {
            tracing::error!("failed to delete post {}: {}", post_id, err);
            error_page(Layout::load(&state, &session).await, StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong, try again later.")
        }
    }
}
