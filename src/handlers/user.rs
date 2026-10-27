use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use sqlx::PgPool;
use tower_sessions::Session;

use crate::{
    AppState,
    entities::user::{self, NAME_MAX_CHARS, ProfileImage, USERNAME_MAX_LEN, USERNAME_MIN_LEN, User},
    handlers::{
        image::{self, IMAGE_FIELD, ImageError, MAX_IMAGE_BYTES},
        session::current_user_id,
        system::{error_page, render_template},
    },
};

/// Errors sent back to the profile page as `?error=<code>`.
#[derive(Clone, Copy)]
enum EditError {
    InvalidName,
    InvalidUsername,
    UsernameTaken,
    MissingImage,
    ImageTooLarge,
    UnsupportedImage,
    Server,
}

impl EditError {
    const ALL: [Self; 7] = [
        Self::InvalidName,
        Self::InvalidUsername,
        Self::UsernameTaken,
        Self::MissingImage,
        Self::ImageTooLarge,
        Self::UnsupportedImage,
        Self::Server,
    ];

    fn code(self) -> &'static str {
        match self {
            Self::InvalidName => "invalid-name",
            Self::InvalidUsername => "invalid-username",
            Self::UsernameTaken => "username-taken",
            Self::MissingImage => "missing-image",
            Self::ImageTooLarge => "image-too-large",
            Self::UnsupportedImage => "unsupported-image",
            Self::Server => "server",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|error| error.code() == code)
    }

    fn message(self) -> String {
        match self {
            Self::InvalidName => format!(
                "Names must be at most {NAME_MAX_CHARS} characters, without control characters."
            ),
            Self::InvalidUsername => format!(
                "The username must be {USERNAME_MIN_LEN} to {USERNAME_MAX_LEN} characters, using only letters, digits and _."
            ),
            Self::UsernameTaken => "This username is already taken.".to_string(),
            Self::MissingImage => "Choose an image to upload.".to_string(),
            Self::ImageTooLarge => format!(
                "The image must be at most {} MB.",
                MAX_IMAGE_BYTES / 1024 / 1024
            ),
            Self::UnsupportedImage => {
                "Supported formats are JPEG, PNG, GIF, WebP and AVIF.".to_string()
            }
            Self::Server => "Something went wrong, try again later.".to_string(),
        }
    }
}

impl From<ImageError> for EditError {
    fn from(err: ImageError) -> Self {
        match err {
            ImageError::Missing => Self::MissingImage,
            ImageError::TooLarge => Self::ImageTooLarge,
            ImageError::UnsupportedFormat => Self::UnsupportedImage,
            ImageError::Io(err) => {
                tracing::error!("failed to save image: {}", err);
                Self::Server
            }
        }
    }
}

fn server_error(err: sqlx::Error) -> EditError {
    tracing::error!("user update failed: {}", err);
    EditError::Server
}

#[derive(Template)]
#[template(path = "user.html")]
struct UserTemplate {
    title: String,
    user: User,
    is_own_profile: bool,
    error: Option<String>,
    image_field: &'static str,
    name_max_chars: usize,
    username_max_len: usize,
}

pub async fn user_page(
    State(state): State<AppState>,
    session: Session,
    Path(username): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(pool) = &state.pool else {
        return error_page(StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };

    let user = match user::find_by_username(pool, &username).await {
        Ok(Some(user)) => user,
        Ok(None) => return error_page(StatusCode::NOT_FOUND, "This user does not exist."),
        Err(err) => {
            tracing::error!("failed to load user {}: {}", username, err);
            return error_page(StatusCode::INTERNAL_SERVER_ERROR, "Failed to load this user.");
        }
    };

    let is_own_profile = current_user_id(&session).await == Some(user.id);
    let error = query
        .get("error")
        .and_then(|code| EditError::from_code(code))
        .filter(|_| is_own_profile)
        .map(EditError::message);

    let template = UserTemplate {
        title: format!("{} (@{})", user.display_name(), user.username),
        user,
        is_own_profile,
        error,
        image_field: IMAGE_FIELD,
        name_max_chars: NAME_MAX_CHARS,
        username_max_len: USERNAME_MAX_LEN,
    };
    render_template(&template)
}

/// Database pool and signed-in user, or the response to send instead.
async fn signed_in_user(state: &AppState, session: &Session) -> Result<(PgPool, User), Response> {
    let Some(pool) = state.pool.clone() else {
        return Err(error_page(StatusCode::SERVICE_UNAVAILABLE, "The database is not configured."));
    };
    let not_signed_in = || error_page(StatusCode::UNAUTHORIZED, "You need to be signed in.");

    let Some(user_id) = current_user_id(session).await else {
        return Err(not_signed_in());
    };
    match user::find_by_id(&pool, user_id).await {
        Ok(Some(user)) => Ok((pool, user)),
        Ok(None) => Err(not_signed_in()),
        Err(err) => {
            tracing::error!("failed to load signed-in user {}: {}", user_id, err);
            Err(error_page(StatusCode::INTERNAL_SERVER_ERROR, "Failed to load your profile."))
        }
    }
}

/// Usernames are validated (`A-Za-z0-9_`), so they are safe to put in the URL as is.
fn back_to_profile(username: &str, result: Result<(), EditError>) -> Response {
    let location = match result {
        Ok(()) => format!("/user/{username}"),
        Err(error) => format!("/user/{username}?error={}", error.code()),
    };
    Redirect::to(&location).into_response()
}

pub async fn update_names(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };

    let field = |key: &str| user::validate_name(form.get(key).map(String::as_str).unwrap_or(""));
    let result = match (field("displayname"), field("firstname"), field("lastname")) {
        (Some(displayname), Some(firstname), Some(lastname)) => {
            user::update_names(&pool, me.id, &displayname, &firstname, &lastname)
                .await
                .map_err(server_error)
        }
        _ => Err(EditError::InvalidName),
    };
    back_to_profile(&me.username, result)
}

pub async fn update_username(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };

    let raw_username = form.get("username").map(String::as_str).unwrap_or("");
    let Some(username) = user::validate_username(raw_username) else {
        return back_to_profile(&me.username, Err(EditError::InvalidUsername));
    };
    if username == me.username {
        return back_to_profile(&me.username, Ok(()));
    }

    match user::update_username(&pool, me.id, &username).await {
        Ok(true) => back_to_profile(&username, Ok(())),
        Ok(false) => back_to_profile(&me.username, Err(EditError::UsernameTaken)),
        Err(err) => back_to_profile(&me.username, Err(server_error(err))),
    }
}

async fn upload_image(
    state: AppState,
    session: Session,
    mut multipart: Multipart,
    image: ProfileImage,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };

    let url = match image::save_user_image(me.id, image, &mut multipart).await {
        Ok(url) => url,
        Err(err) => return back_to_profile(&me.username, Err(err.into())),
    };

    if let Err(err) = user::update_image(&pool, me.id, image, Some(&url)).await {
        image::remove_user_image(me.id, &url).await;
        return back_to_profile(&me.username, Err(server_error(err)));
    }
    if let Some(old_url) = me.image_url(image) {
        image::remove_user_image(me.id, old_url).await;
    }
    back_to_profile(&me.username, Ok(()))
}

async fn remove_image(state: AppState, session: Session, image: ProfileImage) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };

    if let Err(err) = user::update_image(&pool, me.id, image, None).await {
        return back_to_profile(&me.username, Err(server_error(err)));
    }
    if let Some(old_url) = me.image_url(image) {
        image::remove_user_image(me.id, old_url).await;
    }
    back_to_profile(&me.username, Ok(()))
}

pub async fn upload_photo(
    State(state): State<AppState>,
    session: Session,
    multipart: Multipart,
) -> Response {
    upload_image(state, session, multipart, ProfileImage::Photo).await
}

pub async fn remove_photo(State(state): State<AppState>, session: Session) -> Response {
    remove_image(state, session, ProfileImage::Photo).await
}

pub async fn upload_background(
    State(state): State<AppState>,
    session: Session,
    multipart: Multipart,
) -> Response {
    upload_image(state, session, multipart, ProfileImage::Background).await
}

pub async fn remove_background(State(state): State<AppState>, session: Session) -> Response {
    remove_image(state, session, ProfileImage::Background).await
}
