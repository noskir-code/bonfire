use axum::{
    Router,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{AppState, handlers::system};

fn json_response(status: StatusCode, body: jzon::JsonValue) -> Response {
    (status, [(CONTENT_TYPE, "application/json")], body.dump()).into_response()
}

async fn health_live() -> Response {
    json_response(StatusCode::OK, jzon::object! { status: "ok" })
}

async fn health_ready(State(state): State<AppState>) -> Response {
    let Some(pool) = state.pool else {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            jzon::object! { status: "no database configured" },
        );
    };

    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&pool)
        .await
    {
        Ok(_) => json_response(StatusCode::OK, jzon::object! { status: "ready" }),
        Err(err) => {
            tracing::warn!("health readiness check failed: {}", err);
            json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                jzon::object! { status: "unavailable" },
            )
        }
    }
}

pub fn page_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(system::index))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
}
