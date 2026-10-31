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
    handlers::{auth, comment, community, media, messenger, moderation, notification, password, post, reaction, search, user},
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
        .route("/posts/{post_id}/edit", get(post::edit_page).post(post::update_post))
        .route("/posts/{post_id}/delete", post(post::delete_post))
        .route("/comments/{comment_id}/like", post(comment::like_comment))
        .route("/comments/{comment_id}/edit", get(comment::edit_page).post(comment::update_comment))
        .route("/comments/{comment_id}/delete", post(comment::delete_comment))
        .route("/communities", get(community::communities_page).post(community::create_community))
        .route("/community/{slug}", get(community::community_page))
        .route("/community/{slug}/join", post(community::join))
        .route("/community/{slug}/quit", post(community::quit))
        .route("/community/{slug}/request/cancel", post(community::cancel_request))
        .route("/community/{slug}/requests/{user_id}/approve", post(community::approve_request))
        .route("/community/{slug}/requests/{user_id}/reject", post(community::reject_request))
        .route("/community/{slug}/members/{user_id}/remove", post(community::remove_member))
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
        .route("/messenger", get(messenger::messenger_page))
        .route("/messenger/new", post(messenger::start))
        .route("/messenger/with/{username}", get(messenger::with_user).post(messenger::with_user))
        .route("/messenger/remote/{actor_id}", post(messenger::with_remote))
        .route("/messenger/{id}", get(messenger::conversation_page).post(messenger::send))
        .route("/messenger/{id}/after/{message_id}", get(messenger::after))
        .route("/messenger/{id}/messages/{message_id}/delete", post(messenger::delete))
        .route("/report", get(moderation::report_page).post(moderation::send_report))
        .route("/moderation", get(moderation::dashboard))
        .route("/moderation/reports/{id}/{action}", post(moderation::act))
        .route("/moderation/staff", get(moderation::staff_page).post(moderation::add_moderator))
        .route("/moderation/staff/{user_id}/remove", post(moderation::remove_moderator))
        .route("/search", get(search::search_page))
        .route("/search/suggest", get(search::suggest))
        .route("/fediverse", get(search::old_fediverse_page))
        .route("/fediverse/follow", post(search::follow))
        .route("/fediverse/unfollow", post(search::unfollow))
        .route("/remote/{id}", get(search::remote_profile))
        .route("/media/{hash}", get(media::proxy))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/user/{username}", get(user::user_page))
        .route("/user/{username}/follow", post(user::follow_user))
        .route("/user/{username}/unfollow", post(user::unfollow_user))
        .route("/user/{username}/friend", post(user::add_friend))
        .route("/user/{username}/unfriend", post(user::remove_friend))
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
