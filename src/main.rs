use axum::{Router, extract::DefaultBodyLimit, routing::post};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tower_sessions::{
    Expiry, MemoryStore, SessionManagerLayer,
    cookie::{SameSite, time::Duration},
};
mod entities;
mod handlers;
mod pages;
mod settings;
use handlers::user;
use settings::Config;

/// Shared state given to every handler.
#[derive(Clone)]
pub struct AppState {
    /// `None` when no `DATABASE_URL` is configured.
    pub pool: Option<PgPool>,
    pub secure_cookies: bool,
    /// How long "stay signed in" lasts, see `handlers::session`.
    pub remember_days: i32,
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!("failed to install CTRL+C handler: {}", err);
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!("failed to install SIGTERM handler: {}", err);
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_runtime().map_err(io_error_from_string)?;
    settings::logger::init(&config.log_level);

    // Lazy: the server starts even if Postgres is down, queries fail until it is up.
    let pool = match &config.database_url {
        Some(database_url) => Some(
            PgPoolOptions::new()
                .max_connections(config.db_max_connections)
                .connect_lazy(database_url)?,
        ),
        None => {
            tracing::warn!("DATABASE_URL not set; running without database");
            None
        }
    };

    if let Some(pool) = &pool
        && let Err(err) = entities::schema::sync(pool, entities::TABLES).await
    {
        tracing::error!("failed to update the database schema: {}", err);
    }

    if let Some(pool) = &pool {
        match entities::session::delete_expired(pool).await {
            Ok(0) => {}
            Ok(count) => tracing::info!("deleted {} expired remember tokens", count),
            Err(err) => tracing::warn!("failed to delete expired remember tokens: {}", err),
        }
    }

    let state = AppState {
        pool,
        secure_cookies: config.secure_cookies,
        remember_days: config.remember_me_days,
    };

    let session_layer =SessionManagerLayer::new(MemoryStore::default())
        .with_secure(config.secure_cookies)
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_expiry(Expiry::OnInactivity(Duration::days(
            config.session_inactivity_days,
        )));

    let app = Router::new()
        .merge(pages::page_routes())
        .route("/settings/names", post(user::update_names))
        .route("/settings/bio", post(user::update_bio))
        .route("/settings/username", post(user::update_username))
        .route("/settings/photo", post(user::upload_photo))
        .route("/settings/photo/remove", post(user::remove_photo))
        .route("/settings/background", post(user::upload_background))
        .route("/settings/background/remove", post(user::remove_background))
        .fallback(handlers::system::not_found);

    let app = app
        .nest_service("/assets", ServeDir::new("assets"))
        .nest_service("/public", ServeDir::new("public"))
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(
            config.max_request_body_mb * 1024 * 1024,
        ))
        // Inside the session layer: it needs the session to sign users back in.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            handlers::session::restore_remembered,
        ))
        .layer(session_layer)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(config.addr()).await?;

    tracing::info!("Server running on http://{}", config.addr());

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn io_error_from_string(message: String) -> std::io::Error {
    std::io::Error::other(message)
}
