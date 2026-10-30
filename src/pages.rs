use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;

use crate::{
    AppState,
    handlers::{auth, comment, community, notification, password, post, reaction, user},
};

async fn health_live() -> Response {
    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
}

async fn health_ready(State(state): State<AppState>) -> Response {
    let Some(pool) = state.pool else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "no database configured" })),
        )
            .into_response();
    };

    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&pool)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ready" }))).into_response(),
        Err(err) => {
            tracing::warn!("health readiness check failed: {}", err);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "unavailable" })),
            )
                .into_response()
        }
    }
}

pub fn page_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(post::home_page))
        .route("/your-feed", get(post::feed_page))
        .route("/posts", post(post::create_post))
        .route("/posts/{post_id}", get(comment::post_page))
        .route("/posts/{post_id}/comments", post(comment::create_comment))
        .route("/posts/{post_id}/like", post(reaction::like))
        .route("/posts/{post_id}/share", post(reaction::share))
        .route("/comments/{comment_id}/like", post(comment::like_comment))
        .route("/communities", get(community::communities_page).post(community::create_community))
        .route("/community/{slug}", get(community::community_page))
        .route("/community/{slug}/join", post(community::join))
        .route("/community/{slug}/quit", post(community::quit))
        .route("/community/{slug}/ignore", post(community::toggle_ignore))
        .route("/community/{slug}/posts", post(community::create_post))
        .route("/community/{slug}/posts/{post_id}/remove", post(community::remove_post))
        .route("/community/{slug}/ban", post(community::toggle_ban))
        .route("/community/{slug}/delete", post(community::delete_community))
        .route("/community/{slug}/settings", get(community::settings_page))
        .route("/community/{slug}/settings/details", post(community::update_details))
        .route("/community/{slug}/settings/photo", post(community::upload_photo))
        .route("/community/{slug}/settings/photo/remove", post(community::remove_photo))
        .route("/community/{slug}/rules", post(community::add_rule))
        .route("/community/{slug}/rules/{rule_id}", post(community::update_rule))
        .route("/community/{slug}/rules/{rule_id}/delete", post(community::delete_rule))
        .route("/community/{slug}/moderators", post(community::add_moderator))
        .route("/community/{slug}/moderators/{user_id}/remove", post(community::remove_moderator))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/user/{username}", get(user::user_page))
        .route("/notifications", get(notification::notifications_page))
        .route("/notifications/live", get(notification::live))
        .route("/notifications/older", get(notification::older))
        .route("/notifications/read-all", post(notification::read_all))
        .route("/notifications/delete-all", post(notification::delete_all))
        .route("/notifications/{notification_id}/open", get(notification::open))
        .route("/notifications/{notification_id}/delete", post(notification::delete))
        .route("/settings", get(user::settings_page))
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/register", get(auth::register_page).post(auth::register))
        .route("/logout", post(auth::logout))
        .route("/forgot-password", get(password::forgot_page).post(password::forgot))
        .route("/reset-password", get(password::reset_page).post(password::reset))
}
