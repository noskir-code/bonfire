pub mod author;
pub mod comment;
pub mod community;
pub mod federation;
pub mod notification;
pub mod password_reset;
pub mod post;
pub mod schema;
pub mod session;
pub mod user;

/// Tables kept up to date at startup. A table must come after the tables it references.
pub const TABLES: &[&schema::Table] = &[
    &user::TABLE,
    &federation::ACTORS_TABLE,
    &community::TABLE,
    &post::TABLE,
    &post::LIKES_TABLE,
    &post::SHARES_TABLE,
    &comment::TABLE,
    &community::MEMBERS_TABLE,
    &community::MODERATORS_TABLE,
    &community::RULES_TABLE,
    &community::IGNORES_TABLE,
    &session::TABLE,
    &password_reset::TABLE,
    &federation::FOLLOWERS_TABLE,
    &federation::COMMUNITY_FOLLOWERS_TABLE,
    &federation::DELIVERIES_TABLE,
    &notification::TABLE,
];
