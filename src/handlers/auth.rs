use std::{collections::HashMap, sync::OnceLock};

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use askama::Template;
use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use sqlx::types::chrono::{NaiveDate, Utc};
use tower_sessions::Session;

use crate::{
    AppState,
    entities::user::{
        self, CreateAccountError, EMAIL_MAX_LEN, MIN_AGE_YEARS, NAME_MAX_CHARS, NewAccount,
        PASSWORD_MAX_CHARS, PASSWORD_MIN_CHARS, USERNAME_MAX_LEN, USERNAME_MIN_LEN,
    },
    handlers::{
        session::{sign_in, sign_out},
        system::{Layout, error_page, render_template},
    },
};

/*
Passwords
*/

fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Checked when the account does not exist, so the answer takes as long as
/// for a wrong password and does not reveal which usernames exist.
fn dummy_hash() -> &'static str {
    static DUMMY_HASH: OnceLock<String> = OnceLock::new();
    DUMMY_HASH.get_or_init(|| hash_password("dummy password").unwrap_or_default())
}

/// Argon2 is slow on purpose: run it outside the async threads.
async fn hash_password_blocking(password: String) -> Option<String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .ok()?
        .map_err(|err| tracing::error!("password hashing failed: {}", err))
        .ok()
}

async fn verify_password_blocking(password: String, hash: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || match hash {
        Some(hash) => verify_password(&password, &hash),
        None => {
            verify_password(&password, dummy_hash());
            false
        }
    })
    .await
    .unwrap_or(false)
}

fn field<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
    form.get(key).map(String::as_str).unwrap_or("")
}

/// Already signed-in users are sent to their profile instead of the login/register forms.
fn redirect_signed_in(layout: &Layout) -> Option<Response> {
    layout
        .viewer
        .as_ref()
        .map(|viewer| Redirect::to(&format!("/user/{}", viewer.username)).into_response())
}

/*
Register
*/

#[derive(Template)]
#[template(path = "register.html")]
struct RegisterTemplate {
    title: String,
    layout: Layout,
    error: Option<String>,
    // Values kept when the form is shown again after an error, never the password.
    username: String,
    email: String,
    firstname: String,
    lastname: String,
    birthday: String,
    // Limits used by the form inputs, the minimum age is checked by the server.
    today: String,
    username_max_len: usize,
    email_max_len: usize,
    name_max_chars: usize,
    password_min_chars: usize,
    password_max_chars: usize,
}

impl RegisterTemplate {
    fn new(layout: Layout, form: &HashMap<String, String>) -> Self {
        Self {
            title: "Register".to_string(),
            layout,
            error: None,
            username: field(form, "username").trim().to_string(),
            email: field(form, "email").trim().to_string(),
            firstname: field(form, "firstname").trim().to_string(),
            lastname: field(form, "lastname").trim().to_string(),
            birthday: field(form, "birthday").trim().to_string(),
            today: Utc::now().date_naive().format("%Y-%m-%d").to_string(),
            username_max_len: USERNAME_MAX_LEN,
            email_max_len: EMAIL_MAX_LEN,
            name_max_chars: NAME_MAX_CHARS,
            password_min_chars: PASSWORD_MIN_CHARS,
            password_max_chars: PASSWORD_MAX_CHARS,
        }
    }

    fn with_error(mut self, status: StatusCode, message: String) -> Response {
        self.error = Some(message);
        (status, render_template(&self)).into_response()
    }
}

pub async fn register_page(State(state): State<AppState>, session: Session) -> Response {
    let layout = Layout::load(&state, &session).await;
    if let Some(redirect) = redirect_signed_in(&layout) {
        return redirect;
    }
    render_template(&RegisterTemplate::new(layout, &HashMap::new()))
}

/// Checks every field and returns the account to create, or the message to show.
fn validate_registration(
    form: &HashMap<String, String>,
    today: NaiveDate,
) -> Result<(NewAccount, String), String> {
    let username = user::validate_username(field(form, "username")).ok_or_else(|| {
        format!(
            "The username must be {USERNAME_MIN_LEN} to {USERNAME_MAX_LEN} characters, using only letters, digits and _."
        )
    })?;
    let email = user::validate_email(field(form, "email"))
        .ok_or_else(|| "Enter a valid email address.".to_string())?;
    let (Some(firstname), Some(lastname)) = (
        user::validate_name(field(form, "firstname")),
        user::validate_name(field(form, "lastname")),
    ) else {
        return Err(format!(
            "Names must be at most {NAME_MAX_CHARS} characters, without control characters."
        ));
    };
    if firstname.is_empty() || lastname.is_empty() {
        return Err("Enter your first and last name.".to_string());
    }
    let birthday = user::validate_birthday(field(form, "birthday"), today)
        .ok_or_else(|| format!("You must be at least {MIN_AGE_YEARS} years old to register."))?;

    let password = field(form, "password");
    if !user::validate_password(password) {
        return Err(format!(
            "The password must be {PASSWORD_MIN_CHARS} to {PASSWORD_MAX_CHARS} characters, with an uppercase letter, a lowercase letter, a digit and a symbol. Spaces are allowed only one at a time, between words."
        ));
    }
    if password != field(form, "password_confirm") {
        return Err("The two passwords are different.".to_string());
    }

    let account = NewAccount {
        username,
        email,
        password_hash: String::new(),
        firstname,
        lastname,
        birthday,
    };
    Ok((account, password.to_string()))
}

pub async fn register(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    if let Some(redirect) = redirect_signed_in(&layout) {
        return redirect;
    }
    let Some(pool) = &state.pool else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };
    let page = RegisterTemplate::new(layout, &form);

    let (mut account, password) = match validate_registration(&form, Utc::now().date_naive()) {
        Ok(valid) => valid,
        Err(message) => return page.with_error(StatusCode::BAD_REQUEST, message),
    };
    let server_error = "Something went wrong, try again later.".to_string();
    let Some(password_hash) = hash_password_blocking(password).await else {
        return page.with_error(StatusCode::INTERNAL_SERVER_ERROR, server_error);
    };
    account.password_hash = password_hash;

    let created = match user::create_account(pool, &account).await {
        Ok(created) => created,
        Err(CreateAccountError::UsernameTaken) => {
            return page.with_error(StatusCode::CONFLICT, "This username is already taken.".to_string());
        }
        Err(CreateAccountError::EmailTaken) => {
            return page.with_error(
                StatusCode::CONFLICT,
                "An account already uses this email.".to_string(),
            );
        }
        Err(CreateAccountError::Database(err)) => {
            tracing::error!("account creation failed: {}", err);
            return page.with_error(StatusCode::INTERNAL_SERVER_ERROR, server_error);
        }
    };

    if let Err(err) = sign_in(&session, created.id).await {
        tracing::error!("failed to sign in new user {}: {}", created.id, err);
        return Redirect::to("/login").into_response();
    }
    tracing::info!("new account @{}", created.username);
    Redirect::to(&format!("/user/{}", created.username)).into_response()
}

/*
Login
*/

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    title: String,
    layout: Layout,
    error: Option<String>,
    /// Username or email, kept when the form is shown again.
    login: String,
}

pub async fn login_page(State(state): State<AppState>, session: Session) -> Response {
    let layout = Layout::load(&state, &session).await;
    if let Some(redirect) = redirect_signed_in(&layout) {
        return redirect;
    }
    render_template(&LoginTemplate {
        title: "Login".to_string(),
        layout,
        error: None,
        login: String::new(),
    })
}

pub async fn login(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    if let Some(redirect) = redirect_signed_in(&layout) {
        return redirect;
    }
    let Some(pool) = &state.pool else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };

    let login = field(&form, "login").trim().to_string();
    let password = field(&form, "password").to_string();
    let show_error = |layout, status, message: &str| {
        let template = LoginTemplate {
            title: "Login".to_string(),
            layout,
            error: Some(message.to_string()),
            login: login.clone(),
        };
        (status, render_template(&template)).into_response()
    };

    let (user_id, hash) = match user::find_credentials(pool, &login).await {
        Ok(Some((user_id, hash))) => (Some(user_id), hash),
        Ok(None) => (None, None),
        Err(err) => {
            tracing::error!("failed to load credentials: {}", err);
            return show_error(
                layout,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong, try again later.",
            );
        }
    };

    // Always verified, against the dummy hash when there is no account,
    // and the same message whether the account exists or not.
    let verified = verify_password_blocking(password, hash).await;
    let Some(user_id) = user_id.filter(|_| verified) else {
        return show_error(layout, StatusCode::UNAUTHORIZED, "Wrong username, email or password.");
    };

    if let Err(err) = sign_in(&session, user_id).await {
        tracing::error!("failed to sign in user {}: {}", user_id, err);
        return show_error(
            layout,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong, try again later.",
        );
    }
    Redirect::to("/").into_response()
}

pub async fn logout(session: Session) -> Response {
    if let Err(err) = sign_out(&session).await {
        tracing::error!("failed to sign out: {}", err);
    }
    Redirect::to("/").into_response()
}
