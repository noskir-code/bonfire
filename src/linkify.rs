//! Links in the text of posts and comments: `https://…`, `http://…` and
//! `www.…` become clickable. The text is plain, so everything else is
//! escaped: the result is safe to put in a page as HTML.
//!
//! Used for the pages (see `Post::body_html`) and for the Notes sent to
//! other Fediverse servers (see `API::activities::text_to_html`).

/// Characters ending an address at the end of a sentence, never kept in it:
/// in "see https://example.com." the dot is the sentence's.
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', '\'', '"', '*', '_', '~'];
/// Addresses longer than this are shown cut (the link stays whole).
const MAX_SHOWN_CHARS: usize = 60;

/// Escapes `<`, `>`, `&`, `"` and `'` for HTML text and attributes.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for letter in text.chars() {
        match letter {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(letter),
        }
    }
    escaped
}

/// An address found in a text: where it is, and where it leads.
#[derive(Debug, PartialEq, Eq)]
struct Found {
    start: usize,
    end: usize,
    href: String,
}

/// Whether an address can start at `position`: not inside a word
/// ("awww.example" or "xhttps://" are not addresses).
fn starts_word(text: &str, position: usize) -> bool {
    text[..position]
        .chars()
        .next_back()
        .is_none_or(|before| !before.is_alphanumeric() && !matches!(before, '/' | '@' | '.' | '-' | '_'))
}

/// The end of the address starting at `start`: up to a space or a character
/// that can't be in an address, without the punctuation ending a sentence,
/// and without a `)` or `]` it doesn't open itself ("(see https://x.org)").
fn address_end(text: &str, start: usize) -> usize {
    let rest = &text[start..];
    let mut end = rest
        .find(|letter: char| letter.is_whitespace() || letter.is_control() || matches!(letter, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^'))
        .unwrap_or(rest.len());
    loop {
        let address = &rest[..end];
        let Some(last) = address.chars().next_back() else {
            break;
        };
        let unbalanced = match last {
            ')' => address.matches(')').count() > address.matches('(').count(),
            ']' => address.matches(']').count() > address.matches('[').count(),
            _ => false,
        };
        if TRAILING.contains(&last) || unbalanced {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    start + end
}

/// The host of `address` after its scheme or `www.`: letters, digits, `-`
/// and `.`, with a dot and no empty label ("https://." isn't an address).
fn has_host(host_and_more: &str) -> bool {
    let host = host_and_more
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let host = host.split(':').next().unwrap_or_default();
    host.contains('.')
        && host.split('.').all(|label| !label.is_empty())
        && host.chars().all(|letter| letter.is_alphanumeric() || matches!(letter, '-' | '.'))
}

/// Every address of the text, in order.
fn find_addresses(text: &str) -> Vec<Found> {
    let lower = text.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut position = 0;
    while position < text.len() {
        let next = ["https://", "http://", "www."]
            .iter()
            .filter_map(|prefix| lower[position..].find(prefix).map(|offset| (position + offset, *prefix)))
            .min_by_key(|(start, _)| *start);
        let Some((start, prefix)) = next else {
            break;
        };
        let end = address_end(text, start);
        let address = &text[start..end];
        // The trimmed punctuation may leave less than the prefix ("www.").
        let valid = starts_word(text, start) && address.get(prefix.len()..).is_some_and(has_host);
        if valid {
            let href = if prefix == "www." { format!("https://{address}") } else { address.to_string() };
            found.push(Found { start, end, href });
            position = end;
        } else {
            position = start + prefix.len();
        }
    }
    found
}

/// Where the first address of the text leads, for its preview.
pub fn first_link(text: &str) -> Option<String> {
    find_addresses(text).into_iter().next().map(|found| found.href)
}

/// What is shown for an address: without `https://`, cut when very long.
fn shown(address: &str) -> String {
    let without_scheme = address
        .strip_prefix("https://")
        .or_else(|| address.strip_prefix("http://"))
        .unwrap_or(address);
    match without_scheme.char_indices().nth(MAX_SHOWN_CHARS) {
        Some((cut, _)) => format!("{}…", &without_scheme[..cut]),
        None => without_scheme.to_string(),
    }
}

/// The text as HTML: escaped, with its addresses as links opening in a new
/// tab. Line breaks are kept as they are (the pages show them with CSS).
pub fn to_html(text: &str) -> String {
    let mut html = String::with_capacity(text.len() + 32);
    let mut written = 0;
    for address in find_addresses(text) {
        html.push_str(&escape(&text[written..address.start]));
        html.push_str(&format!(
            "<a href=\"{}\" rel=\"nofollow noopener noreferrer ugc\" target=\"_blank\">{}</a>",
            escape(&address.href),
            escape(&shown(&text[address.start..address.end])),
        ));
        written = address.end;
    }
    html.push_str(&escape(&text[written..]));
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(href: &str, text: &str) -> String {
        format!("<a href=\"{href}\" rel=\"nofollow noopener noreferrer ugc\" target=\"_blank\">{text}</a>")
    }

    #[test]
    fn addresses_become_links() {
        assert_eq!(
            to_html("See https://example.com/a?b=1&c=2 now"),
            format!("See {} now", link("https://example.com/a?b=1&amp;c=2", "example.com/a?b=1&amp;c=2"))
        );
        assert_eq!(to_html("www.example.org"), link("https://www.example.org", "www.example.org"));
        assert_eq!(to_html("HTTP://Example.com"), link("HTTP://Example.com", "HTTP://Example.com"));
    }

    #[test]
    fn sentence_punctuation_stays_out() {
        assert_eq!(to_html("Go to www.example.org."), format!("Go to {}.", link("https://www.example.org", "www.example.org")));
        assert_eq!(
            to_html("(see https://en.wikipedia.org/wiki/Rust_(language))"),
            format!(
                "(see {})",
                link("https://en.wikipedia.org/wiki/Rust_(language)", "en.wikipedia.org/wiki/Rust_(language)")
            )
        );
        assert_eq!(to_html("\"https://example.com\""), format!("&quot;{}&quot;", link("https://example.com", "example.com")));
    }

    #[test]
    fn not_addresses() {
        assert_eq!(to_html("awww.example.org"), "awww.example.org");
        assert_eq!(to_html("https:// and www."), "https:// and www.");
        assert_eq!(to_html("www.nothing"), "www.nothing");
        assert_eq!(to_html("mail@www.example.org"), "mail@www.example.org");
    }

    #[test]
    fn everything_is_escaped() {
        assert_eq!(to_html("<b>hi</b> & 'x'"), "&lt;b&gt;hi&lt;/b&gt; &amp; &#39;x&#39;");
        // Quotes can't leave the attribute: they end the address.
        assert_eq!(
            to_html("https://example.com\"onmouseover=\"alert(1)"),
            format!("{}&quot;onmouseover=&quot;alert(1)", link("https://example.com", "example.com"))
        );
        assert_eq!(to_html("javascript:alert(1)"), "javascript:alert(1)");
    }

    #[test]
    fn long_addresses_are_cut() {
        let long = format!("https://example.com/{}", "a".repeat(100));
        assert!(to_html(&long).contains(&format!(">example.com/{}…</a>", "a".repeat(48))));
        assert!(to_html(&long).contains(&format!("href=\"{long}\"")));
    }

    #[test]
    fn first_link_is_found() {
        assert_eq!(first_link("Look at www.a.org and https://b.org"), Some("https://www.a.org".to_string()));
        assert_eq!(first_link("Nothing here."), None);
    }

    #[test]
    fn unicode_is_kept() {
        assert_eq!(
            to_html("Café ☕ https://exämple.com/é !"),
            format!("Café ☕ {} !", link("https://exämple.com/é", "exämple.com/é"))
        );
    }
}
