use std::collections::HashMap;

use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use tower_sessions::Session;

use crate::{
    AppState,
    entities::post::{self, Reaction},
    handlers::{
        system::{Layout, error_page, local_path},
        user::signed_in_user,
    },
};

/// Toggles the reaction, then goes back to the page the button was on,
/// scrolled to the post.
async fn react(
    state: AppState,
    session: Session,
    post_id: i64,
    form: HashMap<String, String>,
    reaction: Reaction,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };

    match post::toggle(&pool, reaction, post_id, me.id).await {
        Ok(Some(_)) => {
            let back = local_path(form.get("return"), "/");
            Redirect::to(&format!("{back}#post-{post_id}")).into_response()
        }
        Ok(None) => {
            let layout = Layout::load(&state, &session).await;
            error_page(layout, StatusCode::NOT_FOUND, "This post does not exist or can't be shared.")
        }
        Err(err) => {
            tracing::error!("failed to toggle {:?} on post {}: {}", reaction, post_id, err);
            let layout = Layout::load(&state, &session).await;
            error_page(layout, StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong, try again later.")
        }
    }
}

pub async fn like(
    State(state): State<AppState>,
    session: Session,
    Path(post_id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    react(state, session, post_id, form, Reaction::Like).await
}

pub async fn share(
    State(state): State<AppState>,
    session: Session,
    Path(post_id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    react(state, session, post_id, form, Reaction::Share).await
}
