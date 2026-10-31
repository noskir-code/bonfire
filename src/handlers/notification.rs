//! The notifications page and its actions (open, mark read, delete), and the
//! live updates: every open page keeps a WebSocket to `/notifications/live`
//! and gets the unread count, new notifications and other changes at once.
//!
//! Every action is a link or a form, so the page works without JavaScript;
//! `assets/system.js` sends them in the background instead.
//!
//! Changes arrive from Postgres (LISTEN on `notification::CHANNEL`, see
//! `listen`), so a notification made by another Bonfire server sharing the
//! database reaches pages served here too.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use askama::Template;
use axum::{
    Json,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgListener};
use tokio::sync::broadcast;
use tower_sessions::{MemoryStore, Session, session::Id};

use crate::{
    AppState,
    entities::notification::{self, Change, Notification},
    handlers::{
        session::signed_in,
        system::{Layout, error_page, render_template},
        user::signed_in_user,
    },
};

/// Proxies close WebSockets that stay silent, usually after a minute.
const PING_EVERY: Duration = Duration::from_secs(30);

#[derive(Template)]
#[template(path = "notifications.html")]
struct NotificationsTemplate {
    title: String,
    layout: Layout,
    notifications: Vec<Notification>,
    /// `?before=`: showing older notifications, not the newest.
    before: Option<i64>,
    /// Id for the link to the next page, `None` on the last one.
    next_before: Option<i64>,
}

/// One notification as HTML, for the live updates and the older pages loaded
/// by `assets/system.js`.
#[derive(Template)]
#[template(
    source = "{% import \"macros/notification.html\" as ui %}{% for notification in notifications %}{% call ui::notification_item(notification) %}{% endcall %}{% endfor %}",
    ext = "html"
)]
struct NotificationItems<'a> {
    notifications: &'a [Notification],
}

fn render_items(notifications: &[Notification]) -> Option<String> {
    NotificationItems { notifications }
        .render()
        .map_err(|err| tracing::error!("failed to render notifications: {}", err))
        .ok()
}

/// The page: newest first, or older than `?before=`.
pub async fn notifications_page(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let before = query.get("before").and_then(|id| id.parse::<i64>().ok());
    let (notifications, has_more) = match notification::page(&pool, me.id, before).await {
        Ok(page) => page,
        Err(err) => {
            tracing::error!("failed to load notifications of {}: {}", me.username, err);
            let layout = Layout::load(&state, &session).await;
            return error_page(layout, StatusCode::INTERNAL_SERVER_ERROR, "Failed to load your notifications.");
        }
    };
    let next_before = has_more
        .then(|| notifications.last().map(|notification| notification.id))
        .flatten();
    render_template(&NotificationsTemplate {
        title: "Notifications".to_string(),
        layout: Layout::load(&state, &session).await,
        notifications,
        before,
        next_before,
    })
}

/// `{"html": "...", "next_before": id or null}`: the notifications older than
/// `?before=`, appended by `assets/system.js` when scrolling down.
pub async fn older(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let Some(before) = query.get("before").and_then(|id| id.parse::<i64>().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (notifications, has_more) = match notification::page(&pool, me.id, Some(before)).await {
        Ok(page) => page,
        Err(err) => {
            tracing::error!("failed to load notifications of {}: {}", me.username, err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(html) = render_items(&notifications) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let next_before = has_more
        .then(|| notifications.last().map(|notification| notification.id))
        .flatten();
    Json(json!({ "html": html, "next_before": next_before })).into_response()
}

/// Opens what the notification is about, marking it read on the way.
pub async fn open(State(state): State<AppState>, session: Session, Path(id): Path<i64>) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let found = match notification::find(&pool, me.id, id).await {
        Ok(found) => found,
        Err(err) => {
            tracing::error!("failed to load notification {}: {}", id, err);
            return Redirect::to("/notifications").into_response();
        }
    };
    let Some(found) = found else {
        // Deleted meanwhile, or not theirs.
        return Redirect::to("/notifications").into_response();
    };
    if found.unread
        && let Err(err) = notification::mark_read(&pool, me.id, id).await
    {
        tracing::warn!("failed to mark notification {} as read: {}", id, err);
    }
    Redirect::to(&found.link()).into_response()
}

/// Runs an action of the signed-in user, then goes back to the page. The
/// page's script sends these in the background and ignores the redirect.
async fn act<F, Fut>(state: AppState, session: Session, action: F) -> Response
where
    F: FnOnce(PgPool, i32) -> Fut,
    Fut: std::future::Future<Output = Result<(), sqlx::Error>>,
{
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    if let Err(err) = action(pool, me.id).await {
        tracing::error!("notification action of {} failed: {}", me.username, err);
        let layout = Layout::load(&state, &session).await;
        return error_page(layout, StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong, try again later.");
    }
    Redirect::to("/notifications").into_response()
}

pub async fn read_all(State(state): State<AppState>, session: Session) -> Response {
    act(state, session, |pool, user_id| async move { notification::mark_all_read(&pool, user_id).await }).await
}

pub async fn delete(State(state): State<AppState>, session: Session, Path(id): Path<i64>) -> Response {
    act(state, session, |pool, user_id| async move { notification::delete(&pool, user_id, id).await }).await
}

pub async fn delete_all(State(state): State<AppState>, session: Session) -> Response {
    act(state, session, |pool, user_id| async move { notification::delete_all(&pool, user_id).await }).await
}

/// Users with an open page, each with a channel of the changes of their
/// notifications.
#[derive(Default)]
pub struct LiveUsers {
    channels: Mutex<HashMap<i32, broadcast::Sender<Change>>>,
}

impl LiveUsers {
    fn subscribe(&self, user_id: i32) -> broadcast::Receiver<Change> {
        let mut channels = self.channels.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Forgets users whose pages all closed.
        channels.retain(|_, sender| sender.receiver_count() > 0);
        // A page falling behind gets a full count instead, see `send_changes`.
        channels
            .entry(user_id)
            .or_insert_with(|| broadcast::channel(64).0)
            .subscribe()
    }

    fn signal(&self, user_id: i32, change: Change) {
        let channels = self.channels.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sender) = channels.get(&user_id) {
            // Fails only when no page listens anymore.
            let _ = sender.send(change);
        }
    }
}

/// Forwards the Postgres signals to the open pages, forever. Reconnects when
/// the database goes away.
pub async fn listen(pool: PgPool, live: Arc<LiveUsers>) {
    loop {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(listener) => listener,
            Err(err) => {
                tracing::warn!("live notifications: failed to connect, retrying: {}", err);
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        if let Err(err) = listener.listen(notification::CHANNEL).await {
            tracing::warn!("live notifications: failed to listen, retrying: {}", err);
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        loop {
            // Reconnects by itself after a lost connection, then returns
            // the error once: signals sent meanwhile are lost, the next
            // change brings the count back up to date.
            match listener.recv().await {
                Ok(signal) => match Change::parse(signal.payload()) {
                    Some((user_id, change)) => live.signal(user_id, change),
                    None => tracing::warn!("live notifications: invalid signal '{}'", signal.payload()),
                },
                Err(err) => {
                    tracing::warn!("live notifications: connection lost: {}", err);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

/// Opens the live count of the signed-in user. Refused to visitors, and to
/// pages of other sites: a WebSocket isn't limited to its own site like
/// `fetch` is, only the `Origin` header tells where it comes from.
pub async fn live(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(pool) = state.pool.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let user_id = match signed_in(&pool, &session).await {
        Ok(Some(me)) => me.id,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(err) => {
            tracing::warn!("live notifications: failed to load user: {}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let live = state.live_notifications.clone();
    let store = state.session_store.clone();
    let session_id = session.id();
    upgrade.on_upgrade(move |socket| send_changes(socket, pool, store, session_id, live, user_id))
}

/// Whether `Origin` is this site (the `Host` the browser connected to).
/// Clients that aren't browsers send no `Origin`, they have no cookies of
/// the user either.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|origin| origin.split_once("://"))
        .map(|(_, host)| host);
    let host = headers.get(header::HOST).and_then(|host| host.to_str().ok());
    origin_host.is_some() && origin_host == host
}

/// Sends `{"unread": n}` at once, then each change with the new count:
/// `added` (the notification's HTML), `removed` or `read` (an id, or "all").
/// Stops when the page closes or the user signs out.
async fn send_changes(
    mut socket: WebSocket,
    pool: PgPool,
    store: MemoryStore,
    session_id: Option<Id>,
    live: Arc<LiveUsers>,
    user_id: i32,
) {
    let mut changes = live.subscribe(user_id);
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;

    if send(&mut socket, &pool, user_id, json!({})).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            change = changes.recv() => {
                let message = match change {
                    Ok(change) => describe(&pool, user_id, change).await,
                    // Too many changes at once: the count is enough to catch up.
                    Err(broadcast::error::RecvError::Lagged(_)) => Some(json!({})),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                // Signed out, or password changed, since the page opened. Read
                // again from the store: a `Session` keeps what it loaded first.
                let session = Session::new(session_id, Arc::new(store.clone()), None);
                if !matches!(signed_in(&pool, &session).await, Ok(Some(me)) if me.id == user_id) {
                    break;
                }
                if let Some(message) = message
                    && send(&mut socket, &pool, user_id, message).await.is_err()
                {
                    break;
                }
            }
            message = socket.recv() => match message {
                // The page sends nothing, pongs are answered by axum.
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// What the page needs to apply `change`, `None` when there is nothing to send.
async fn describe(pool: &PgPool, user_id: i32, change: Change) -> Option<Value> {
    Some(match change {
        Change::Added(id) => {
            // Already deleted again: nothing to show.
            let notification = match notification::find(pool, user_id, id).await {
                Ok(found) => found?,
                Err(err) => {
                    tracing::warn!("live notifications: failed to load {}: {}", id, err);
                    return Some(json!({}));
                }
            };
            let html = render_items(std::slice::from_ref(&notification))?;
            json!({ "added": { "id": id, "html": html } })
        }
        Change::Removed(id) => json!({ "removed": id }),
        Change::Read(id) => json!({ "read": id }),
        Change::AllRemoved => json!({ "removed": "all" }),
        Change::AllRead => json!({ "read": "all" }),
        Change::Message(conversation_id) => json!({ "message": conversation_id }),
    })
}

/// Sends `message` with the current unread counts added: notifications and
/// conversations of the messenger.
async fn send(socket: &mut WebSocket, pool: &PgPool, user_id: i32, mut message: Value) -> Result<(), ()> {
    let unread = notification::unread_count(pool, user_id).await.map_err(|err| {
        tracing::warn!("live notifications: failed to count: {}", err);
    })?;
    let unread_messages = crate::entities::message::unread_count(pool, user_id).await.map_err(|err| {
        tracing::warn!("live notifications: failed to count messages: {}", err);
    })?;
    message["unread"] = json!(unread);
    message["unread_messages"] = json!(unread_messages);
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_must_match_host() {
        let headers = |origin: Option<&str>, host: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            if let Some(origin) = origin {
                headers.insert(header::ORIGIN, origin.parse().unwrap());
            }
            headers
        };
        assert!(same_origin(&headers(Some("https://example.com"), "example.com")));
        assert!(same_origin(&headers(Some("http://127.0.0.1:8080"), "127.0.0.1:8080")));
        assert!(same_origin(&headers(None, "example.com")));
        assert!(!same_origin(&headers(Some("https://evil.example"), "example.com")));
        assert!(!same_origin(&headers(Some("null"), "example.com")));
    }
}
