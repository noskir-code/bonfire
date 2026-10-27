use askama::Template;
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    title: String,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate<'a> {
    title: String,
    message: &'a str,
}

/// Error page keeping the site layout.
pub fn error_page(status: StatusCode, message: &str) -> Response {
    let template = ErrorTemplate {
        title: status.canonical_reason().unwrap_or("Error").to_string(),
        message,
    };
    (status, render_template(&template)).into_response()
}

pub fn render_template<T: Template>(template: &T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!("template rendering failed: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
        }
    }
}

pub async fn index() -> Response {
    let template = IndexTemplate {
        title: "Home".to_string(),
    };
    render_template(&template)
}
