use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Redirect, Response},
};
use sqlx::PgPool;
use tower_sessions::Session;

use crate::{
    AppState,
    entities::comment::{self, ProfileComment},
    entities::follow::{self, Counts, Listed},
    entities::friend::{self, Added, State as FriendState},
    entities::notification,
    entities::post::{self, Post},
    entities::session as remember_tokens,
    entities::user::{
        self, BIO_MAX_CHARS, NAME_MAX_CHARS, PASSWORD_MAX_CHARS, PASSWORD_MIN_CHARS, ProfileImage,
        USERNAME_MAX_LEN, USERNAME_MIN_LEN, User,
    },
    handlers::{
        auth::{hash_password_blocking, verify_password_blocking},
        image::{self, IMAGE_FIELD, ImageError, ImageOwner, MAX_IMAGE_BYTES},
        session::{has_remember_cookie, remember, sign_in, signed_in},
        system::{Layout, error_page, local_path, render_template},
    },
};

/// Errors sent back to the settings page as `?error=<code>`.
#[derive(Clone, Copy)]
enum EditError {
    InvalidName,
    InvalidBio,
    InvalidUsername,
    UsernameTaken,
    MissingImage,
    ImageTooLarge,
    UnsupportedImage,
    WrongPassword,
    WeakPassword,
    PasswordMismatch,
    Server,
}

impl EditError {
    const ALL: [Self; 11] = [
        Self::InvalidName,
        Self::InvalidBio,
        Self::InvalidUsername,
        Self::UsernameTaken,
        Self::MissingImage,
        Self::ImageTooLarge,
        Self::UnsupportedImage,
        Self::WrongPassword,
        Self::WeakPassword,
        Self::PasswordMismatch,
        Self::Server,
    ];

    fn code(self) -> &'static str {
        match self {
            Self::InvalidName => "invalid-name",
            Self::InvalidBio => "invalid-bio",
            Self::InvalidUsername => "invalid-username",
            Self::UsernameTaken => "username-taken",
            Self::MissingImage => "missing-image",
            Self::ImageTooLarge => "image-too-large",
            Self::UnsupportedImage => "unsupported-image",
            Self::WrongPassword => "wrong-password",
            Self::WeakPassword => "weak-password",
            Self::PasswordMismatch => "password-mismatch",
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
            Self::InvalidBio => format!(
                "The bio must be at most {BIO_MAX_CHARS} characters, without control characters."
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
            Self::WrongPassword => "Your current password is wrong.".to_string(),
            Self::WeakPassword => format!(
                "The new password must be {PASSWORD_MIN_CHARS} to {PASSWORD_MAX_CHARS} characters, with an uppercase letter, a lowercase letter, a digit and a symbol. Spaces are allowed only one at a time, between words."
            ),
            Self::PasswordMismatch => "The two new passwords are different.".to_string(),
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

/// Sections of the profile, chosen with `?tab=`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProfileTab {
    Posts,
    Comments,
    Friends,
    Followers,
    Following,
    About,
}

impl ProfileTab {
    const ALL: [Self; 6] = [
        Self::Posts,
        Self::Comments,
        Self::Friends,
        Self::Followers,
        Self::Following,
        Self::About,
    ];

    fn slug(self) -> &'static str {
        match self {
            Self::Posts => "posts",
            Self::Comments => "comments",
            Self::About => "about",
            Self::Friends => "friends",
            Self::Followers => "followers",
            Self::Following => "following",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Posts => "Posts",
            Self::Comments => "Comments",
            Self::About => "About me",
            Self::Friends => "Friends",
            Self::Followers => "Followers",
            Self::Following => "Following",
        }
    }

    /// Unknown or missing values open the posts.
    fn from_query(value: Option<&String>) -> Self {
        value
            .and_then(|slug| Self::ALL.into_iter().find(|tab| tab.slug() == slug))
            .unwrap_or(Self::Posts)
    }
}

struct TabLink {
    label: &'static str,
    url: String,
    active: bool,
}

/// The default tab links to the profile itself, so `/user/alice` stays the main URL.
fn tab_links(username: &str, current: ProfileTab) -> Vec<TabLink> {
    ProfileTab::ALL
        .into_iter()
        .map(|tab| TabLink {
            label: tab.label(),
            url: match tab {
                ProfileTab::Posts => format!("/user/{username}"),
                _ => format!("/user/{username}?tab={}", tab.slug()),
            },
            active: tab == current,
        })
        .collect()
}

struct AboutRow {
    label: &'static str,
    value: String,
    /// Hidden from other users, shown to the owner with a note.
    only_you: bool,
}

/// What the viewer may see in "About me", following the privacy settings.
fn about_rows(user: &User, is_own_profile: bool) -> Vec<AboutRow> {
    let mut rows = Vec::new();
    let mut add = |label, value: String, public: bool| {
        if !value.is_empty() && (public || is_own_profile) {
            rows.push(AboutRow {
                label,
                value,
                only_you: !public,
            });
        }
    };

    add("First name", user.firstname.clone(), user.firstname_public);
    add("Last name", user.lastname.clone(), user.lastname_public);
    if let Some(birthday) = user.birthday {
        add("Birthday", birthday.format("%B %-d, %Y").to_string(), user.birthday_public);
    }
    add("Gender", user.gender.clone(), true);
    add("Joined", user.created_at.format("%B %Y").to_string(), true);
    rows
}

#[derive(Template)]
#[template(path = "user.html")]
struct UserTemplate {
    title: String,
    layout: Layout,
    user: User,
    is_own_profile: bool,
    tab: ProfileTab,
    tabs: Vec<TabLink>,
    about: Vec<AboutRow>,
    /// Only loaded for the posts tab.
    posts: Vec<Post>,
    /// Only loaded for the comments tab.
    comments: Vec<ProfileComment>,
    /// Followers or followed accounts, for those tabs.
    accounts: Vec<Listed>,
    counts: Counts,
    /// Whether the viewer follows the user, `None` on their own profile and
    /// for visitors.
    viewer_follows: Option<bool>,
    /// Where the viewer and the user are as friends, `None` like `viewer_follows`.
    friendship: Option<FriendState>,
    friend_count: i64,
    /// For the friends tab. The requests only on the user's own profile.
    friends: Vec<User>,
    requests_received: Vec<User>,
    requests_sent: Vec<User>,
    /// The actor for Fediverse servers, see `templates/base.html`.
    activitypub_url: Option<String>,
    /// Where the like and share buttons come back to.
    return_to: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    title: String,
    layout: Layout,
    user: User,
    error: Option<String>,
    saved: bool,
    image_field: &'static str,
    name_max_chars: usize,
    bio_max_chars: usize,
    password_min_chars: usize,
    password_max_chars: usize,
    username_max_len: usize,
}

pub async fn user_page(
    State(state): State<AppState>,
    session: Session,
    Path(username): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    let Some(pool) = &state.pool else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };

    let user = match user::find_by_username(pool, &username).await {
        // Suspended: only the site's staff still see the profile.
        Ok(Some(user)) if user.is_suspended() && !layout.viewer.as_ref().is_some_and(User::is_staff) => {
            return error_page(layout, StatusCode::NOT_FOUND, "This account is suspended.");
        }
        Ok(Some(user)) => user,
        Ok(None) => return error_page(layout, StatusCode::NOT_FOUND, "This user does not exist."),
        Err(err) => {
            tracing::error!("failed to load user {}: {}", username, err);
            return error_page(
                layout,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load this user.",
            );
        }
    };

    let is_own_profile = layout.viewer.as_ref().is_some_and(|viewer| viewer.id == user.id);

    let tab = ProfileTab::from_query(query.get("tab"));
    let posts = if tab == ProfileTab::Posts {
        let viewer_id = layout.viewer.as_ref().map(|viewer| viewer.id);
        post::by_author(pool, user.id, viewer_id)
            .await
            .unwrap_or_else(|err| {
                tracing::error!("failed to load posts of {}: {}", user.username, err);
                Vec::new()
            })
    } else {
        Vec::new()
    };
    let comments = if tab == ProfileTab::Comments {
        let viewer_id = layout.viewer.as_ref().map(|viewer| viewer.id);
        comment::by_author(pool, user.id, viewer_id)
            .await
            .unwrap_or_else(|err| {
                tracing::error!("failed to load comments of {}: {}", user.username, err);
                Vec::new()
            })
    } else {
        Vec::new()
    };
    let accounts = match tab {
        ProfileTab::Followers => follow::followers(pool, user.id).await,
        ProfileTab::Following => follow::following(pool, user.id, is_own_profile).await,
        _ => Ok(Vec::new()),
    }
    .unwrap_or_else(|err| {
        tracing::error!("failed to load follows of {}: {}", user.username, err);
        Vec::new()
    });
    let counts = follow::counts(pool, user.id).await.unwrap_or_else(|err| {
        tracing::error!("failed to count follows of {}: {}", user.username, err);
        Counts::default()
    });
    let viewer_follows = match &layout.viewer {
        Some(viewer) if !is_own_profile => Some(follow::is_following(pool, viewer.id, user.id).await.unwrap_or(false)),
        _ => None,
    };
    let friendship = match &layout.viewer {
        Some(viewer) if !is_own_profile => Some(friend::state(pool, viewer.id, user.id).await.unwrap_or(FriendState::None)),
        _ => None,
    };
    let friend_count = friend::count(pool, user.id).await.unwrap_or_else(|err| {
        tracing::error!("failed to count friends of {}: {}", user.username, err);
        0
    });
    let (friends, requests_received, requests_sent) = if tab == ProfileTab::Friends {
        let loaded = async {
            let friends = friend::friends(pool, user.id).await?;
            if !is_own_profile {
                return Ok::<_, sqlx::Error>((friends, Vec::new(), Vec::new()));
            }
            Ok((friends, friend::received(pool, user.id).await?, friend::sent(pool, user.id).await?))
        }
        .await;
        loaded.unwrap_or_else(|err| {
            tracing::error!("failed to load friends of {}: {}", user.username, err);
            (Vec::new(), Vec::new(), Vec::new())
        })
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    let activitypub_url = state
        .federation
        .as_ref()
        .filter(|_| user.federation_enabled)
        .map(|federation| federation.actor_url(user.public_id));
    let template = UserTemplate {
        accounts,
        counts,
        viewer_follows,
        friendship,
        friend_count,
        friends,
        requests_received,
        requests_sent,
        title: format!("{} (@{})", user.display_name(), user.username),
        activitypub_url,
        layout,
        tab,
        tabs: tab_links(&user.username, tab),
        about: about_rows(&user, is_own_profile),
        posts,
        comments,
        return_to: format!("/user/{}", user.username),
        user,
        is_own_profile,
    };
    render_template(&template)
}

/// Follows or unfollows the user `username`, then goes back to `return`.
async fn change_follow(state: AppState, session: Session, username: String, form: HashMap<String, String>, follow: bool) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let target = match user::find_by_username(&pool, &username).await {
        Ok(Some(target)) => target,
        Ok(None) => return error_page(Layout::load(&state, &session).await, StatusCode::NOT_FOUND, "This user does not exist."),
        Err(err) => {
            tracing::error!("failed to load user {}: {}", username, err);
            return error_page(Layout::load(&state, &session).await, StatusCode::INTERNAL_SERVER_ERROR, "Failed to load this user.");
        }
    };
    let result = async {
        if follow {
            if follow::follow(&pool, me.id, target.id).await? {
                notification::user_followed(&pool, target.id, me.id).await?;
            }
        } else if follow::unfollow(&pool, me.id, target.id).await? {
            notification::user_unfollowed(&pool, target.id, me.id).await?;
        }
        Ok::<_, sqlx::Error>(())
    }
    .await;
    if let Err(err) = result {
        tracing::error!("failed to change the follow of @{} by @{}: {}", target.username, me.username, err);
        return error_page(Layout::load(&state, &session).await, StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong, try again later.");
    }
    Redirect::to(&local_path(form.get("return"), &format!("/user/{}", target.username))).into_response()
}

/// "Add friend" (a request, or accepting theirs) or removing whatever links
/// the two users, then back to `return`.
async fn change_friend(state: AppState, session: Session, username: String, form: HashMap<String, String>, add: bool) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let other = match user::find_by_username(&pool, &username).await {
        Ok(Some(other)) => other,
        Ok(None) => return error_page(Layout::load(&state, &session).await, StatusCode::NOT_FOUND, "This user does not exist."),
        Err(err) => {
            tracing::error!("failed to load user {}: {}", username, err);
            return error_page(Layout::load(&state, &session).await, StatusCode::INTERNAL_SERVER_ERROR, "Failed to load this user.");
        }
    };
    let result = async {
        if add {
            match friend::request(&pool, me.id, other.id).await? {
                Added::Requested => notification::friend_changed(&pool, other.id, me.id, false).await?,
                Added::Accepted => {
                    notification::friend_request_answered(&pool, me.id, other.id).await?;
                    notification::friend_changed(&pool, other.id, me.id, true).await?;
                }
                Added::Nothing => {}
            }
        } else if friend::remove(&pool, me.id, other.id).await? != FriendState::None {
            notification::friend_removed(&pool, me.id, other.id).await?;
        }
        Ok::<_, sqlx::Error>(())
    }
    .await;
    if let Err(err) = result {
        tracing::error!("failed to change the friendship of @{} and @{}: {}", me.username, other.username, err);
        return error_page(Layout::load(&state, &session).await, StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong, try again later.");
    }
    Redirect::to(&local_path(form.get("return"), &format!("/user/{}", other.username))).into_response()
}

pub async fn add_friend(
    State(state): State<AppState>,
    session: Session,
    Path(username): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    change_friend(state, session, username, form, true).await
}

pub async fn remove_friend(
    State(state): State<AppState>,
    session: Session,
    Path(username): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    change_friend(state, session, username, form, false).await
}

pub async fn follow_user(
    State(state): State<AppState>,
    session: Session,
    Path(username): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    change_follow(state, session, username, form, true).await
}

pub async fn unfollow_user(
    State(state): State<AppState>,
    session: Session,
    Path(username): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    change_follow(state, session, username, form, false).await
}

/// Profile editing, for the signed-in user only. Visitors are sent to the login page.
pub async fn settings_page(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    let Some(user) = layout.viewer.clone() else {
        return Redirect::to("/login").into_response();
    };

    let template = SettingsTemplate {
        title: "Settings".to_string(),
        layout,
        user,
        error: query
            .get("error")
            .and_then(|code| EditError::from_code(code))
            .map(EditError::message),
        saved: query.contains_key("saved"),
        image_field: IMAGE_FIELD,
        name_max_chars: NAME_MAX_CHARS,
        bio_max_chars: BIO_MAX_CHARS,
        password_min_chars: PASSWORD_MIN_CHARS,
        password_max_chars: PASSWORD_MAX_CHARS,
        username_max_len: USERNAME_MAX_LEN,
    };
    render_template(&template)
}

/// Database pool and signed-in user, or the response to send instead.
/// Error pages use an empty layout since there is no valid signed-in user.
pub(crate) async fn signed_in_user(state: &AppState, session: &Session) -> Result<(PgPool, User), Response> {
    let Some(pool) = state.pool.clone() else {
        return Err(error_page(
            Layout::default(),
            StatusCode::SERVICE_UNAVAILABLE,
            "The database is not configured.",
        ));
    };
    let not_signed_in =
        || error_page(Layout::default(), StatusCode::UNAUTHORIZED, "You need to be signed in.");

    match signed_in(&pool, session).await {
        Ok(Some(user)) => Ok((pool, user)),
        Ok(None) => Err(not_signed_in()),
        Err(err) => {
            tracing::error!("failed to load signed-in user: {}", err);
            Err(error_page(
                Layout::default(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load your profile.",
            ))
        }
    }
}

fn back_to_settings(result: Result<(), EditError>) -> Response {
    let location = match result {
        Ok(()) => "/settings?saved".to_string(),
        Err(error) => format!("/settings?error={}", error.code()),
    };
    Redirect::to(&location).into_response()
}

/// Changes the password after checking the current one. Every other browser is
/// signed out (sessions and "stay signed in"), this one stays signed in.
pub async fn change_password(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let field = |key: &str| form.get(key).cloned().unwrap_or_default();
    let (current, new, confirm) = (
        field("current_password"),
        field("new_password"),
        field("new_password_confirm"),
    );

    let stored_hash = match user::password_hash(&pool, me.id).await {
        Ok(hash) => hash,
        Err(err) => return back_to_settings(Err(server_error(err))),
    };
    // Accounts without a password can't confirm one: they use "forgot password".
    if !verify_password_blocking(current, stored_hash).await {
        return back_to_settings(Err(EditError::WrongPassword));
    }
    if !user::validate_password(&new) {
        return back_to_settings(Err(EditError::WeakPassword));
    }
    if new != confirm {
        return back_to_settings(Err(EditError::PasswordMismatch));
    }

    let Some(new_hash) = hash_password_blocking(new).await else {
        return back_to_settings(Err(EditError::Server));
    };
    let updated = match user::update_password(&pool, me.id, &new_hash).await {
        Ok(updated) => updated,
        Err(err) => return back_to_settings(Err(server_error(err))),
    };
    if let Err(err) = remember_tokens::delete_for_user(&pool, me.id).await {
        tracing::error!("failed to sign out other browsers of user {}: {}", me.id, err);
    }
    // The password change made every session outdated, this one included.
    if let Err(err) = sign_in(&session, &updated).await {
        tracing::error!("failed to keep user {} signed in: {}", me.id, err);
    }

    let mut response = back_to_settings(Ok(()));
    if has_remember_cookie(&headers)
        && let Some(cookie) = remember(&state, me.id).await
    {
        response.headers_mut().append(SET_COOKIE, cookie);
    }
    response
}

pub async fn update_bio(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };

    let result = match user::validate_bio(form.get("bio").map(String::as_str).unwrap_or("")) {
        Some(bio) => user::update_bio(&pool, me.id, &bio).await.map_err(server_error),
        None => Err(EditError::InvalidBio),
    };
    back_to_settings(result)
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
    back_to_settings(result)
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
        return back_to_settings(Err(EditError::InvalidUsername));
    };
    if username == me.username {
        return back_to_settings(Ok(()));
    }

    match user::update_username(&pool, me.id, &username).await {
        Ok(true) => back_to_settings(Ok(())),
        Ok(false) => back_to_settings(Err(EditError::UsernameTaken)),
        Err(err) => back_to_settings(Err(server_error(err))),
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

    let url = match image::save_image(ImageOwner::User(me.id), image.as_str(), &mut multipart).await {
        Ok(url) => url,
        Err(err) => return back_to_settings(Err(err.into())),
    };

    if let Err(err) = user::update_image(&pool, me.id, image, Some(&url)).await {
        image::remove_image(ImageOwner::User(me.id), &url).await;
        return back_to_settings(Err(server_error(err)));
    }
    if let Some(old_url) = me.image_url(image) {
        image::remove_image(ImageOwner::User(me.id), old_url).await;
    }
    back_to_settings(Ok(()))
}

async fn remove_image(state: AppState, session: Session, image: ProfileImage) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };

    if let Err(err) = user::update_image(&pool, me.id, image, None).await {
        return back_to_settings(Err(server_error(err)));
    }
    if let Some(old_url) = me.image_url(image) {
        image::remove_image(ImageOwner::User(me.id), old_url).await;
    }
    back_to_settings(Ok(()))
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
