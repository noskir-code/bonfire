use axum::{
    extract::{Request, State},
    http::{
        HeaderMap, HeaderValue,
        header::{COOKIE, SET_COOKIE},
    },
    middleware::Next,
    response::Response,
};
use sqlx::PgPool;
use tower_sessions::{Session, session::Id};

use crate::{
    AppState,
    entities::session as tokens,
    handlers::auth::{hash_password_blocking, verify_password_blocking},
};

pub const USER_ID_KEY: &str = "user_id";
/// Same name as the old Instavox cookie.
const REMEMBER_COOKIE: &str = "remember_token";

/// Id of the signed-in user, if any.
pub async fn current_user_id(session: &Session) -> Option<i32> {
    session.get::<i32>(USER_ID_KEY).await.ok().flatten()
}

/// Gives the session a new id before storing the user, so an id set
/// before signing in (session fixation) can't be reused afterwards.
pub async fn sign_in(session: &Session, user_id: i32) -> Result<(), tower_sessions::session::Error> {
    session.cycle_id().await?;
    session.insert(USER_ID_KEY, user_id).await
}

/// Deletes the session data and its cookie.
pub async fn sign_out(session: &Session) -> Result<(), tower_sessions::session::Error> {
    session.flush().await
}

/*
Stay signed in
*/

/// 128 random bits, URL-safe. Session ids come from a cryptographic generator
/// seeded by the operating system, reused here instead of adding a crate.
fn random_part() -> String {
    Id::default().to_string()
}

fn cookie(value: &str, max_age_secs: i64, secure: bool) -> Option<HeaderValue> {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{REMEMBER_COOKIE}={value}; Max-Age={max_age_secs}; Path=/; HttpOnly; SameSite=Lax{secure}"
    ))
    .ok()
}

/// `Set-Cookie` header deleting the token cookie.
pub fn forget_cookie(secure: bool) -> Option<HeaderValue> {
    cookie("", 0, secure)
}

/// Token cookie sent by the browser, as `(selector, validator)`.
fn read_token(headers: &HeaderMap) -> Option<(String, String)> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == REMEMBER_COOKIE)
        .and_then(|(_, value)| value.split_once('.'))
        .map(|(selector, validator)| (selector.to_string(), validator.to_string()))
}

/// Creates a token for the user and returns the cookie to send,
/// `None` (logged) if it failed: the user stays signed in until the next restart.
pub async fn remember(state: &AppState, user_id: i32) -> Option<HeaderValue> {
    let pool = state.pool.as_ref()?;
    let selector = random_part();
    let validator = format!("{}{}", random_part(), random_part());

    let validator_hash = hash_password_blocking(validator.clone()).await?;
    if let Err(err) = tokens::create(pool, &selector, &validator_hash, user_id, state.remember_days).await {
        tracing::error!("failed to store remember token: {}", err);
        return None;
    }
    let max_age = i64::from(state.remember_days) * 24 * 60 * 60;
    cookie(&format!("{selector}.{validator}"), max_age, state.secure_cookies)
}

/// Deletes the token of this browser, at logout.
pub async fn forget(state: &AppState, headers: &HeaderMap) {
    let (Some(pool), Some((selector, _))) = (&state.pool, read_token(headers)) else {
        return;
    };
    if let Err(err) = tokens::delete(pool, &selector).await {
        tracing::error!("failed to delete remember token: {}", err);
    }
}

/// Signs the user in from the token. `Ok(false)` when the token is not valid
/// anymore and its cookie should be deleted.
async fn restore(
    pool: &PgPool,
    session: &Session,
    selector: &str,
    validator: String,
    days: i32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(stored) = tokens::find_valid(pool, selector).await? else {
        return Ok(false);
    };
    if !verify_password_blocking(validator, Some(stored.validator_hash)).await {
        // Right selector, wrong secret: a forged or stolen copy. The token is
        // deleted so it can't be guessed further.
        tracing::warn!("invalid remember token for user {}, deleted", stored.user_id);
        tokens::delete(pool, selector).await?;
        return Ok(false);
    }

    sign_in(session, stored.user_id).await?;
    tokens::extend(pool, selector, days).await?;
    Ok(true)
}

/// Middleware: when the session has no user (new visit, or the server restarted
/// and lost the sessions) but the browser has a token, signs the user back in.
/// Must run inside the session layer.
pub async fn restore_remembered(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // Pages load their files in parallel: only pages restore, not each image or script.
    let path = request.uri().path();
    let is_file = path.starts_with("/assets/") || path.starts_with("/public/");

    let mut invalid = false;
    if !is_file
        && let (Some(pool), Some(session)) = (&state.pool, request.extensions().get::<Session>().cloned())
        && current_user_id(&session).await.is_none()
        && let Some((selector, validator)) = read_token(request.headers())
    {
        match restore(pool, &session, &selector, validator, state.remember_days).await {
            Ok(restored) => invalid = !restored,
            Err(err) => tracing::error!("failed to restore remembered session: {}", err),
        }
    }

    let mut response = next.run(request).await;
    if invalid && let Some(value) = forget_cookie(state.secure_cookies) {
        response.headers_mut().append(SET_COOKIE, value);
    }
    response
}
