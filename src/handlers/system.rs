use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use tower_sessions::Session;

use crate::{
    AppState,
    entities::{notification, user::User},
    handlers::session::signed_in,
};

/// Data needed by `base.html` on every page.
#[derive(Default)]
pub struct Layout {
    /// Signed-in user, `None` for visitors.
    pub viewer: Option<User>,
    /// Shown next to "Notifications" in the menu, 0 for visitors.
    pub unread_notifications: i64,
}

impl Layout {
    pub async fn load(state: &AppState, session: &Session) -> Self {
        let Some(pool) = &state.pool else {
            return Self::default();
        };
        let viewer = match signed_in(pool, session).await {
            Ok(viewer) => viewer,
            Err(err) => {
                tracing::warn!("failed to load signed-in user: {}", err);
                return Self::default();
            }
        };
        let unread_notifications = match &viewer {
            Some(viewer) => notification::unread_count(pool, viewer.id).await.unwrap_or_else(|err| {
                tracing::warn!("failed to count notifications: {}", err);
                0
            }),
            None => 0,
        };
        Self { viewer, unread_notifications }
    }

    /// The count as shown in the menu: "99+" past 99.
    pub fn unread_label(&self) -> String {
        match self.unread_notifications {
            count if count > 99 => "99+".to_string(),
            count => count.to_string(),
        }
    }
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

/// `raw` without its `#fragment` if it's a path on this site, else `fallback`.
/// Used for the "return to" field of forms, so a forged form can't send
/// users to another website (`//evil.example` or `https://...`).
pub fn local_path(raw: Option<&String>, fallback: &str) -> String {
    let path = raw
        .map(|raw| raw.split('#').next().unwrap_or_default())
        .unwrap_or_default();
    let safe = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.len() <= 512;
    if safe { path.to_string() } else { fallback.to_string() }
}

/// Any route that does not exist.
pub async fn not_found(State(state): State<AppState>, session: Session) -> Response {
    let layout = Layout::load(&state, &session).await;
    error_page(layout, StatusCode::NOT_FOUND, "This page does not exist.")
}
