//! The media proxy: `/media/{hash}` serves a picture, video or sound of
//! another server or the image of a link preview (see `entities::media`),
//! downloaded once into `CACHE_DIR`.
//! Visitors never contact the other server, and videos can be seeked since
//! the cached file is served with ranges.

use std::{
    path::{Path as FilePath, PathBuf},
    time::{Duration, SystemTime},
};

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{AppState, API::linker, entities::media};

const CACHE_DIR: &str = "cache/media";
/// Larger files aren't proxied: a few minutes of video.
const MAX_MEDIA_BYTES: usize = 64 * 1024 * 1024;
/// Cached files unused for this long are deleted, see `clean_cache`.
const KEEP_DAYS: u64 = 30;

fn is_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn cached_path(hash: &str, extension: &str) -> PathBuf {
    PathBuf::from(CACHE_DIR).join(format!("{hash}.{extension}"))
}

pub async fn proxy(State(state): State<AppState>, Path(hash): Path<String>, request: Request) -> Response {
    let Some(pool) = &state.pool else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let web = &state.web;
    if !is_hash(&hash) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let proxied = match media::find(pool, &hash).await {
        Ok(Some(proxied)) => proxied,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!("failed to load media {}: {}", hash, err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let cached = match &proxied.extension {
        Some(extension) if tokio::fs::try_exists(cached_path(&hash, extension)).await.unwrap_or(false) => {
            cached_path(&hash, extension)
        }
        _ if proxied.recently_failed => return StatusCode::NOT_FOUND.into_response(),
        _ => match download(web, &hash, &proxied.url).await {
            Ok(extension) => {
                if let Err(err) = media::set_cached(pool, &hash, extension).await {
                    tracing::warn!("failed to save media {}: {}", hash, err);
                }
                cached_path(&hash, extension)
            }
            Err(err) => {
                tracing::info!("failed to proxy {}: {}", proxied.url, err);
                if let Err(err) = media::set_failed(pool, &hash).await {
                    tracing::warn!("failed to save media {}: {}", hash, err);
                }
                return StatusCode::BAD_GATEWAY.into_response();
            }
        },
    };

    // Ranges, If-Modified-Since and the content type come from ServeFile.
    let Ok(response) = ServeFile::new(&cached).oneshot(request).await;
    let mut response = response.map(Body::new);
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=604800"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // Never run as a page, whatever the file holds.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    response
}

/// Downloads into the cache, returning the file's extension.
async fn download(web: &crate::API::Web, hash: &str, url: &str) -> Result<&'static str, String> {
    tokio::fs::create_dir_all(CACHE_DIR)
        .await
        .map_err(|err| format!("failed to create {CACHE_DIR}: {err}"))?;
    let downloaded = PathBuf::from(CACHE_DIR).join(format!("{hash}.download"));
    let extension = linker::download_media(web, url, &downloaded, MAX_MEDIA_BYTES).await?;
    tokio::fs::rename(&downloaded, cached_path(hash, extension))
        .await
        .map_err(|err| format!("failed to save {hash}: {err}"))?;
    Ok(extension)
}

/// Deletes cached files not used for `KEEP_DAYS`, and downloads left
/// unfinished. They are downloaded again when needed.
pub async fn clean_cache() {
    let Ok(mut entries) = tokio::fs::read_dir(CACHE_DIR).await else {
        return;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        // Reading a file doesn't always update its access time: the last
        // modification is used, the download.
        let age = metadata
            .accessed()
            .ok()
            .max(metadata.modified().ok())
            .and_then(|time| now.duration_since(time).ok())
            .unwrap_or_default();
        let unfinished = is_unfinished(&path);
        let limit = if unfinished { Duration::from_secs(3600) } else { Duration::from_secs(KEEP_DAYS * 86400) };
        if age > limit && tokio::fs::remove_file(&path).await.is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!("deleted {} old cached media", removed);
    }
}

fn is_unfinished(path: &FilePath) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "part" | "download"))
}
