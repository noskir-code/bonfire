use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::extract::Multipart;


pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// Multipart field holding the uploaded file.
pub const IMAGE_FIELD: &str = "image";

const UPLOADS_DIR: &str = "public/uploads";
const UPLOADS_URL: &str = "/public/uploads";

/// Whose folder an uploaded image goes in.
#[derive(Clone, Copy, Debug)]
pub enum ImageOwner {
    User(i32),
    Community(i64),
}

impl ImageOwner {
    /// Folder under the uploads, like `users/12`.
    fn folder(self) -> String {
        match self {
            Self::User(id) => format!("users/{id}"),
            Self::Community(id) => format!("communities/{id}"),
        }
    }
}

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

/// Saves the uploaded image in the owner's folder and returns its public URL.
/// `kind` starts the file name, like `photo`.
pub async fn save_image(owner: ImageOwner, kind: &str, multipart: &mut Multipart) -> Result<String, ImageError> {
    let data = read_image_field(multipart).await?;
    let extension = detect_extension(&data).ok_or(ImageError::UnsupportedFormat)?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let file_name = format!("{kind}-{nanos}.{extension}");

    let folder = owner.folder();
    let dir = PathBuf::from(UPLOADS_DIR).join(&folder);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(ImageError::Io)?;
    tokio::fs::write(dir.join(&file_name), &data)
        .await
        .map_err(ImageError::Io)?;

    Ok(format!("{UPLOADS_URL}/{folder}/{file_name}"))
}

/// Deletes a file previously returned by `save_image`.
/// URLs outside the owner's upload folder are ignored.
pub async fn remove_image(owner: ImageOwner, url: &str) {
    let folder = owner.folder();
    let prefix = format!("{UPLOADS_URL}/{folder}/");
    let Some(file_name) = url.strip_prefix(&prefix) else {
        return;
    };
    if file_name.is_empty() || file_name.contains('/') || file_name.contains("..") {
        return;
    }

    let path = PathBuf::from(UPLOADS_DIR).join(&folder).join(file_name);
    if let Err(err) = tokio::fs::remove_file(&path).await {
        tracing::warn!("failed to remove {}: {}", path.display(), err);
    }
}
