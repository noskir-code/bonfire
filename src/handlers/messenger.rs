//! The messenger: private conversations with users of this server and with
//! accounts of other Fediverse servers (see `entities::message`).
//!
//! New messages appear live: the WebSocket of the notifications says which
//! conversation changed, and the open conversation asks for what's new
//! (`/messenger/{id}/after/{message_id}`, see `assets/system.js`).

use std::collections::HashMap;

use askama::Template;
use axum::{
    Form, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use serde_json::json;
use sqlx::PgPool;
use tower_sessions::Session;

use crate::{
    AppState,
    entities::{
        author::Author,
        friend,
        message::{self, BODY_MAX_CHARS, Conversation, Message, Summary},
        notification,
        post::BodyError,
        user::{self, User},
    },
    handlers::{
        notifier,
        search::{self, Found},
        system::{Layout, error_page, render_template},
        user::signed_in_user,
    },
};

#[derive(Template)]
#[template(path = "messenger.html")]
struct MessengerTemplate {
    title: String,
    layout: Layout,
    conversations: Vec<Summary>,
    /// Friends without a message yet, after the conversations: one click
    /// opens the conversation.
    friends: Vec<User>,
    error: Option<String>,
    /// `@name@server` can be written to.
    fediverse: bool,
}

#[derive(Template)]
#[template(path = "conversation.html")]
struct ConversationTemplate {
    title: String,
    layout: Layout,
    conversation: Conversation,
    messages: Vec<Message>,
    error: Option<String>,
    body_max_chars: usize,
    /// The other person can't receive messages: shown instead of the form.
    closed: Option<&'static str>,
}

/// New messages of a conversation, for the live page.
#[derive(Template)]
#[template(
    source = "{% import \"macros/message.html\" as ui %}{% for message in messages %}{% call ui::message_item(message, conversation_id) %}{% endcall %}{% endfor %}",
    ext = "html"
)]
struct MessageItems<'a> {
    messages: &'a [Message],
    conversation_id: i64,
}

/// Results sent back as `?error=<code>`.
fn error_message(code: &str) -> Option<String> {
    Some(match code {
        "empty" => "Write something before sending.".to_string(),
        "too-long" => format!("A message can be at most {BODY_MAX_CHARS} characters."),
        "invalid-characters" => "The message contains characters that are not allowed.".to_string(),
        "nobody" => "Nobody found with this name or address.".to_string(),
        "yourself" => "You can't write to yourself.".to_string(),
        "fediverse-off" => "Your account isn't on the Fediverse: it can only write to people of this server.".to_string(),
        "server" => "Something went wrong, try again later.".to_string(),
        _ => return None,
    })
}

fn body_error_code(err: BodyError) -> &'static str {
    match err {
        BodyError::Empty => "empty",
        BodyError::TooLong => "too-long",
        BodyError::InvalidCharacters => "invalid-characters",
    }
}

/// The conversations of the signed-in user, latest first.
pub async fn messenger_page(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let conversations = message::list(&pool, me.id).await.unwrap_or_else(|err| {
        tracing::error!("failed to load conversations of {}: {}", me.username, err);
        Vec::new()
    });
    // Friends already in a conversation appear there.
    let mut friends: Vec<User> = friend::friends(&pool, me.id)
        .await
        .unwrap_or_else(|err| {
            tracing::error!("failed to load friends of {}: {}", me.username, err);
            Vec::new()
        })
        .into_iter()
        .filter(|friend| {
            !conversations
                .iter()
                .any(|conversation| conversation.other.local().is_some_and(|other| other.id == friend.id))
        })
        .collect();
    friends.sort_by_key(|friend| friend.display_name().to_lowercase());
    render_template(&MessengerTemplate {
        title: "Messenger".to_string(),
        layout: Layout::load(&state, &session).await,
        conversations,
        friends,
        error: query.get("error").and_then(|code| error_message(code)),
        fediverse: state.federation.is_some() && me.federation_enabled,
    })
}

/// Tells the open pages of both people that the conversation changed.
async fn signal(pool: &PgPool, conversation_id: i64) {
    let signalled = async {
        let people = message::local_people(pool, conversation_id).await?;
        notification::signal_message(pool, &people, conversation_id).await
    }
    .await;
    if let Err(err) = signalled {
        tracing::warn!("failed to signal conversation {}: {}", conversation_id, err);
    }
}

/// Opens (or starts) the conversation with `to`: a username here, or an
/// account of another server (`@name@server` or the link of its profile).
pub async fn start(State(state): State<AppState>, session: Session, Form(form): Form<HashMap<String, String>>) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let to = form.get("to").map(|to| to.trim()).unwrap_or_default();
    let back = |code: &str| Redirect::to(&format!("/messenger?error={code}#new")).into_response();
    let local_name = match state.federation.as_deref() {
        // `@name@this-domain` is a user here.
        Some(federation) => to
            .trim_start_matches('@')
            .strip_suffix(&format!("@{}", federation.domain))
            .map(str::to_string),
        None => None,
    };
    if search::is_address(to) && local_name.is_none() {
        if state.federation.is_none() || !me.federation_enabled {
            return back("fediverse-off");
        }
        return match search::find_account(&state, &pool, &me, to).await {
            Ok(Found::Remote(account)) => open_remote(&pool, &me, account.author.id).await,
            Ok(Found::Local(_)) | Err(_) => back("nobody"),
        };
    }
    let name = local_name.unwrap_or_else(|| to.trim_start_matches('@').to_string());
    match user::find_by_username(&pool, &name).await {
        Ok(Some(other)) if other.id == me.id => back("yourself"),
        Ok(Some(other)) => open_local(&pool, &me, &other).await,
        Ok(None) => back("nobody"),
        Err(err) => {
            tracing::error!("failed to find {}: {}", name, err);
            back("server")
        }
    }
}

async fn open_local(pool: &PgPool, me: &User, other: &User) -> Response {
    match message::with_user(pool, me.id, other.id).await {
        Ok(id) => Redirect::to(&format!("/messenger/{id}")).into_response(),
        Err(err) => {
            tracing::error!("failed to open a conversation with {}: {}", other.username, err);
            Redirect::to("/messenger?error=server").into_response()
        }
    }
}

async fn open_remote(pool: &PgPool, me: &User, remote_actor_id: i64) -> Response {
    match message::with_remote(pool, me.id, remote_actor_id).await {
        Ok(id) => Redirect::to(&format!("/messenger/{id}")).into_response(),
        Err(err) => {
            tracing::error!("failed to open a conversation with actor {}: {}", remote_actor_id, err);
            Redirect::to("/messenger?error=server").into_response()
        }
    }
}

/// "Message" on a profile here, and the friends of the messenger's list
/// (a link: opening a conversation changes nothing until a message is sent).
pub async fn with_user(State(state): State<AppState>, session: Session, Path(username): Path<String>) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    match user::find_by_username(&pool, &username).await {
        Ok(Some(other)) if other.id != me.id => open_local(&pool, &me, &other).await,
        Ok(_) => Redirect::to("/messenger?error=nobody#new").into_response(),
        Err(err) => {
            tracing::error!("failed to find {}: {}", username, err);
            Redirect::to("/messenger?error=server").into_response()
        }
    }
}

/// "Message" on the page of an account of another server.
pub async fn with_remote(State(state): State<AppState>, session: Session, Path(actor_id): Path<i64>) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    if state.federation.is_none() || !me.federation_enabled {
        return Redirect::to("/messenger?error=fediverse-off").into_response();
    }
    open_remote(&pool, &me, actor_id).await
}

/// The conversation of the signed-in user, `Err` with the page to show
/// when it isn't theirs.
async fn load(state: &AppState, session: &Session, id: i64) -> Result<(PgPool, User, Conversation), Response> {
    let (pool, me) = signed_in_user(state, session).await?;
    match message::find(&pool, id, me.id).await {
        Ok(Some(conversation)) => Ok((pool, me, conversation)),
        Ok(None) => Err(error_page(
            Layout::load(state, session).await,
            StatusCode::NOT_FOUND,
            "This conversation does not exist.",
        )),
        Err(err) => {
            tracing::error!("failed to load conversation {}: {}", id, err);
            Err(error_page(
                Layout::load(state, session).await,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load this conversation.",
            ))
        }
    }
}

/// Why the other person can't receive messages, if they can't.
fn closed(state: &AppState, me: &User, conversation: &Conversation) -> Option<&'static str> {
    match &conversation.other {
        Author::Remote(_) if state.federation.is_none() => Some("The Fediverse is turned off on this server."),
        Author::Remote(_) if !me.federation_enabled => {
            Some("Your account isn't on the Fediverse: it can only write to people of this server.")
        }
        _ => None,
    }
}

pub async fn conversation_page(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (pool, me, conversation) = match load(&state, &session, id).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let messages = match message::messages(&pool, id, me.id, None).await {
        Ok(messages) => messages,
        Err(err) => {
            tracing::error!("failed to load messages of {}: {}", id, err);
            Vec::new()
        }
    };
    if let Err(err) = message::mark_read(&pool, id, me.id).await {
        tracing::warn!("failed to mark conversation {} read: {}", id, err);
    }
    // Other tabs update their unread count.
    notification::signal_message(&pool, &[me.id], id).await.ok();
    render_template(&ConversationTemplate {
        title: format!("Messages with {}", conversation.other.display_name()),
        layout: Layout::load(&state, &session).await,
        closed: closed(&state, &me, &conversation),
        conversation,
        messages,
        error: query.get("error").and_then(|code| error_message(code)),
        body_max_chars: BODY_MAX_CHARS,
    })
}

pub async fn send(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me, conversation) = match load(&state, &session, id).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if closed(&state, &me, &conversation).is_some() {
        return Redirect::to(&format!("/messenger/{id}")).into_response();
    }
    let body = match message::validate_body(form.get("body").map(String::as_str).unwrap_or("")) {
        Ok(body) => body,
        Err(err) => return Redirect::to(&format!("/messenger/{id}?error={}#composer", body_error_code(err))).into_response(),
    };
    match message::add(&pool, id, me.id, &body).await {
        Ok((message_id, sent_at)) => {
            signal(&pool, id).await;
            notifier::message_sent(&state, &pool, &me, &conversation, message_id, &body, &sent_at).await;
            Redirect::to(&format!("/messenger/{id}#message-{message_id}")).into_response()
        }
        Err(err) => {
            tracing::error!("failed to send a message in {}: {}", id, err);
            Redirect::to(&format!("/messenger/{id}?error=server#composer")).into_response()
        }
    }
}

/// Deletes one of the user's messages, for the other person too.
pub async fn delete(State(state): State<AppState>, session: Session, Path((id, message_id)): Path<(i64, i64)>) -> Response {
    let (pool, me, conversation) = match load(&state, &session, id).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    match message::delete_own(&pool, message_id, me.id).await {
        Ok(Some(conversation_id)) if conversation_id == id => {
            signal(&pool, id).await;
            notifier::message_deleted(&state, &pool, &me, &conversation, message_id).await;
        }
        Ok(_) => {}
        Err(err) => tracing::error!("failed to delete message {}: {}", message_id, err),
    }
    Redirect::to(&format!("/messenger/{id}")).into_response()
}

/// `{"html": "...", "ids": [...]}`: the messages after `message_id`, and the
/// ids of all the shown ones (to remove deleted ones). Marks it read.
pub async fn after(State(state): State<AppState>, session: Session, Path((id, message_id)): Path<(i64, i64)>) -> Response {
    let (pool, me, _) = match load(&state, &session, id).await {
        Ok(loaded) => loaded,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let loaded = async {
        let new = message::messages(&pool, id, me.id, Some(message_id)).await?;
        let all = message::messages(&pool, id, me.id, None).await?;
        message::mark_read(&pool, id, me.id).await?;
        Ok::<_, sqlx::Error>((new, all))
    }
    .await;
    let (new, all) = match loaded {
        Ok(loaded) => loaded,
        Err(err) => {
            tracing::error!("failed to load new messages of {}: {}", id, err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let html = match (MessageItems { messages: &new, conversation_id: id }).render() {
        Ok(html) => html,
        Err(err) => {
            tracing::error!("failed to render messages: {}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let ids: Vec<i64> = all.iter().map(|shown| shown.id).collect();
    let bodies: HashMap<String, String> = all.iter().map(|shown| (shown.id.to_string(), shown.body_html())).collect();
    Json(json!({ "html": html, "ids": ids, "bodies": bodies })).into_response()
}
