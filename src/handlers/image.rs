use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::extract::Multipart;

use crate::entities::user::ProfileImage;

pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// Multipart field holding the uploaded file.
pub const IMAGE_FIELD: &str = "image";

const UPLOADS_DIR: &str = "public/uploads/users";
const UPLOADS_URL: &str = "/public/uploads/users";

#[derive(Debug)]
pub enum ImageError {
    Missing,
    TooLarge,
    UnsupportedFormat,
    Io(std::io::Error),
}

/// Detects the format from the file signature, never from the name sent by the browser.
/// SVG is not accepted since it can contain scripts.
fn detect_extension(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("gif")
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("webp")
    } else if data.len() >= 12 && &data[4..12] == b"ftypavif" {
        Some("avif")
    } else {
        None
    }
}

async fn read_image_field(multipart: &mut Multipart) -> Result<Vec<u8>, ImageError> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ImageError::Missing)?
    {
        if field.name() != Some(IMAGE_FIELD) {
            continue;
        }
        // Fails when the body goes over the global `DefaultBodyLimit`.
        let data = field.bytes().await.map_err(|_| ImageError::TooLarge)?;
        if data.is_empty() {
            return Err(ImageError::Missing);
        }
        if data.len() > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge);
        }
        return Ok(data.to_vec());
    }
    Err(ImageError::Missing)
}

/// Saves the uploaded image in the user's folder and returns its public URL.
pub async fn save_user_image(
    user_id: i32,
    image: ProfileImage,
    multipart: &mut Multipart,
) -> Result<String, ImageError> {
    let data = read_image_field(multipart).await?;
    let extension = detect_extension(&data).ok_or(ImageError::UnsupportedFormat)?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let file_name = format!("{}-{nanos}.{extension}", image.as_str());

    let dir = PathBuf::from(UPLOADS_DIR).join(user_id.to_string());
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(ImageError::Io)?;
    tokio::fs::write(dir.join(&file_name), &data)
        .await
        .map_err(ImageError::Io)?;

    Ok(format!("{UPLOADS_URL}/{user_id}/{file_name}"))
}

/// Deletes a file previously returned by `save_user_image`.
/// URLs outside the user's upload folder are ignored.
pub async fn remove_user_image(user_id: i32, url: &str) {
    let prefix = format!("{UPLOADS_URL}/{user_id}/");
    let Some(file_name) = url.strip_prefix(&prefix) else {
        return;
    };
    if file_name.is_empty() || file_name.contains('/') || file_name.contains("..") {
        return;
    }

    let path = PathBuf::from(UPLOADS_DIR)
        .join(user_id.to_string())
        .join(file_name);
    if let Err(err) = tokio::fs::remove_file(&path).await {
        tracing::warn!("failed to remove {}: {}", path.display(), err);
    }
}
