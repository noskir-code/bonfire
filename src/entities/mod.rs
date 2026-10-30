pub mod comment;
pub mod post;
pub mod schema;
pub mod session;
pub mod user;

/// Tables kept up to date at startup. A table must come after the tables it references.
pub const TABLES: &[&schema::Table] = &[
    &user::TABLE,
    &post::TABLE,
    &post::LIKES_TABLE,
    &post::SHARES_TABLE,
    &comment::TABLE,
    &session::TABLE,
];
