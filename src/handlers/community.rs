//! Community pages: the directory, a community with its posts, and its
//! settings. Every action is a form that comes back to the page, with
//! `?error=` or `?saved=` to show the result.

use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use sqlx::PgPool;
use tower_sessions::Session;

use crate::{
    AppState,
    entities::{
        community::{
            self, Community, CreateError, DESCRIPTION_MAX_CHARS, Membership, Moderator, NAME_MAX_CHARS,
            RULE_BODY_MAX_CHARS, RULE_TITLE_MAX_CHARS, Rule, SLUG_MAX_LEN, SLUG_MIN_LEN, Visibility,
        },
        federation as store,
        post::{self, BODY_MAX_CHARS, BodyError, Post, Sort},
        user::User,
    },
    handlers::{
        image::{self, IMAGE_FIELD, ImageError, ImageOwner, MAX_IMAGE_BYTES},
        notifier,
        system::{Layout, error_page, local_path, render_template},
        user::signed_in_user,
    },
};

/// Results sent back to the pages as `?error=<code>`.
#[derive(Clone, Copy)]
enum CommunityError {
    InvalidSlug,
    SlugTaken,
    InvalidName,
    InvalidDescription,
    InvalidVisibility,
    InvalidRule,
    TooManyRules,
    NotMember,
    PostEmpty,
    PostTooLong,
    PostInvalidCharacters,
    ImageMissing,
    ImageTooLarge,
    ImageFormat,
    Server,
}

impl CommunityError {
    const ALL: [Self; 15] = [
        Self::InvalidSlug,
        Self::SlugTaken,
        Self::InvalidName,
        Self::InvalidDescription,
        Self::InvalidVisibility,
        Self::InvalidRule,
        Self::TooManyRules,
        Self::NotMember,
        Self::PostEmpty,
        Self::PostTooLong,
        Self::PostInvalidCharacters,
        Self::ImageMissing,
        Self::ImageTooLarge,
        Self::ImageFormat,
        Self::Server,
    ];

    fn code(self) -> &'static str {
        match self {
            Self::InvalidSlug => "invalid-slug",
            Self::SlugTaken => "slug-taken",
            Self::InvalidName => "invalid-name",
            Self::InvalidDescription => "invalid-description",
            Self::InvalidVisibility => "invalid-visibility",
            Self::InvalidRule => "invalid-rule",
            Self::TooManyRules => "too-many-rules",
            Self::NotMember => "not-member",
            Self::PostEmpty => "post-empty",
            Self::PostTooLong => "post-too-long",
            Self::PostInvalidCharacters => "post-invalid-characters",
            Self::ImageMissing => "image-missing",
            Self::ImageTooLarge => "image-too-large",
            Self::ImageFormat => "image-format",
            Self::Server => "server",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|error| error.code() == code)
    }

    fn message(self) -> String {
        match self {
            Self::InvalidSlug => format!(
                "The address must be {SLUG_MIN_LEN} to {SLUG_MAX_LEN} lowercase letters, digits or _."
            ),
            Self::SlugTaken => "This address is already used by a community or a user.".to_string(),
            Self::InvalidName => format!("The name must be 1 to {NAME_MAX_CHARS} characters."),
            Self::InvalidDescription => format!("The description can be at most {DESCRIPTION_MAX_CHARS} characters."),
            Self::InvalidVisibility => "Choose who can read the posts.".to_string(),
            Self::InvalidRule => format!(
                "A rule needs a title of at most {RULE_TITLE_MAX_CHARS} characters, and at most {RULE_BODY_MAX_CHARS} for its text."
            ),
            Self::TooManyRules => format!("A community can have at most {} rules.", community::MAX_RULES),
            Self::NotMember => "Only members can be moderators: they must join first.".to_string(),
            Self::PostEmpty => "Write something before publishing.".to_string(),
            Self::PostTooLong => format!("A post can be at most {BODY_MAX_CHARS} characters."),
            Self::PostInvalidCharacters => "The post contains characters that are not allowed.".to_string(),
            Self::ImageMissing => "Choose an image to upload.".to_string(),
            Self::ImageTooLarge => format!("The image can be at most {} MB.", MAX_IMAGE_BYTES / (1024 * 1024)),
            Self::ImageFormat => "Use a JPEG, PNG, GIF, WebP or AVIF image.".to_string(),
            Self::Server => "Something went wrong, try again later.".to_string(),
        }
    }
}

impl From<BodyError> for CommunityError {
    fn from(err: BodyError) -> Self {
        match err {
            BodyError::Empty => Self::PostEmpty,
            BodyError::TooLong => Self::PostTooLong,
            BodyError::InvalidCharacters => Self::PostInvalidCharacters,
        }
    }
}

impl From<ImageError> for CommunityError {
    fn from(err: ImageError) -> Self {
        match err {
            ImageError::Missing => Self::ImageMissing,
            ImageError::TooLarge => Self::ImageTooLarge,
            ImageError::UnsupportedFormat => Self::ImageFormat,
            ImageError::Io(err) => {
                tracing::error!("failed to save community image: {}", err);
                Self::Server
            }
        }
    }
}

fn server_error(err: impl std::fmt::Display) -> CommunityError {
    tracing::error!("community action failed: {}", err);
    CommunityError::Server
}

/// `?error=` read back on a page.
fn error_message(query: &HashMap<String, String>) -> Option<String> {
    query
        .get("error")
        .and_then(|code| CommunityError::from_code(code))
        .map(CommunityError::message)
}

/// Back to `url` with the result.
fn back(url: &str, result: Result<(), CommunityError>) -> Response {
    let location = match result {
        Ok(()) => format!("{url}?saved=1"),
        Err(error) => format!("{url}?error={}", error.code()),
    };
    Redirect::to(&location).into_response()
}

fn not_found(layout: Layout) -> Response {
    error_page(layout, StatusCode::NOT_FOUND, "This community does not exist.")
}

// --- Directory ---

#[derive(Template)]
#[template(path = "communities.html")]
struct CommunitiesTemplate {
    title: String,
    layout: Layout,
    communities: Vec<Community>,
    joined: Vec<Community>,
    search: String,
    error: Option<String>,
    visibilities: [Visibility; 2],
    slug_min_len: usize,
    slug_max_len: usize,
    name_max_chars: usize,
    description_max_chars: usize,
    /// `@slug@domain` hint under the address field, when federation is on.
    domain: Option<String>,
}

/// The directory, with the search (`?q=`) and the creation form.
pub async fn communities_page(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let layout = Layout::load(&state, &session).await;
    let Some(pool) = &state.pool else {
        return error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured.");
    };
    let search = query.get("q").map(|search| search.trim().to_string()).unwrap_or_default();
    let communities = community::list(pool, Some(&search).filter(|search| !search.is_empty()).map(String::as_str))
        .await
        .unwrap_or_else(|err| {
            tracing::error!("failed to list communities: {}", err);
            Vec::new()
        });
    let joined = match &layout.viewer {
        Some(viewer) => community::joined(pool, viewer.id).await.unwrap_or_else(|err| {
            tracing::error!("failed to list communities of {}: {}", viewer.username, err);
            Vec::new()
        }),
        None => Vec::new(),
    };
    render_template(&CommunitiesTemplate {
        title: "Communities".to_string(),
        layout,
        communities,
        joined,
        search,
        error: error_message(&query),
        visibilities: Visibility::ALL,
        slug_min_len: SLUG_MIN_LEN,
        slug_max_len: SLUG_MAX_LEN,
        name_max_chars: NAME_MAX_CHARS,
        description_max_chars: DESCRIPTION_MAX_CHARS,
        domain: state.federation.as_ref().map(|federation| federation.domain.clone()),
    })
}

pub async fn create_community(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (pool, me) = match signed_in_user(&state, &session).await {
        Ok(signed_in) => signed_in,
        Err(response) => return response,
    };
    let field = |name: &str| form.get(name).map(String::as_str).unwrap_or("");
    let result = async {
        let slug = community::validate_slug(field("slug")).ok_or(CommunityError::InvalidSlug)?;
        let name = community::validate_name(field("name")).ok_or(CommunityError::InvalidName)?;
        let description = community::validate_description(field("description")).ok_or(CommunityError::InvalidDescription)?;
        let visibility = Visibility::parse(field("visibility")).ok_or(CommunityError::InvalidVisibility)?;
        community::create(&pool, me.id, &slug, &name, &description, visibility)
            .await
            .map_err(|err| match err {
                CreateError::SlugTaken => CommunityError::SlugTaken,
                CreateError::Database(err) => server_error(err),
            })
    }
    .await;
    match result {
        Ok(created) => {
            tracing::info!("@{} created community {}", me.username, created.slug);
            Redirect::to(&format!("/community/{}", created.slug)).into_response()
        }
        Err(error) => Redirect::to(&format!("/communities?error={}#create", error.code())).into_response(),
    }
}

// --- One community ---

/// The community from the URL, and what the viewer is in it. A banned
/// community is only found by the site's staff.
struct Loaded {
    pool: PgPool,
    layout: Layout,
    community: Community,
    membership: Membership,
}

impl Loaded {
    fn viewer(&self) -> Option<&User> {
        self.layout.viewer.as_ref()
    }

    fn is_staff(&self) -> bool {
        self.viewer().is_some_and(User::is_staff)
    }

    fn can_manage(&self) -> bool {
        self.membership.can_manage() || self.is_staff()
    }
}

async fn load(state: &AppState, session: &Session, slug: &str) -> Result<Loaded, Response> {
    let layout = Layout::load(state, session).await;
    let Some(pool) = state.pool.clone() else {
        return Err(error_page(layout, StatusCode::SERVICE_UNAVAILABLE, "The database is not configured."));
    };
    let community = match community::find_by_slug(&pool, slug).await {
        Ok(Some(community)) => community,
        Ok(None) => return Err(not_found(layout)),
        Err(err) => {
            tracing::error!("failed to load community {}: {}", slug, err);
            return Err(error_page(layout, StatusCode::INTERNAL_SERVER_ERROR, "Failed to load this community."));
        }
    };
    let staff = layout.viewer.as_ref().is_some_and(User::is_staff);
    if community.is_banned() && !staff {
        return Err(error_page(
            layout,
            StatusCode::GONE,
            "This community was closed by the site's moderators.",
        ));
    }
    let membership = match &layout.viewer {
        Some(viewer) => match community::membership(&pool, &community, viewer.id).await {
            Ok(membership) => membership,
            Err(err) => {
                tracing::error!("failed to load membership: {}", err);
                Membership::default()
            }
        },
        None => Membership::default(),
    };
    Ok(Loaded {
        pool,
        layout,
        community,
        membership,
    })
}

/// Like `load`, for actions: the viewer must be signed in, and `allowed`.
async fn load_for_action(
    state: &AppState,
    session: &Session,
    slug: &str,
    allowed: impl FnOnce(&Loaded) -> bool,
) -> Result<Loaded, Response> {
    let loaded = load(state, session, slug).await?;
    if loaded.viewer().is_none() {
        return Err(Redirect::to("/login").into_response());
    }
    if !allowed(&loaded) {
        return Err(error_page(
            loaded.layout,
            StatusCode::FORBIDDEN,
            "You can't do this in this community.",
        ));
    }
    Ok(loaded)
}

struct SortLink {
    label: &'static str,
    url: String,
    active: bool,
}

#[derive(Template)]
#[template(path = "community.html")]
struct CommunityTemplate {
    title: String,
    layout: Layout,
    community: Community,
    membership: Membership,
    is_staff: bool,
    can_manage: bool,
    /// Private community and the viewer isn't a member.
    posts_hidden: bool,
    posts: Vec<Post>,
    rules: Vec<Rule>,
    moderators: Vec<Moderator>,
    sorts: Vec<SortLink>,
    error: Option<String>,
    body_max_chars: usize,
    /// `@slug@domain`, how Fediverse users find it, for public communities.
    fediverse_handle: Option<String>,
    follower_count: i64,
    /// The Group for Fediverse servers, see `templates/base.html`.
    activitypub_url: Option<String>,
    return_to: String,
}

pub async fn community_page(
    State(state): State<AppState>,
    session: Session,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let loaded = match load(&state, &session, &slug).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let Loaded { pool, layout, community, membership } = loaded;
    let viewer_id = layout.viewer.as_ref().map(|viewer| viewer.id);
    let is_staff = layout.viewer.as_ref().is_some_and(User::is_staff);
    let sort = Sort::parse(query.get("sort").map(String::as_str));
    let posts_hidden = !community.is_public() && !membership.is_member;

    let posts = if posts_hidden {
        Vec::new()
    } else {
        post::for_community(&pool, community.id, viewer_id, sort)
            .await
            .unwrap_or_else(|err| {
                tracing::error!("failed to load posts of {}: {}", community.slug, err);
                Vec::new()
            })
    };
    let rules = community::rules(&pool, community.id).await.unwrap_or_else(|err| {
        tracing::error!("failed to load rules of {}: {}", community.slug, err);
        Vec::new()
    });
    let moderators = community::moderators(&pool, &community).await.unwrap_or_else(|err| {
        tracing::error!("failed to load moderators of {}: {}", community.slug, err);
        Vec::new()
    });
    let federation = state.federation.as_ref().filter(|_| community.is_federated());
    let follower_count = match federation {
        Some(_) => store::community_follower_count(&pool, community.id).await.unwrap_or(0),
        None => 0,
    };
    let page_url = format!("/community/{}", community.slug);
    let sorts = Sort::ALL
        .into_iter()
        .map(|option| SortLink {
            label: option.label(),
            url: format!("{page_url}?sort={}", option.as_str()),
            active: option == sort,
        })
        .collect();

    render_template(&CommunityTemplate {
        title: community.name.clone(),
        can_manage: membership.can_manage() || is_staff,
        is_staff,
        posts_hidden,
        posts,
        rules,
        moderators,
        sorts,
        error: error_message(&query),
        body_max_chars: BODY_MAX_CHARS,
        fediverse_handle: federation.map(|federation| format!("@{}@{}", community.slug, federation.domain)),
        follower_count,
        activitypub_url: federation.map(|federation| federation.community_url(community.public_id)),
        return_to: format!("{page_url}?sort={}", sort.as_str()),
        layout,
        community,
        membership,
    })
}

pub async fn join(State(state): State<AppState>, session: Session, Path(slug): Path<String>) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, |_| true).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let url = format!("/community/{}", loaded.community.slug);
    let me = loaded.viewer().map(|viewer| viewer.id).unwrap_or_default();
    back(
        &url,
        community::join(&loaded.pool, loaded.community.id, me).await.map_err(server_error),
    )
}

pub async fn quit(State(state): State<AppState>, session: Session, Path(slug): Path<String>) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, |_| true).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let url = format!("/community/{}", loaded.community.slug);
    let me = loaded.viewer().map(|viewer| viewer.id).unwrap_or_default();
    back(
        &url,
        community::quit(&loaded.pool, loaded.community.id, me).await.map_err(server_error),
    )
}

pub async fn toggle_ignore(State(state): State<AppState>, session: Session, Path(slug): Path<String>) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, |_| true).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let url = format!("/community/{}", loaded.community.slug);
    let me = loaded.viewer().map(|viewer| viewer.id).unwrap_or_default();
    back(
        &url,
        community::toggle_ignore(&loaded.pool, loaded.community.id, me)
            .await
            .map(|_| ())
            .map_err(server_error),
    )
}

/// A post in the community, by a member.
pub async fn create_post(
    State(state): State<AppState>,
    session: Session,
    Path(slug): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, |loaded| loaded.membership.is_member).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let url = format!("/community/{}", loaded.community.slug);
    let me = loaded.viewer().map(|viewer| viewer.id).unwrap_or_default();
    let result = async {
        let body = post::validate_body(form.get("body").map(String::as_str).unwrap_or(""))?;
        // Public inside the community: who reads it depends on the community.
        post::create(&loaded.pool, me, &body, post::Visibility::Public, Some(loaded.community.id))
            .await
            .map_err(server_error)
    }
    .await;
    match result {
        Ok(post_id) => {
            notifier::post_created(&state, &loaded.pool, post_id).await;
            Redirect::to(&format!("{url}?sort=new#post-{post_id}")).into_response()
        }
        Err(error) => Redirect::to(&format!("{url}?error={}#composer", error.code())).into_response(),
    }
}

/// Removes a post, by the community's moderators or the site's staff.
pub async fn remove_post(
    State(state): State<AppState>,
    session: Session,
    Path((slug, post_id)): Path<(String, i64)>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::can_manage).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let url = format!("/community/{}", loaded.community.slug);
    match post::delete_in_community(&loaded.pool, post_id, loaded.community.id).await {
        Ok(_) => {
            let moderator = loaded.viewer().map(|viewer| viewer.username.as_str()).unwrap_or_default();
            tracing::info!("@{} removed post {} from {}", moderator, post_id, loaded.community.slug);
            Redirect::to(&local_path(form.get("return"), &url)).into_response()
        }
        Err(err) => back(&url, Err(server_error(err))),
    }
}

/// Bans or unbans the community, by the site's staff.
pub async fn toggle_ban(State(state): State<AppState>, session: Session, Path(slug): Path<String>) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::is_staff).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let url = format!("/community/{}", loaded.community.slug);
    let banned = !loaded.community.is_banned();
    let staff = loaded.viewer().map(|viewer| viewer.username.as_str()).unwrap_or_default();
    tracing::info!("@{} {} community {}", staff, if banned { "banned" } else { "unbanned" }, loaded.community.slug);
    back(
        &url,
        community::set_banned(&loaded.pool, loaded.community.id, banned).await.map_err(server_error),
    )
}

/// Deletes the community and its posts, by its owner or the site's staff.
pub async fn delete_community(State(state): State<AppState>, session: Session, Path(slug): Path<String>) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, |loaded| loaded.membership.is_owner || loaded.is_staff()).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if let Err(err) = community::delete(&loaded.pool, loaded.community.id).await {
        return back(&format!("/community/{}/settings", loaded.community.slug), Err(server_error(err)));
    }
    if let Some(photo) = &loaded.community.photo_url {
        image::remove_image(ImageOwner::Community(loaded.community.id), photo).await;
    }
    let by = loaded.viewer().map(|viewer| viewer.username.as_str()).unwrap_or_default();
    tracing::info!("@{} deleted community {}", by, loaded.community.slug);
    Redirect::to("/communities").into_response()
}

// --- Settings ---

#[derive(Template)]
#[template(path = "community_settings.html")]
struct SettingsTemplate {
    title: String,
    layout: Layout,
    community: Community,
    membership: Membership,
    is_staff: bool,
    rules: Vec<Rule>,
    moderators: Vec<Moderator>,
    error: Option<String>,
    saved: bool,
    visibilities: [Visibility; 2],
    image_field: &'static str,
    name_max_chars: usize,
    description_max_chars: usize,
    rule_title_max_chars: usize,
    rule_body_max_chars: usize,
}

pub async fn settings_page(
    State(state): State<AppState>,
    session: Session,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::can_manage).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let is_staff = loaded.is_staff();
    let Loaded { pool, layout, community, membership } = loaded;
    let rules = community::rules(&pool, community.id).await.unwrap_or_default();
    let moderators = community::moderators(&pool, &community).await.unwrap_or_default();
    render_template(&SettingsTemplate {
        title: format!("Settings of {}", community.name),
        layout,
        community,
        membership,
        is_staff,
        rules,
        moderators,
        error: error_message(&query),
        saved: query.contains_key("saved"),
        visibilities: Visibility::ALL,
        image_field: IMAGE_FIELD,
        name_max_chars: NAME_MAX_CHARS,
        description_max_chars: DESCRIPTION_MAX_CHARS,
        rule_title_max_chars: RULE_TITLE_MAX_CHARS,
        rule_body_max_chars: RULE_BODY_MAX_CHARS,
    })
}

fn settings_url(community: &Community) -> String {
    format!("/community/{}/settings", community.slug)
}

pub async fn update_details(
    State(state): State<AppState>,
    session: Session,
    Path(slug): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::can_manage).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let field = |name: &str| form.get(name).map(String::as_str).unwrap_or("");
    let result = async {
        let name = community::validate_name(field("name")).ok_or(CommunityError::InvalidName)?;
        let description = community::validate_description(field("description")).ok_or(CommunityError::InvalidDescription)?;
        let visibility = Visibility::parse(field("visibility")).ok_or(CommunityError::InvalidVisibility)?;
        community::update_details(&loaded.pool, loaded.community.id, &name, &description, visibility)
            .await
            .map_err(server_error)
    }
    .await;
    back(&settings_url(&loaded.community), result)
}

pub async fn upload_photo(
    State(state): State<AppState>,
    session: Session,
    Path(slug): Path<String>,
    mut multipart: Multipart,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::can_manage).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let owner = ImageOwner::Community(loaded.community.id);
    let url = match image::save_image(owner, "photo", &mut multipart).await {
        Ok(url) => url,
        Err(err) => return back(&settings_url(&loaded.community), Err(err.into())),
    };
    if let Err(err) = community::update_photo(&loaded.pool, loaded.community.id, Some(&url)).await {
        image::remove_image(owner, &url).await;
        return back(&settings_url(&loaded.community), Err(server_error(err)));
    }
    if let Some(old_url) = &loaded.community.photo_url {
        image::remove_image(owner, old_url).await;
    }
    back(&settings_url(&loaded.community), Ok(()))
}

pub async fn remove_photo(State(state): State<AppState>, session: Session, Path(slug): Path<String>) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::can_manage).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if let Err(err) = community::update_photo(&loaded.pool, loaded.community.id, None).await {
        return back(&settings_url(&loaded.community), Err(server_error(err)));
    }
    if let Some(old_url) = &loaded.community.photo_url {
        image::remove_image(ImageOwner::Community(loaded.community.id), old_url).await;
    }
    back(&settings_url(&loaded.community), Ok(()))
}

/// Title, text and position of a rule from its form.
fn rule_fields(form: &HashMap<String, String>) -> Result<(String, String, i32), CommunityError> {
    let field = |name: &str| form.get(name).map(String::as_str).unwrap_or("");
    let title = community::validate_rule_title(field("title")).ok_or(CommunityError::InvalidRule)?;
    let body = community::validate_rule_body(field("body")).ok_or(CommunityError::InvalidRule)?;
    let sort_order = match field("sort_order").trim() {
        "" => 0,
        order => order.parse().map_err(|_| CommunityError::InvalidRule)?,
    };
    Ok((title, body, sort_order))
}

pub async fn add_rule(
    State(state): State<AppState>,
    session: Session,
    Path(slug): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::can_manage).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let result = async {
        let (title, body, sort_order) = rule_fields(&form)?;
        let added = community::add_rule(&loaded.pool, loaded.community.id, &title, &body, sort_order)
            .await
            .map_err(server_error)?;
        if added { Ok(()) } else { Err(CommunityError::TooManyRules) }
    }
    .await;
    back(&settings_url(&loaded.community), result)
}

pub async fn update_rule(
    State(state): State<AppState>,
    session: Session,
    Path((slug, rule_id)): Path<(String, i64)>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::can_manage).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let result = async {
        let (title, body, sort_order) = rule_fields(&form)?;
        community::update_rule(&loaded.pool, loaded.community.id, rule_id, &title, &body, sort_order)
            .await
            .map_err(server_error)
    }
    .await;
    back(&settings_url(&loaded.community), result)
}

pub async fn delete_rule(
    State(state): State<AppState>,
    session: Session,
    Path((slug, rule_id)): Path<(String, i64)>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, Loaded::can_manage).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    back(
        &settings_url(&loaded.community),
        community::delete_rule(&loaded.pool, loaded.community.id, rule_id)
            .await
            .map_err(server_error),
    )
}

/// Adds a moderator by username: the owner only.
pub async fn add_moderator(
    State(state): State<AppState>,
    session: Session,
    Path(slug): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, |loaded| loaded.membership.is_owner || loaded.is_staff()).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let me = loaded.viewer().map(|viewer| viewer.id).unwrap_or_default();
    let username = form.get("username").map(String::as_str).unwrap_or("");
    let result = community::add_moderator(&loaded.pool, loaded.community.id, username, me)
        .await
        .map_err(|err| match err {
            community::AddModeratorError::NotMember => CommunityError::NotMember,
            community::AddModeratorError::Database(err) => server_error(err),
        });
    back(&settings_url(&loaded.community), result)
}

pub async fn remove_moderator(
    State(state): State<AppState>,
    session: Session,
    Path((slug, user_id)): Path<(String, i32)>,
) -> Response {
    let loaded = match load_for_action(&state, &session, &slug, |loaded| loaded.membership.is_owner || loaded.is_staff()).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    back(
        &settings_url(&loaded.community),
        community::remove_moderator(&loaded.pool, loaded.community.id, user_id)
            .await
            .map_err(server_error),
    )
}
