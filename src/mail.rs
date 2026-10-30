//! Outgoing email, through the SMTP server set in `configs/runtime.conf`.
//!
//! For development, see `for_development`: emails go to Mailpit when it runs,
//! else they are printed in the log.
//!
//! Each email has a plain text version and an HTML one, both Askama templates
//! in `templates/emails/` (see `Email::render`). The HTML uses classes from
//! `assets/email.css`, turned into inline styles by `inline_css`.

use std::{collections::HashMap, sync::OnceLock, time::Duration};

use askama::Template;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MultiPart, header::ContentType},
    transport::smtp::authentication::Credentials,
};

use crate::settings::{SmtpConfig, SmtpSecurity};

/// Mailpit's web inbox when it runs with its default ports.
const MAILPIT_INBOX: &str = "http://127.0.0.1:8025";

/// Built into the server, so it can't be missing at runtime.
const EMAIL_CSS: &str = include_str!("../assets/email.css");

/// `email.css` as class name -> declarations. Only `.class` selectors are
/// supported, other rules are logged and skipped.
fn email_styles() -> &'static HashMap<String, String> {
    static STYLES: OnceLock<HashMap<String, String>> = OnceLock::new();
    STYLES.get_or_init(|| parse_css(EMAIL_CSS))
}

fn parse_css(css: &str) -> HashMap<String, String> {
    // Without comments, the file is a list of `selectors { declarations }`.
    let mut without_comments = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        without_comments.push_str(&rest[..start]);
        rest = rest[start..].find("*/").map_or("", |end| &rest[start + end + 2..]);
    }
    without_comments.push_str(rest);

    let mut styles: HashMap<String, String> = HashMap::new();
    for rule in without_comments.split('}') {
        let Some((selectors, declarations)) = rule.split_once('{') else {
            continue;
        };
        // One line, each declaration ending with `;`.
        let declarations = declarations
            .split(';')
            .map(|declaration| declaration.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|declaration| !declaration.is_empty())
            .map(|declaration| format!("{declaration};"))
            .collect::<Vec<_>>()
            .join(" ");

        for selector in selectors.split(',').map(str::trim) {
            let class = selector.strip_prefix('.').filter(|class| {
                !class.is_empty()
                    && class
                        .chars()
                        .all(|letter| letter.is_ascii_alphanumeric() || letter == '-' || letter == '_')
            });
            match class {
                Some(class) => {
                    let entry = styles.entry(class.to_string()).or_default();
                    if !entry.is_empty() {
                        entry.push(' ');
                    }
                    entry.push_str(&declarations);
                }
                None => tracing::warn!("email.css: selector '{}' is not supported, skipped", selector),
            }
        }
    }
    styles
}

/// Replaces every `class="..."` with the matching `style="..."`. Unknown
/// classes are returned so they can be reported. Template values can't add a
/// `class="` since Askama escapes their quotes.
fn inline_css(html: &str, styles: &HashMap<String, String>) -> (String, Vec<String>) {
    const ATTRIBUTE: &str = "class=\"";
    let mut inlined = String::with_capacity(html.len() * 2);
    let mut unknown = Vec::new();
    let mut rest = html;

    while let Some(position) = rest.find(ATTRIBUTE) {
        // Only a real attribute, preceded by a space or a line break.
        let is_attribute = rest[..position].ends_with(char::is_whitespace);
        let after = &rest[position + ATTRIBUTE.len()..];
        let Some(end) = after.find('"').filter(|_| is_attribute) else {
            inlined.push_str(&rest[..position + ATTRIBUTE.len()]);
            rest = after;
            continue;
        };

        let style = after[..end]
            .split_whitespace()
            .filter_map(|class| {
                let found = styles.get(class).map(String::as_str);
                if found.is_none() {
                    unknown.push(class.to_string());
                }
                found
            })
            .collect::<Vec<_>>()
            .join(" ")
            .replace('"', "&quot;");

        inlined.push_str(&rest[..position]);
        if !style.is_empty() {
            inlined.push_str("style=\"");
            inlined.push_str(&style);
            inlined.push('"');
        }
        rest = &after[end + 1..];
    }
    inlined.push_str(rest);
    (inlined, unknown)
}

/// An email ready to send.
pub struct Email {
    pub subject: String,
    /// For clients that don't show HTML, and for spam filters which prefer both.
    pub text: String,
    /// `None` sends the text version only.
    pub html: Option<String>,
}

impl Email {
    /// Renders both versions. If the HTML one fails, the email still goes out
    /// as text only: better than no email at all.
    pub fn render(subject: &str, text: &impl Template, html: &impl Template) -> Result<Self, String> {
        let text = text
            .render()
            .map_err(|err| format!("failed to render email text: {err}"))?;
        let html = html
            .render()
            .map_err(|err| tracing::error!("failed to render email HTML, sending text only: {}", err))
            .ok()
            .map(|html| {
                let (inlined, unknown) = inline_css(&html, email_styles());
                if !unknown.is_empty() {
                    tracing::warn!("email '{}' uses classes missing from email.css: {}", subject, unknown.join(", "));
                }
                inlined
            });
        Ok(Self {
            subject: subject.to_string(),
            text,
            html,
        })
    }
}

enum Delivery {
    Smtp(AsyncSmtpTransport<Tokio1Executor>),
    /// Development only: the email is printed in the log instead of sent.
    Log,
}

pub struct Mailer {
    delivery: Delivery,
    from: Mailbox,
    /// Shown in the startup log.
    description: String,
}

impl Mailer {
    pub fn new(config: &SmtpConfig) -> Result<Self, String> {
        let builder = match config.security {
            SmtpSecurity::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host),
            SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host),
            SmtpSecurity::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)),
        }
        .map_err(|err| format!("invalid SMTP_HOST: {err}"))?
        .port(config.port)
        // A mail server that doesn't answer must not hang a request for long.
        .timeout(Some(Duration::from_secs(10)));

        let builder = match (&config.username, &config.password) {
            (Some(username), Some(password)) => {
                builder.credentials(Credentials::new(username.clone(), password.clone()))
            }
            _ => builder,
        };

        let description = if config.is_mailpit {
            format!("Mailpit at {}:{}, inbox at {MAILPIT_INBOX}", config.host, config.port)
        } else {
            format!("SMTP server {}:{}", config.host, config.port)
        };

        Ok(Self {
            delivery: Delivery::Smtp(builder.build()),
            from: parse_from(&config.from)?,
            description,
        })
    }

    /// Mailer for debug builds without SMTP settings: Mailpit if it runs on
    /// its default port, else the log.
    pub async fn for_development() -> Self {
        let mailpit = SmtpConfig::mailpit();
        if let Ok(mailer) = Self::new(&mailpit)
            && mailer.check().await.is_ok()
        {
            return mailer;
        }
        Self {
            delivery: Delivery::Log,
            from: parse_from(&mailpit.from).expect("the Mailpit sender is valid"),
            description: format!(
                "the log (start Mailpit to see them in an inbox at {MAILPIT_INBOX})"
            ),
        }
    }

    /// Where emails go, for the startup log.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Checks the mail server answers. Always succeeds for the log.
    pub async fn check(&self) -> Result<(), String> {
        match &self.delivery {
            Delivery::Smtp(transport) => match transport.test_connection().await {
                Ok(true) => Ok(()),
                Ok(false) => Err("the server did not answer".to_string()),
                Err(err) => Err(err.to_string()),
            },
            Delivery::Log => Ok(()),
        }
    }

    pub async fn send(&self, to: &str, email: Email) -> Result<(), String> {
        let recipient: Mailbox = to.parse().map_err(|err| format!("invalid recipient: {err}"))?;
        let transport = match &self.delivery {
            Delivery::Smtp(transport) => transport,
            Delivery::Log => {
                tracing::warn!(
                    "email not sent (development, text version only), to {}: {}\n{}",
                    recipient,
                    email.subject,
                    email.text
                );
                return Ok(());
            }
        };
        let builder = Message::builder()
            .from(self.from.clone())
            .to(recipient)
            .subject(email.subject);
        let message = match email.html {
            // Clients show the last version they support: HTML, else text.
            Some(html) => builder.multipart(MultiPart::alternative_plain_html(email.text, html)),
            None => builder.header(ContentType::TEXT_PLAIN).body(email.text),
        }
        .map_err(|err| format!("failed to build email: {err}"))?;
        transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|err| format!("failed to send email: {err}"))
    }
}

fn parse_from(from: &str) -> Result<Mailbox, String> {
    from.parse().map_err(|err| format!("invalid SMTP_FROM: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_become_inline_styles() {
        let styles = parse_css(
            "/* comment */ .a { color: red; }\n.b, .c {\n  margin: 0 0 16px;\n  font-family: 'Segoe UI', sans-serif;\n}\n.c { padding: 0 }",
        );
        assert_eq!(styles["a"], "color: red;");
        assert_eq!(styles["c"], "margin: 0 0 16px; font-family: 'Segoe UI', sans-serif; padding: 0;");

        let (html, unknown) = inline_css(
            "<p class=\"a b\">x</p><td\n    class=\"missing\">",
            &styles,
        );
        assert_eq!(
            html,
            "<p style=\"color: red; margin: 0 0 16px; font-family: 'Segoe UI', sans-serif;\">x</p><td\n    >"
        );
        assert_eq!(unknown, ["missing"]);
    }

    #[test]
    fn only_attributes_are_replaced() {
        // Not preceded by a space or line break: part of a word, kept as is.
        let (html, unknown) = inline_css("<p>xclass=\"a\"</p>", &HashMap::new());
        assert_eq!(html, "<p>xclass=\"a\"</p>");
        assert!(unknown.is_empty());
    }

    #[test]
    fn email_css_is_fully_supported() {
        let styles = email_styles();
        for class in ["page", "card", "button", "note", "spaced", "link"] {
            assert!(styles.contains_key(class), "missing .{class}");
        }
    }
}
