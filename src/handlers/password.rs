//! "Forgot password": a link sent by email to choose a new password.

use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use sqlx::PgPool;
use tower_sessions::Session;

use crate::{
    AppState,
    entities::{
        password_reset::{self as reset_tokens, VALID_MINUTES},
        session as remember_tokens,
        user::{self, PASSWORD_MAX_CHARS, PASSWORD_MIN_CHARS},
    },
    handlers::{
        auth::{
            FormError, error_for, general_errors, hash_password_blocking, server_error,
            verify_password_blocking,
        },
        session::{new_token, sign_out, split_token},
        system::{Layout, error_page, render_template},
    },
    mail::Email,
};

#[derive(Template)]
#[template(path = "emails/reset_password.txt")]
struct ResetEmailText<'a> {
    link: &'a str,
    valid_minutes: i32,
}

#[derive(Template)]
#[template(path = "emails/reset_password.html")]
struct ResetEmailHtml<'a> {
    site_url: &'a str,
    link: &'a str,
    valid_minutes: i32,
}

fn field<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
    form.get(key).map(String::as_str).unwrap_or("")
}

/*
Ask for a link
*/

#[derive(Template)]
#[template(path = "forgot_password.html")]
struct ForgotTemplate {
    title: String,
    layout: Layout,
    errors: Vec<FormError>,
    email: String,
    /// The form was sent: show the "check your email" message.
    sent: bool,
    valid_minutes: i32,
}

impl ForgotTemplate {
    fn new(layout: Layout, email: String) -> Self {
        Self {
            title: "Forgot password".to_string(),
            layout,
            errors: Vec::new(),
            email,
            sent: false,
            valid_minutes: VALID_MINUTES,
        }
    }

    fn error_for(&self, field: &str) -> Option<&str> {
        error_for(&self.errors, field)
    }

    fn general_errors(&self) -> Vec<&str> {
        general_errors(&self.errors)
    }
}

pub async fn forgot_page(State(state): State<AppState>, session: Session) -> Response {
    let layout = Layout::load(&state, &session).await;
    // Signed-in users change their password in the settings.
    if layout.viewer.is_some() {
        return Redirect::to("/settings").into_response();
    }
    render_template(&ForgotTemplate::new(layout, String::new()))
}

pub async fn forgot(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    if state.pool.is_none() {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    }
    let mut page = ForgotTemplate::new(layout, field(&form, "email").trim().to_string());

    let Some(email) = user::validate_email(field(&form, "email")) else {
        page.errors.push(FormError::field("email", "Enter a valid email address."));
        return (StatusCode::BAD_REQUEST, render_template(&page)).into_response();
    };
    // Debug builds always have a mailer (Mailpit or the log), see `mail.rs`.
    if state.mailer.is_none() {
        page.errors.push(FormError::general(
            "Password reset by email is not available on this server. Contact its administrator.",
        ));
        return (StatusCode::SERVICE_UNAVAILABLE, render_template(&page)).into_response();
    }

    // In the background, and the same answer for every email: neither the
    // message nor the response time tells whether an account uses it.
    tokio::spawn(send_reset_link(state.clone(), email));
    page.sent = true;
    render_template(&page)
}

async fn send_reset_link(state: AppState, email: String) {
    let Some(pool) = &state.pool else {
        return;
    };
    let (user_id, address) = match user::find_by_email(pool, &email).await {
        Ok(Some(found)) => found,
        Ok(None) => return,
        Err(err) => {
            tracing::error!("failed to look up email for password reset: {}", err);
            return;
        }
    };
    match reset_tokens::recently_sent(pool, user_id).await {
        Ok(false) => {}
        Ok(true) => {
            tracing::info!("password reset for user {} asked again too soon, not sent", user_id);
            return;
        }
        Err(err) => {
            tracing::error!("failed to check password reset delay: {}", err);
            return;
        }
    }

    let (selector, validator) = new_token();
    let Some(validator_hash) = hash_password_blocking(validator.clone()).await else {
        return;
    };
    if let Err(err) = reset_tokens::create(pool, &selector, &validator_hash, user_id).await {
        tracing::error!("failed to store password reset token: {}", err);
        return;
    }

    let link = format!("{}/reset-password?token={selector}.{validator}", state.public_base_url);
    let Some(mailer) = &state.mailer else {
        return;
    };
    let email = Email::render(
        "Reset your Bonfire password",
        &ResetEmailText {
            link: &link,
            valid_minutes: VALID_MINUTES,
        },
        &ResetEmailHtml {
            site_url: &state.public_base_url,
            link: &link,
            valid_minutes: VALID_MINUTES,
        },
    );
    let result = match email {
        Ok(email) => mailer.send(&address, email).await,
        Err(err) => Err(err),
    };
    match result {
        Ok(()) => tracing::info!("password reset link sent to user {}", user_id),
        Err(err) => tracing::error!("failed to send password reset email to user {}: {}", user_id, err),
    }
}

/*
Choose a new password
*/

#[derive(Template)]
#[template(path = "reset_password.html")]
struct ResetTemplate {
    title: String,
    layout: Layout,
    errors: Vec<FormError>,
    /// Sent back with the form. Empty when the link is not valid.
    token: String,
    password_min_chars: usize,
    password_max_chars: usize,
}

impl ResetTemplate {
    fn new(layout: Layout, token: String) -> Self {
        Self {
            title: "Reset password".to_string(),
            layout,
            errors: Vec::new(),
            token,
            password_min_chars: PASSWORD_MIN_CHARS,
            password_max_chars: PASSWORD_MAX_CHARS,
        }
    }

    fn error_for(&self, field: &str) -> Option<&str> {
        error_for(&self.errors, field)
    }

    fn general_errors(&self) -> Vec<&str> {
        general_errors(&self.errors)
    }
}

/// The user of a valid, unexpired link.
async fn check_token(pool: &PgPool, token: &str) -> Result<Option<i32>, sqlx::Error> {
    let Some((selector, validator)) = split_token(token) else {
        return Ok(None);
    };
    let Some(stored) = reset_tokens::find_valid(pool, &selector).await? else {
        return Ok(None);
    };
    let valid = verify_password_blocking(validator, Some(stored.validator_hash)).await;
    Ok(valid.then_some(stored.user_id))
}

/// Shown instead of the form when the link can't be used.
fn invalid_link(layout: Layout) -> Response {
    error_page(
        layout,
        StatusCode::BAD_REQUEST,
        "This link is not valid anymore: it expired, was already used, or a newer one was sent. Ask for a new one from the login page.",
    )
}

pub async fn reset_page(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    let Some(pool) = &state.pool else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };
    let token = query.get("token").cloned().unwrap_or_default();
    match check_token(pool, &token).await {
        Ok(Some(_)) => render_template(&ResetTemplate::new(layout, token)),
        Ok(None) => invalid_link(layout),
        Err(err) => {
            tracing::error!("failed to check password reset token: {}", err);
            error_page(layout, StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong, try again later.")
        }
    }
}

pub async fn reset(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    let Some(pool) = &state.pool else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };
    let token = field(&form, "token").to_string();
    let user_id = match check_token(pool, &token).await {
        Ok(Some(user_id)) => user_id,
        Ok(None) => return invalid_link(layout),
        Err(err) => {
            tracing::error!("failed to check password reset token: {}", err);
            let mut page = ResetTemplate::new(layout, token);
            page.errors.push(server_error());
            return (StatusCode::INTERNAL_SERVER_ERROR, render_template(&page)).into_response();
        }
    };

    let password = field(&form, "password");
    let mut page = ResetTemplate::new(layout, token);
    if !user::validate_password(password) {
        page.errors.push(FormError::field(
            "password",
            format!(
                "{PASSWORD_MIN_CHARS} to {PASSWORD_MAX_CHARS} characters, with an uppercase letter, a lowercase letter, a digit and a symbol. Spaces are allowed only one at a time, between words."
            ),
        ));
    } else if password != field(&form, "password_confirm") {
        page.errors.push(FormError::field("password_confirm", "The two passwords are different."));
    }
    if !page.errors.is_empty() {
        return (StatusCode::BAD_REQUEST, render_template(&page)).into_response();
    }

    let Some(password_hash) = hash_password_blocking(password.to_string()).await else {
        page.errors.push(server_error());
        return (StatusCode::INTERNAL_SERVER_ERROR, render_template(&page)).into_response();
    };
    // Also signs out every session: they keep the old `session_version`.
    if let Err(err) = user::update_password(pool, user_id, &password_hash).await {
        tracing::error!("failed to reset password of user {}: {}", user_id, err);
        page.errors.push(server_error());
        return (StatusCode::INTERNAL_SERVER_ERROR, render_template(&page)).into_response();
    }
    if let Err(err) = reset_tokens::delete_for_user(pool, user_id).await {
        tracing::error!("failed to delete password reset links of user {}: {}", user_id, err);
    }
    if let Err(err) = remember_tokens::delete_for_user(pool, user_id).await {
        tracing::error!("failed to sign out browsers of user {}: {}", user_id, err);
    }
    if let Err(err) = sign_out(&session).await {
        tracing::warn!("failed to sign out after password reset: {}", err);
    }
    tracing::info!("password reset for user {}", user_id);
    Redirect::to("/login?reset").into_response()
}
