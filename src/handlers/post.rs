use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    extract::{Query, State},
    response::{IntoResponse, Redirect, Response},
};
use tower_sessions::Session;

use crate::{
    AppState,
    entities::post::{self, BODY_MAX_CHARS, BodyError, Post, Visibility},
    handlers::{
        notifier,
        system::{Layout, render_template},
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

/// The signed-in user's feed. Until following exists, it holds their own posts.
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
    let posts = or_empty(post::by_author(pool, viewer_id, Some(viewer_id)).await);
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
    }
    let location = match result {
        Ok(_) => page.url().to_string(),
        Err(error) => format!("{}?error={}", page.url(), error.code()),
    };
    Redirect::to(&location).into_response()
}
