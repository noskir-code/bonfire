//! Link previews: a background task looks at new and edited posts (see
//! `entities::preview`), fetches the page of their first link, and reads
//! its card from the tags sites write for Mastodon, Facebook and Twitter
//! (OpenGraph `og:title`, `og:image`..., else `<title>` and `description`).
//!
//! Pages are fetched by the server like the media proxy (see `API::Web`):
//! public addresses only, and visitors never contact the site; the image is
//! shown through the media proxy too.

use std::{sync::Arc, time::Duration};

use axum::http::header;
use reqwest::Url;
use sqlx::PgPool;
use tokio::sync::Notify;

use crate::{
    API::{Web, activities::decode_entities, linker::check_url},
    entities::preview::{self, LinkPreview},
    linkify,
};

/// Only the `<head>` of a page is needed, a large page is cut there.
const MAX_PAGE_BYTES: usize = 512 * 1024;
const TITLE_MAX_CHARS: usize = 200;
const DESCRIPTION_MAX_CHARS: usize = 300;
const SITE_MAX_CHARS: usize = 80;
/// Posts looked at in one go.
const BATCH: i64 = 10;

/// Looks at posts not checked yet, forever: woken by `wake` when a post is
/// written here, else every minute (posts of other servers).
pub async fn run(web: Arc<Web>, pool: PgPool, wake: Arc<Notify>) {
    loop {
        let posts = match preview::posts_to_check(&pool, BATCH).await {
            Ok(posts) => posts,
            Err(err) => {
                tracing::warn!("failed to load posts for link previews: {}", err);
                Vec::new()
            }
        };
        if posts.is_empty() {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                _ = wake.notified() => {}
            }
            continue;
        }
        for (_, body) in &posts {
            let Some(url) = linkify::first_link(body) else {
                continue;
            };
            match preview::is_fresh(&pool, &url).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!("failed to check the preview of {}: {}", url, err);
                    continue;
                }
            }
            let found = match fetch(&web, &url).await {
                Ok(found) => found,
                Err(err) => {
                    tracing::info!("no preview for {}: {}", url, err);
                    None
                }
            };
            if let Err(err) = preview::save(&pool, &url, found.as_ref()).await {
                tracing::warn!("failed to save the preview of {}: {}", url, err);
            }
        }
        let ids: Vec<i64> = posts.iter().map(|(id, _)| *id).collect();
        if let Err(err) = preview::mark_checked(&pool, &ids).await {
            tracing::warn!("failed to mark posts checked for previews: {}", err);
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

/// The card of the page at `url`, `None` when it has none.
async fn fetch(web: &Web, url: &str) -> Result<Option<LinkPreview>, String> {
    let parsed = Url::parse(url).map_err(|err| format!("invalid address: {err}"))?;
    check_url(&parsed, web.allow_insecure)?;
    let mut response = web
        .pages
        .get(parsed)
        .header(header::ACCEPT, "text/html, application/xhtml+xml")
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("answered {}", response.status()));
    }
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.contains("html"));
    if !is_html {
        return Ok(None);
    }
    // After redirects: images may be relative to it.
    let page_url = response.url().clone();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| format!("failed to read: {err}"))? {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_PAGE_BYTES || find_ignoring_case(&body, b"</head") {
            break;
        }
    }
    body.truncate(MAX_PAGE_BYTES);
    Ok(parse(&String::from_utf8_lossy(&body), &page_url, url))
}

fn find_ignoring_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

/// The attributes of a tag, from after its name to its `>`: names in
/// lowercase, values decoded.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = tag;
    loop {
        rest = rest.trim_start_matches(|letter: char| letter.is_whitespace() || letter == '/');
        let name_end = rest
            .find(|letter: char| letter.is_whitespace() || matches!(letter, '=' | '>' | '/'))
            .unwrap_or(rest.len());
        if name_end == 0 {
            break;
        }
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        let Some(after_equal) = rest.strip_prefix('=') else {
            found.push((name, String::new()));
            continue;
        };
        let after_equal = after_equal.trim_start();
        let (value, after) = match after_equal.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let inside = &after_equal[1..];
                match inside.find(quote) {
                    Some(end) => (&inside[..end], &inside[end + 1..]),
                    None => (inside, ""),
                }
            }
            _ => {
                let end = after_equal
                    .find(|letter: char| letter.is_whitespace() || letter == '>')
                    .unwrap_or(after_equal.len());
                (&after_equal[..end], &after_equal[end..])
            }
        };
        found.push((name, decode_entities(value)));
        rest = after;
    }
    found
}

/// One line, spaces collapsed, at most `max_chars`.
fn clean(text: &str, max_chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
        None => text,
    }
}

/// Reads the card from the `<head>` of a page. `page_url` resolves relative
/// image addresses, `url` is the address in the post.
fn parse(html: &str, page_url: &Url, url: &str) -> Option<LinkPreview> {
    let lower = html.to_ascii_lowercase();
    let head_end = lower.find("</head").or_else(|| lower.find("<body")).unwrap_or(html.len());
    let (html, lower) = (&html[..head_end], &lower[..head_end]);

    let mut metas: Vec<(String, String)> = Vec::new();
    let mut title_tag = None;
    let mut position = 0;
    while let Some(offset) = lower[position..].find('<') {
        let start = position + offset;
        let Some(length) = lower[start..].find('>') else {
            break;
        };
        let tag = &html[start + 1..start + length];
        let tag_lower = &lower[start + 1..start + length];
        if let Some(inside) = tag_lower.strip_prefix("meta").filter(|rest| rest.starts_with(char::is_whitespace)) {
            let attributes = attributes(&tag[tag.len() - inside.len()..]);
            let get = |name: &str| attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
            if let (Some(key), Some(content)) = (get("property").or_else(|| get("name")), get("content")) {
                metas.push((key.to_ascii_lowercase(), content));
            }
        } else if title_tag.is_none() && (tag_lower == "title" || tag_lower.starts_with("title ")) {
            let text_start = start + length + 1;
            let text_end = lower[text_start..].find("</title").map_or(lower.len(), |end| text_start + end);
            title_tag = Some(decode_entities(&html[text_start..text_end]));
        }
        position = start + length + 1;
    }

    let meta = |names: &[&str]| {
        names.iter().find_map(|name| {
            metas
                .iter()
                .find(|(key, value)| key == name && !value.trim().is_empty())
                .map(|(_, value)| value.clone())
        })
    };
    let title = clean(&meta(&["og:title", "twitter:title"]).or(title_tag).unwrap_or_default(), TITLE_MAX_CHARS);
    if title.is_empty() {
        return None;
    }
    let image_url = meta(&["og:image:secure_url", "og:image", "og:image:url", "twitter:image", "twitter:image:src"])
        .and_then(|image| page_url.join(image.trim()).ok())
        .filter(|image| matches!(image.scheme(), "https" | "http"))
        .map(String::from);
    Some(LinkPreview {
        url: url.to_string(),
        title,
        description: clean(
            &meta(&["og:description", "twitter:description", "description"]).unwrap_or_default(),
            DESCRIPTION_MAX_CHARS,
        ),
        site_name: clean(&meta(&["og:site_name", "application-name"]).unwrap_or_default(), SITE_MAX_CHARS),
        image_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Url {
        Url::parse("https://example.com/articles/one").unwrap()
    }

    #[test]
    fn opengraph_is_read() {
        let html = r#"<!doctype html><html><head>
            <title>Fallback</title>
            <meta property="og:title" content="Rust &amp; Tison">
            <META NAME='description' CONTENT='A short   text
              on two lines'>
            <meta property=og:image content="/img/card.png" />
            <meta property="og:site_name" content="Example">
            </head><body><meta property="og:title" content="Not this"></body>"#;
        let found = parse(html, &page(), "https://example.com/a").unwrap();
        assert_eq!(found.title, "Rust & Tison");
        assert_eq!(found.description, "A short text on two lines");
        assert_eq!(found.image_url.as_deref(), Some("https://example.com/img/card.png"));
        assert_eq!(found.site_name, "Example");
    }

    #[test]
    fn title_tag_is_the_fallback() {
        let found = parse("<html><head><title> Just a page </title></head>", &page(), "u").unwrap();
        assert_eq!((found.title.as_str(), found.image_url), ("Just a page", None));
        assert!(parse("<html><head></head><body>No title</body>", &page(), "u").is_none());
    }

    #[test]
    fn unsafe_images_are_dropped() {
        let html = r#"<head><meta property="og:title" content="x"><meta property="og:image" content="javascript:alert(1)"></head>"#;
        assert_eq!(parse(html, &page(), "u").unwrap().image_url, None);
    }

    #[test]
    fn long_texts_are_cut() {
        let html = format!(r#"<head><meta property="og:title" content="{}"></head>"#, "word ".repeat(100));
        let title = parse(&html, &page(), "u").unwrap().title;
        assert!(title.chars().count() <= TITLE_MAX_CHARS + 1 && title.ends_with('…'));
    }
}
