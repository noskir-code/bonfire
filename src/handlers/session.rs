use tower_sessions::Session;

pub const USER_ID_KEY: &str = "user_id";

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
