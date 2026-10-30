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
    handlers::{auth, system, user},
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
        .route("/", get(system::index))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/user/{username}", get(user::user_page))
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/register", get(auth::register_page).post(auth::register))
        .route("/logout", post(auth::logout))
}
