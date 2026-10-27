use tower_sessions::Session;

pub const USER_ID_KEY: &str = "user_id";

/// Id of the signed-in user, if any.
pub async fn current_user_id(session: &Session) -> Option<i32> {
    session.get::<i32>(USER_ID_KEY).await.ok().flatten()
}

/// Development-only sign in, until real authentication is written.
/// Creates the user if it does not exist yet. Not compiled in release builds.
#[cfg(debug_assertions)]
pub async fn dev_login(
    session: Session,
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> axum::response::Response {
    use crate::{entities::user, handlers::system::error_page};
    use axum::{http::StatusCode, response::{IntoResponse, Redirect}};

    let Some(pool) = &state.pool else {
        return error_page(StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };
    let Some(username) = user::validate_username(&username) else {
        return error_page(StatusCode::BAD_REQUEST, "Invalid username.");
    };

    let found = match user::find_by_username(pool, &username).await {
        Ok(Some(found)) => Ok(found),
        Ok(None) => user::create(pool, &username).await,
        Err(err) => Err(err),
    };
    let found = match found {
        Ok(found) => found,
        Err(err) => {
            tracing::error!("dev login failed: {}", err);
            return error_page(StatusCode::INTERNAL_SERVER_ERROR, "Dev login failed.");
        }
    };

    if let Err(err) = session.insert(USER_ID_KEY, found.id).await {
        tracing::error!("failed to store session: {}", err);
        return error_page(StatusCode::INTERNAL_SERVER_ERROR, "Dev login failed.");
    }
    tracing::warn!("dev login as @{}", found.username);
    Redirect::to(&format!("/user/{}", found.username)).into_response()
}
