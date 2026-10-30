use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use tower_sessions::Session;

use crate::{
    AppState,
    entities::user::{self, User},
    handlers::session::current_user_id,
};

/// Data needed by `base.html` on every page.
#[derive(Default)]
pub struct Layout {
    /// Signed-in user, `None` for visitors.
    pub viewer: Option<User>,
}

impl Layout {
    pub async fn load(state: &AppState, session: &Session) -> Self {
        let (Some(pool), Some(user_id)) = (&state.pool, current_user_id(session).await) else {
            return Self::default();
        };
        match user::find_by_id(pool, user_id).await {
            Ok(viewer) => Self { viewer },
            Err(err) => {
                tracing::warn!("failed to load signed-in user {}: {}", user_id, err);
                Self::default()
            }
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    title: String,
    layout: Layout,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate<'a> {
    title: String,
    layout: Layout,
    message: &'a str,
}

/// Error page keeping the site layout.
pub fn error_page(layout: Layout, status: StatusCode, message: &str) -> Response {
    let template = ErrorTemplate {
        title: status.canonical_reason().unwrap_or("Error").to_string(),
        layout,
        message,
    };
    (status, render_template(&template)).into_response()
}

pub fn render_template<T: Template>(template: &T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!("template rendering failed: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
        }
    }
}

pub async fn index(State(state): State<AppState>, session: Session) -> Response {
    let template = IndexTemplate {
        title: "Home".to_string(),
        layout: Layout::load(&state, &session).await,
    };
    render_template(&template)
}

/// Any route that does not exist.
pub async fn not_found(State(state): State<AppState>, session: Session) -> Response {
    let layout = Layout::load(&state, &session).await;
    error_page(layout, StatusCode::NOT_FOUND, "This page does not exist.")
}
