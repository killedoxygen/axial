use crate::state::AppState;
use crate::state::skins::{SavedSkinDeleteResult, SavedSkinRecord};
use crate::state::{
    AuthLoginMinecraftAccount, AuthLoginMinecraftCape, AuthLoginMinecraftProfile,
    AuthLoginMinecraftSkin,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::{Response, StatusCode, header},
    routing::{delete, get, post, put},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use croopor_config::validate_username;
use croopor_minecraft::offline_uuid;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt::Write,
    io::Cursor,
    path::{Path as FsPath, PathBuf},
    sync::{LazyLock, OnceLock},
    time::{Duration, Instant},
};

const DEFAULT_HEAD_SIZE: u32 = 64;
const MIN_HEAD_SIZE: u32 = 16;
const MAX_HEAD_SIZE: u32 = 256;
const HEAD_CACHE_CONTROL: &str = "private, max-age=86400";
const PROFILE_SKIN_FILE_CACHE_CONTROL: &str = "private, max-age=300";
const PROFILE_CAPE_FILE_CACHE_CONTROL: &str = "private, max-age=86400";
const SAVED_SKIN_FILE_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";
const MINECRAFT_TEXTURE_URL_PREFIX: &str = "https://textures.minecraft.net/texture/";
const PROFILE_SKIN_FILE_CACHE_DIR: &str = "profile-cache";
const PROFILE_CAPE_FILE_CACHE_DIR: &str = "cape-cache";
const SKIN_UPLOAD_MAX_BYTES: usize = 256 * 1024;
const CAPE_TEXTURE_MAX_DIMENSION: u32 = 512;
const SAVE_SKIN_FROM_PROFILE_REQUEST_MAX_BYTES: usize = 4 * 1024;
const SAVE_SKIN_FROM_USERNAME_REQUEST_MAX_BYTES: usize = 4 * 1024;
const MOJANG_PROFILE_RESPONSE_MAX_BYTES: usize = 16 * 1024;
const MINECRAFT_SESSION_PROFILE_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const MINECRAFT_SESSION_TEXTURES_PROPERTY_MAX_BYTES: usize = 16 * 1024;
const MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const MINECRAFT_SKIN_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MINECRAFT_SKIN_HTTP_TIMEOUT: Duration = Duration::from_secs(25);
const MINECRAFT_USERNAME_LOOKUP_CACHE_TTL: Duration = Duration::from_secs(300);
const MINECRAFT_USERNAME_LOOKUP_CACHE_MAX_ENTRIES: usize = 256;
const SKIN_CHANGE_DEBOUNCE: Duration = Duration::from_secs(10);
const SKIN_WIDTH: u32 = 64;
const SKIN_HEIGHT: u32 = 64;
const LEGACY_SKIN_HEIGHT: u32 = 32;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const SAVED_SKIN_NAME_MAX_CHARS: usize = 64;
const SAVED_SKIN_SOURCE: &str = "local_upload";
const SAVED_SKIN_DEFAULT_SOURCE: &str = "minecraft_default_skin";
const SAVED_SKIN_PROFILE_SOURCE: &str = "minecraft_profile_skin";
const SAVED_SKIN_USERNAME_SOURCE: &str = "minecraft_username_skin";
const MOJANG_PROFILE_ENDPOINT: &str = "https://api.mojang.com/users/profiles/minecraft";
const MINECRAFT_SESSION_PROFILE_ENDPOINT: &str =
    "https://sessionserver.mojang.com/session/minecraft/profile";
const MINECRAFT_SKIN_UPLOAD_ENDPOINT: &str =
    "https://api.minecraftservices.com/minecraft/profile/skins";
const MINECRAFT_SKIN_RESET_ENDPOINT: &str =
    "https://api.minecraftservices.com/minecraft/profile/skins/active";
const MINECRAFT_CAPE_ENDPOINT: &str =
    "https://api.minecraftservices.com/minecraft/profile/capes/active";
const CROOPOR_USER_AGENT: &str = concat!("croopor/", env!("CARGO_PKG_VERSION"));

type ApiError = (StatusCode, Json<serde_json::Value>);

static PENDING_SKIN_APPLIES: LazyLock<tokio::sync::Mutex<PendingSkinApplyState>> =
    LazyLock::new(|| tokio::sync::Mutex::new(PendingSkinApplyState::default()));
static PENDING_SKIN_APPLY_FLUSH_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));
static MINECRAFT_USERNAME_SKIN_CACHE: LazyLock<
    tokio::sync::Mutex<HashMap<String, MinecraftUsernameSkinCacheEntry>>,
> = LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

#[derive(Debug, Default)]
struct PendingSkinApplyState {
    pending: HashMap<String, PendingSkinApplyEntry>,
}

#[derive(Debug)]
struct PendingSkinApplyEntry {
    change: PendingSkinApplyChange,
    generation: u64,
}

#[derive(Debug, Clone)]
struct PendingSkinApplyChange {
    login_id: String,
    texture_key: String,
}

#[derive(Clone)]
struct MinecraftUsernameSkinCacheEntry {
    profile: MinecraftUsernameSkinProfile,
    expires_at: Instant,
}

#[derive(Debug, Default, Deserialize)]
struct SkinQuery {
    username: Option<String>,
    size: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct SkinProfileFileQuery {
    texture: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SkinCapeFileQuery {
    id: String,
}

#[derive(Debug, Deserialize)]
struct SkinLookupQuery {
    username: String,
    size: Option<u32>,
}

#[derive(Debug, Serialize)]
struct SkinProfileResponse {
    auth_mode: &'static str,
    username: String,
    uuid: String,
    source: &'static str,
    variant: &'static str,
    texture_url: Option<String>,
    head_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct SkinLookupResponse {
    username: String,
    uuid: String,
    source: &'static str,
    variant: &'static str,
    texture_url: String,
    texture_file_url: String,
    cape_url: Option<String>,
    head_url: String,
}

#[derive(Debug, Serialize)]
struct SkinNormalizeResponse {
    texture_key: String,
    variant_suggestion: &'static str,
    original_width: u32,
    original_height: u32,
    normalized_width: u32,
    normalized_height: u32,
    normalized_byte_size: usize,
    normalized_data_url: String,
}

#[derive(Debug, Default, Deserialize)]
struct SaveSkinQuery {
    name: Option<String>,
    variant: Option<String>,
    cape_id: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveSkinFromProfileRequest {
    name: Option<String>,
    variant: Option<String>,
    mark_current: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveSkinFromUsernameRequest {
    username: String,
    name: Option<String>,
    variant: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct UpdateSavedSkinRequest {
    name: Option<String>,
    variant: Option<String>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_cape_update")]
    cape_id: CapeUpdate,
}

#[derive(Debug, Default, Deserialize)]
struct ReplaceSavedSkinTextureQuery {
    name: Option<String>,
    variant: Option<String>,
    cape_id: Option<String>,
    clear_cape: Option<bool>,
}

#[derive(Debug, Default)]
enum CapeUpdate {
    #[default]
    Unchanged,
    Clear,
    Set(String),
}

fn deserialize_cape_update<'de, D>(deserializer: D) -> Result<CapeUpdate, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
        .map(|value| value.map_or(CapeUpdate::Clear, CapeUpdate::Set))
}

#[derive(Debug, Default, Deserialize)]
struct ApplySavedSkinQuery {
    defer: Option<bool>,
}

#[derive(Debug, Serialize)]
struct SavedSkinsResponse {
    skins: Vec<SavedSkinRecord>,
    pending_apply_texture_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct SkinApplyResponse {
    status: &'static str,
    texture_key: String,
    profile_updated: bool,
}

#[derive(Debug, Serialize)]
struct SkinProfileResetResponse {
    status: &'static str,
    profile_updated: bool,
}

#[derive(Debug, Serialize)]
struct SkinCapeResetResponse {
    status: &'static str,
    profile_updated: bool,
}

#[derive(Debug, Serialize)]
struct SkinFlushResponse {
    status: &'static str,
    applied: usize,
}

#[derive(Debug, Serialize)]
struct SkinPendingClearResponse {
    status: &'static str,
    cleared: bool,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/skin/profile", get(handle_skin_profile))
        .route(
            "/api/v1/skin/profile/reset",
            post(handle_skin_profile_reset),
        )
        .route("/api/v1/skin/profile/file", get(handle_skin_profile_file))
        .route("/api/v1/skin/cape/file", get(handle_skin_cape_file))
        .route("/api/v1/skin/cape/reset", post(handle_skin_cape_reset))
        .route("/api/v1/skin/head", get(handle_skin_head))
        .route("/api/v1/skin/lookup", get(handle_skin_lookup))
        .route("/api/v1/skin/lookup/file", get(handle_skin_lookup_file))
        .route("/api/v1/skin/lookup/head", get(handle_skin_lookup_head))
        .route("/api/v1/skin/lookup/cape", get(handle_skin_lookup_cape))
        .route("/api/v1/skins/normalize", post(handle_skin_normalize))
        .route(
            "/api/v1/skins",
            get(handle_saved_skins).post(handle_save_skin),
        )
        .route(
            "/api/v1/skins/from-profile",
            post(handle_save_skin_from_profile),
        )
        .route(
            "/api/v1/skins/from-username",
            post(handle_save_skin_from_username),
        )
        .route(
            "/api/v1/skins/pending",
            delete(handle_clear_pending_saved_skin_apply),
        )
        .route(
            "/api/v1/skins/{texture_key}",
            delete(handle_delete_skin).put(handle_update_saved_skin),
        )
        .route(
            "/api/v1/skins/{texture_key}/texture",
            put(handle_replace_saved_skin_texture),
        )
        .route(
            "/api/v1/skins/{texture_key}/file",
            get(handle_saved_skin_file),
        )
        .route(
            "/api/v1/skins/{texture_key}/apply",
            post(handle_apply_saved_skin),
        )
        .route("/api/v1/skins/flush", post(handle_flush_saved_skin_applies))
}

async fn handle_skin_profile(
    State(state): State<AppState>,
    Query(query): Query<SkinQuery>,
) -> Result<Json<SkinProfileResponse>, (StatusCode, Json<serde_json::Value>)> {
    let config = state.config().current();
    if query.username.is_none()
        && let Some(profile) = online_skin_profile(
            state
                .auth_logins()
                .active_current_minecraft_account_state()
                .await
                .map(|state| state.account),
        )
    {
        return Ok(Json(profile));
    }

    let identity = select_offline_identity(query.username.as_deref(), &config.username)?;

    Ok(Json(SkinProfileResponse {
        auth_mode: "offline",
        username: identity.username.clone(),
        uuid: identity.uuid,
        source: "default",
        variant: identity.variant,
        texture_url: None,
        head_url: Some(format!("/api/v1/skin/head?username={}", identity.username)),
    }))
}

fn online_skin_profile(account: Option<AuthLoginMinecraftAccount>) -> Option<SkinProfileResponse> {
    let account = account?;
    let profile_name = account.profile.name.trim();
    let profile_id = account.profile.id.trim();
    if profile_name.is_empty() || profile_id.is_empty() {
        return None;
    }

    let selected_skin = select_minecraft_skin(&account.profile.skins);
    let texture_url = selected_skin.and_then(|skin| sane_minecraft_texture_url(&skin.url));
    let variant = selected_skin
        .map(|skin| skin_variant(&skin.variant))
        .unwrap_or("classic");
    let source = if selected_skin.is_some() {
        "minecraft_profile_skin"
    } else {
        "default"
    };

    Some(SkinProfileResponse {
        auth_mode: "online",
        username: profile_name.to_string(),
        uuid: profile_id.to_string(),
        source,
        variant,
        texture_url,
        head_url: None,
    })
}

struct ActiveMinecraftProfileSkin {
    account: AuthLoginMinecraftAccount,
    texture_url: String,
    cape_id: Option<String>,
}

struct ActiveMinecraftProfileCape {
    texture_url: String,
}

async fn active_minecraft_profile_skin(
    state: &AppState,
    allowed_prefix: &str,
    not_ready_message: &'static str,
) -> Result<ActiveMinecraftProfileSkin, ApiError> {
    let minecraft_state = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .ok_or_else(|| {
            json_status_error(
                StatusCode::UNAUTHORIZED,
                "Minecraft account login required",
                "minecraft_account_required",
            )
        })?;
    let account = minecraft_state.account;
    if !account.owns_minecraft_java
        || account.profile.id.trim().is_empty()
        || account.profile.name.trim().is_empty()
    {
        return Err(json_status_error(
            StatusCode::CONFLICT,
            not_ready_message,
            "minecraft_account_not_ready",
        ));
    }

    let selected_skin =
        select_sane_minecraft_skin_with_prefix(&account.profile.skins, allowed_prefix).ok_or_else(
            || {
                json_status_error(
                    StatusCode::CONFLICT,
                    "Minecraft profile does not have a usable skin texture",
                    "minecraft_profile_skin_missing",
                )
            },
        )?;
    let texture_url = sane_minecraft_texture_url_with_prefix(&selected_skin.url, allowed_prefix)
        .ok_or_else(|| {
            json_status_error(
                StatusCode::CONFLICT,
                "Minecraft profile does not have a usable skin texture",
                "minecraft_profile_skin_missing",
            )
        })?;
    let cape_id = active_minecraft_cape_id(&account.profile);

    Ok(ActiveMinecraftProfileSkin {
        account,
        texture_url,
        cape_id,
    })
}

async fn active_minecraft_profile_cape(
    state: &AppState,
    cape_id: &str,
    allowed_prefix: &str,
    not_ready_message: &'static str,
) -> Result<ActiveMinecraftProfileCape, ApiError> {
    let minecraft_state = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .ok_or_else(|| {
            json_status_error(
                StatusCode::UNAUTHORIZED,
                "Minecraft account login required",
                "minecraft_account_required",
            )
        })?;
    let account = minecraft_state.account;
    if !account.owns_minecraft_java
        || account.profile.id.trim().is_empty()
        || account.profile.name.trim().is_empty()
    {
        return Err(json_status_error(
            StatusCode::CONFLICT,
            not_ready_message,
            "minecraft_account_not_ready",
        ));
    }

    let cape = account
        .profile
        .capes
        .iter()
        .find(|cape| cape.id == cape_id)
        .ok_or_else(|| {
            json_status_error(
                StatusCode::NOT_FOUND,
                "Minecraft cape is not available for this account",
                "minecraft_cape_not_found",
            )
        })?;
    let texture_url = sane_minecraft_texture_url_with_prefix(&cape.url, allowed_prefix)
        .ok_or_else(|| {
            json_status_error(
                StatusCode::CONFLICT,
                "Minecraft cape does not have a usable texture",
                "minecraft_cape_texture_missing",
            )
        })?;

    Ok(ActiveMinecraftProfileCape { texture_url })
}

fn select_minecraft_skin(skins: &[AuthLoginMinecraftSkin]) -> Option<&AuthLoginMinecraftSkin> {
    select_minecraft_skin_with_prefix(skins, MINECRAFT_TEXTURE_URL_PREFIX)
}

fn select_minecraft_skin_with_prefix<'a>(
    skins: &'a [AuthLoginMinecraftSkin],
    allowed_prefix: &str,
) -> Option<&'a AuthLoginMinecraftSkin> {
    skins
        .iter()
        .find(|skin| skin.state.eq_ignore_ascii_case("ACTIVE"))
        .or_else(|| {
            skins.iter().find(|skin| {
                sane_minecraft_texture_url_with_prefix(&skin.url, allowed_prefix).is_some()
            })
        })
}

fn select_sane_minecraft_skin_with_prefix<'a>(
    skins: &'a [AuthLoginMinecraftSkin],
    allowed_prefix: &str,
) -> Option<&'a AuthLoginMinecraftSkin> {
    skins
        .iter()
        .find(|skin| {
            skin.state.eq_ignore_ascii_case("ACTIVE")
                && sane_minecraft_texture_url_with_prefix(&skin.url, allowed_prefix).is_some()
        })
        .or_else(|| {
            skins.iter().find(|skin| {
                sane_minecraft_texture_url_with_prefix(&skin.url, allowed_prefix).is_some()
            })
        })
}

fn active_minecraft_cape_id(profile: &AuthLoginMinecraftProfile) -> Option<String> {
    profile
        .capes
        .iter()
        .find(|cape| cape.state.eq_ignore_ascii_case("ACTIVE"))
        .map(|cape| cape.id.clone())
}

fn skin_variant(variant: &str) -> &'static str {
    if variant.eq_ignore_ascii_case("slim") {
        "slim"
    } else {
        "classic"
    }
}

fn skin_lookup_response(
    profile: MinecraftUsernameSkinProfile,
    head_size: u32,
) -> SkinLookupResponse {
    SkinLookupResponse {
        username: profile.name.clone(),
        uuid: profile.uuid,
        source: SAVED_SKIN_USERNAME_SOURCE,
        variant: profile.variant,
        texture_url: profile.texture_url,
        texture_file_url: format!("/api/v1/skin/lookup/file?username={}", profile.name),
        cape_url: profile.cape_url,
        head_url: format!(
            "/api/v1/skin/lookup/head?username={}&size={}",
            profile.name, head_size
        ),
    }
}

fn sane_minecraft_texture_url(url: &str) -> Option<String> {
    sane_minecraft_texture_url_with_prefix(url, MINECRAFT_TEXTURE_URL_PREFIX)
}

fn sane_minecraft_texture_url_with_prefix(url: &str, allowed_prefix: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed != url {
        return None;
    }

    // Mojang session and profile APIs still hand out http:// texture URLs;
    // canonicalize them to the https prefix before validating and downloading.
    let canonical = match (
        allowed_prefix.strip_prefix("https://"),
        trimmed.strip_prefix("http://"),
    ) {
        (Some(_), Some(rest)) => format!("https://{rest}"),
        _ => trimmed.to_string(),
    };
    if !canonical.starts_with(allowed_prefix) {
        return None;
    }

    let texture_id = &canonical[allowed_prefix.len()..];
    if texture_id.is_empty() || texture_id.len() > 128 {
        return None;
    }
    if !texture_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return None;
    }

    Some(canonical)
}

async fn handle_skin_head(
    State(state): State<AppState>,
    Query(query): Query<SkinQuery>,
) -> Result<Response<Body>, ApiError> {
    let config = state.config().current();
    let identity = select_offline_identity(query.username.as_deref(), &config.username)?;
    let size = clamp_head_size(query.size);
    let svg = offline_head_svg(&identity.uuid, size);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/svg+xml")
        .header(header::CACHE_CONTROL, HEAD_CACHE_CONTROL)
        .body(Body::from(svg))
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to build skin head response" })),
            )
        })
}

async fn handle_skin_lookup(
    Query(query): Query<SkinLookupQuery>,
) -> Result<Json<SkinLookupResponse>, ApiError> {
    handle_skin_lookup_with_client(
        Query(query),
        MinecraftSkinUsernameClient::default(),
        MINECRAFT_TEXTURE_URL_PREFIX.to_string(),
    )
    .await
}

async fn handle_skin_lookup_with_client(
    Query(query): Query<SkinLookupQuery>,
    profile_client: MinecraftSkinUsernameClient,
    allowed_texture_prefix: String,
) -> Result<Json<SkinLookupResponse>, ApiError> {
    let username = validate_username(&query.username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;
    let profile = profile_client
        .skin_profile(&username, &allowed_texture_prefix)
        .await
        .map_err(skin_username_lookup_error)?;
    let size = clamp_head_size(query.size);

    Ok(Json(skin_lookup_response(profile, size)))
}

async fn handle_skin_lookup_file(
    State(state): State<AppState>,
    Query(query): Query<SkinLookupQuery>,
) -> Result<Response<Body>, ApiError> {
    handle_skin_lookup_file_with_clients(
        State(state),
        Query(query),
        MinecraftSkinUsernameClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn handle_skin_lookup_file_with_clients(
    State(state): State<AppState>,
    Query(query): Query<SkinLookupQuery>,
    profile_client: MinecraftSkinUsernameClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Response<Body>, ApiError> {
    let username = validate_username(&query.username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;
    let profile = profile_client
        .skin_profile(&username, texture_client.allowed_prefix())
        .await
        .map_err(skin_username_lookup_error)?;
    let cache_path =
        profile_skin_file_cache_path(&state.config().paths().config_dir, &profile.texture_url);
    if let Some(bytes) = read_profile_skin_file_cache(&cache_path).await {
        return profile_skin_file_response(bytes);
    }

    let bytes = texture_client
        .download(&profile.texture_url)
        .await
        .map_err(skin_texture_download_error)?;
    let normalized = normalize_skin_png(&bytes)?;
    let png_bytes = normalized.png_bytes;
    let _ = write_profile_skin_file_cache(&cache_path, &png_bytes).await;

    profile_skin_file_response(png_bytes)
}

async fn handle_skin_lookup_head(
    State(state): State<AppState>,
    Query(query): Query<SkinLookupQuery>,
) -> Result<Response<Body>, ApiError> {
    handle_skin_lookup_head_with_clients(
        State(state),
        Query(query),
        MinecraftSkinUsernameClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn handle_skin_lookup_head_with_clients(
    State(state): State<AppState>,
    Query(query): Query<SkinLookupQuery>,
    profile_client: MinecraftSkinUsernameClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Response<Body>, ApiError> {
    let username = validate_username(&query.username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;
    let profile = profile_client
        .skin_profile(&username, texture_client.allowed_prefix())
        .await
        .map_err(skin_username_lookup_error)?;
    let cache_path =
        profile_skin_file_cache_path(&state.config().paths().config_dir, &profile.texture_url);
    let normalized_png = match read_profile_skin_file_cache(&cache_path).await {
        Some(bytes) => bytes,
        None => {
            let bytes = texture_client
                .download(&profile.texture_url)
                .await
                .map_err(skin_texture_download_error)?;
            let normalized = normalize_skin_png(&bytes)?;
            let png_bytes = normalized.png_bytes;
            let _ = write_profile_skin_file_cache(&cache_path, &png_bytes).await;
            png_bytes
        }
    };
    let png_bytes = render_skin_head_png(&normalized_png, clamp_head_size(query.size))?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, HEAD_CACHE_CONTROL)
        .body(Body::from(png_bytes))
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to build skin head response" })),
            )
        })
}

async fn handle_skin_lookup_cape(
    State(state): State<AppState>,
    Query(query): Query<SkinLookupQuery>,
) -> Result<Response<Body>, ApiError> {
    handle_skin_lookup_cape_with_clients(
        State(state),
        Query(query),
        MinecraftSkinUsernameClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn handle_skin_lookup_cape_with_clients(
    State(state): State<AppState>,
    Query(query): Query<SkinLookupQuery>,
    profile_client: MinecraftSkinUsernameClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Response<Body>, ApiError> {
    let username = validate_username(&query.username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;
    let profile = profile_client
        .skin_profile(&username, texture_client.allowed_prefix())
        .await
        .map_err(skin_username_lookup_error)?;
    let cape_url = profile.cape_url.ok_or_else(|| {
        json_status_error(
            StatusCode::CONFLICT,
            "Minecraft player profile does not have a usable cape texture",
            "minecraft_lookup_cape_missing",
        )
    })?;
    let cache_path = profile_cape_file_cache_path(&state.config().paths().config_dir, &cape_url);
    if let Some(bytes) = read_profile_cape_file_cache(&cache_path).await {
        return profile_cape_file_response(bytes);
    }

    let bytes = texture_client
        .download(&cape_url)
        .await
        .map_err(cape_texture_download_error)?;
    if !is_valid_cape_texture_png(&bytes) {
        return Err(cape_texture_invalid_error());
    }
    let _ = write_profile_skin_file_cache(&cache_path, &bytes).await;

    profile_cape_file_response(bytes)
}

async fn handle_skin_normalize(body: Body) -> Result<Json<SkinNormalizeResponse>, ApiError> {
    let bytes = to_bytes(body, SKIN_UPLOAD_MAX_BYTES)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "skin upload is too large"))?;
    let normalized = normalize_skin_png(&bytes)?;

    Ok(Json(SkinNormalizeResponse {
        texture_key: texture_key(&normalized.png_bytes),
        variant_suggestion: normalized.variant_suggestion,
        original_width: normalized.original_width,
        original_height: normalized.original_height,
        normalized_width: SKIN_WIDTH,
        normalized_height: SKIN_HEIGHT,
        normalized_byte_size: normalized.png_bytes.len(),
        normalized_data_url: format!(
            "data:image/png;base64,{}",
            BASE64_STANDARD.encode(&normalized.png_bytes)
        ),
    }))
}

async fn handle_skin_profile_file(
    State(state): State<AppState>,
    Query(query): Query<SkinProfileFileQuery>,
) -> Result<Response<Body>, ApiError> {
    handle_skin_profile_file_with_client(
        State(state),
        Query(query),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn handle_skin_profile_file_with_client(
    State(state): State<AppState>,
    Query(query): Query<SkinProfileFileQuery>,
    client: MinecraftSkinTextureClient,
) -> Result<Response<Body>, ApiError> {
    let texture_url = match query.texture.as_deref() {
        Some(texture) => sane_minecraft_texture_url_with_prefix(texture, client.allowed_prefix())
            .ok_or_else(|| {
            json_status_error(
                StatusCode::BAD_REQUEST,
                "Minecraft profile skin texture is invalid",
                "minecraft_profile_skin_invalid",
            )
        })?,
        None => {
            active_minecraft_profile_skin(
                &state,
                client.allowed_prefix(),
                "Minecraft account is not ready for profile skin preview",
            )
            .await?
            .texture_url
        }
    };
    let cache_path = profile_skin_file_cache_path(&state.config().paths().config_dir, &texture_url);
    if let Some(bytes) = read_profile_skin_file_cache(&cache_path).await {
        return profile_skin_file_response(bytes);
    }

    let bytes = client
        .download(&texture_url)
        .await
        .map_err(skin_texture_download_error)?;
    let normalized = normalize_skin_png(&bytes)?;
    let png_bytes = normalized.png_bytes;
    let _ = write_profile_skin_file_cache(&cache_path, &png_bytes).await;

    profile_skin_file_response(png_bytes)
}

async fn handle_skin_cape_file(
    State(state): State<AppState>,
    Query(query): Query<SkinCapeFileQuery>,
) -> Result<Response<Body>, ApiError> {
    handle_skin_cape_file_with_client(
        State(state),
        Query(query),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn handle_skin_cape_file_with_client(
    State(state): State<AppState>,
    Query(query): Query<SkinCapeFileQuery>,
    client: MinecraftSkinTextureClient,
) -> Result<Response<Body>, ApiError> {
    let cape_id = validate_saved_skin_cape_id(&query.id)?;
    let cape = active_minecraft_profile_cape(
        &state,
        &cape_id,
        client.allowed_prefix(),
        "Minecraft account is not ready for cape preview",
    )
    .await?;
    let cache_path =
        profile_cape_file_cache_path(&state.config().paths().config_dir, &cape.texture_url);
    if let Some(bytes) = read_profile_cape_file_cache(&cache_path).await {
        return profile_cape_file_response(bytes);
    }

    let bytes = client
        .download(&cape.texture_url)
        .await
        .map_err(cape_texture_download_error)?;
    if !is_valid_cape_texture_png(&bytes) {
        return Err(cape_texture_invalid_error());
    }
    let _ = write_profile_skin_file_cache(&cache_path, &bytes).await;

    profile_cape_file_response(bytes)
}

fn profile_skin_file_response(png_bytes: Vec<u8>) -> Result<Response<Body>, ApiError> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, PROFILE_SKIN_FILE_CACHE_CONTROL)
        .body(Body::from(png_bytes))
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to build profile skin response" })),
            )
        })
}

fn profile_cape_file_response(png_bytes: Vec<u8>) -> Result<Response<Body>, ApiError> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, PROFILE_CAPE_FILE_CACHE_CONTROL)
        .body(Body::from(png_bytes))
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to build profile cape response" })),
            )
        })
}

fn profile_skin_file_cache_path(config_dir: &FsPath, texture_url: &str) -> PathBuf {
    config_dir
        .join("skins")
        .join(PROFILE_SKIN_FILE_CACHE_DIR)
        .join(format!("{}.png", profile_skin_file_cache_key(texture_url)))
}

fn profile_cape_file_cache_path(config_dir: &FsPath, texture_url: &str) -> PathBuf {
    config_dir
        .join("skins")
        .join(PROFILE_CAPE_FILE_CACHE_DIR)
        .join(format!("{}.png", profile_skin_file_cache_key(texture_url)))
}

fn profile_skin_file_cache_key(texture_url: &str) -> String {
    texture_key(texture_url.as_bytes())
}

async fn read_profile_skin_file_cache(path: &FsPath) -> Option<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > SKIN_UPLOAD_MAX_BYTES as u64 {
        return None;
    }

    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes.len() > SKIN_UPLOAD_MAX_BYTES || !is_valid_normalized_skin_cache_png(&bytes) {
        return None;
    }

    Some(bytes)
}

async fn read_profile_cape_file_cache(path: &FsPath) -> Option<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > SKIN_UPLOAD_MAX_BYTES as u64 {
        return None;
    }

    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes.len() > SKIN_UPLOAD_MAX_BYTES || !is_valid_cape_texture_png(&bytes) {
        return None;
    }

    Some(bytes)
}

fn is_valid_normalized_skin_cache_png(bytes: &[u8]) -> bool {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return false;
    }

    decode_skin_png(bytes)
        .is_ok_and(|decoded| decoded.width == SKIN_WIDTH && decoded.height == SKIN_HEIGHT)
}

fn is_valid_cape_texture_png(bytes: &[u8]) -> bool {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return false;
    }
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let Ok(reader) = decoder.read_info() else {
        return false;
    };
    let info = reader.info();
    info.width > 0
        && info.height > 0
        && info.width <= CAPE_TEXTURE_MAX_DIMENSION
        && info.height <= CAPE_TEXTURE_MAX_DIMENSION
}

async fn write_profile_skin_file_cache(path: &FsPath, bytes: &[u8]) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(path, bytes).await
}

async fn handle_saved_skins(
    State(state): State<AppState>,
) -> Result<Json<SavedSkinsResponse>, ApiError> {
    let skins = list_saved_skins(&state).await?;
    let pending_apply_texture_key =
        pending_saved_skin_apply_texture_key_for_active_account(&state).await;

    Ok(Json(SavedSkinsResponse {
        skins,
        pending_apply_texture_key,
    }))
}

async fn handle_save_skin(
    State(state): State<AppState>,
    Query(query): Query<SaveSkinQuery>,
    body: Body,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let name = validate_saved_skin_name(query.name.as_deref().unwrap_or_default())?;
    let bytes = to_bytes(body, SKIN_UPLOAD_MAX_BYTES)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "skin upload is too large"))?;
    let normalized = normalize_skin_png(&bytes)?;
    let variant = match query.variant.as_deref() {
        Some(_) => validate_saved_skin_variant(query.variant.as_deref())?,
        None => normalized.variant_suggestion.to_string(),
    };
    let cape_id = match query.cape_id.as_deref() {
        Some(cape_id) => {
            validate_saved_skin_cape_update(&state, &CapeUpdate::Set(cape_id.to_string()))
                .await?
                .flatten()
        }
        None => None,
    };
    let source = validate_saved_skin_upload_source(query.source.as_deref())?;
    let texture_key = texture_key(&normalized.png_bytes);
    let record = save_saved_skin(
        &state,
        texture_key,
        name,
        variant,
        source,
        cape_id,
        normalized.png_bytes,
    )
    .await?;

    Ok(Json(record))
}

async fn handle_save_skin_from_profile(
    State(state): State<AppState>,
    body: Body,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let payload = read_save_skin_from_profile_request(body).await?;
    handle_save_skin_from_profile_with_client(
        State(state),
        payload,
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn read_save_skin_from_profile_request(
    body: Body,
) -> Result<SaveSkinFromProfileRequest, ApiError> {
    let bytes = to_bytes(body, SAVE_SKIN_FROM_PROFILE_REQUEST_MAX_BYTES)
        .await
        .map_err(|_| {
            json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "skin profile save request is too large",
            )
        })?;
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(SaveSkinFromProfileRequest::default());
    }

    serde_json::from_slice(&bytes).map_err(|_| {
        json_error(
            StatusCode::BAD_REQUEST,
            "skin profile save request must be JSON",
        )
    })
}

async fn handle_save_skin_from_profile_with_client(
    State(state): State<AppState>,
    payload: SaveSkinFromProfileRequest,
    client: MinecraftSkinTextureClient,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let profile_skin = active_minecraft_profile_skin(
        &state,
        client.allowed_prefix(),
        "Minecraft account is not ready for profile skin save",
    )
    .await?;
    let account = profile_skin.account;
    let name = match payload.name.as_deref() {
        Some(name) => validate_saved_skin_name(name)?,
        None => validate_saved_skin_name(&default_profile_skin_name(&account.profile.name))?,
    };
    let variant_override = match payload.variant.as_deref() {
        Some(variant) => Some(validate_saved_skin_variant(Some(variant))?),
        None => None,
    };
    let cape_id = profile_skin.cape_id;
    let cache_path = profile_skin_file_cache_path(
        &state.config().paths().config_dir,
        &profile_skin.texture_url,
    );
    let normalized = match read_profile_skin_file_cache(&cache_path).await {
        Some(png_bytes) => normalize_skin_png(&png_bytes)?,
        None => {
            let bytes = client
                .download(&profile_skin.texture_url)
                .await
                .map_err(skin_texture_download_error)?;
            let normalized = normalize_skin_png(&bytes)?;
            let _ = write_profile_skin_file_cache(&cache_path, &normalized.png_bytes).await;
            normalized
        }
    };
    let variant = variant_override.unwrap_or_else(|| normalized.variant_suggestion.to_string());
    let texture_key = texture_key(&normalized.png_bytes);
    let mut record = save_saved_skin(
        &state,
        texture_key,
        name,
        variant,
        SAVED_SKIN_PROFILE_SOURCE.to_string(),
        cape_id,
        normalized.png_bytes,
    )
    .await?;
    if payload.mark_current == Some(true)
        && let Some(applied_at) =
            mark_saved_skin_applied(&state, record.texture_key.clone()).await?
    {
        record.applied_at = Some(applied_at);
    }

    Ok(Json(record))
}

async fn handle_save_skin_from_username(
    State(state): State<AppState>,
    body: Body,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let payload = read_save_skin_from_username_request(body).await?;
    handle_save_skin_from_username_with_clients(
        State(state),
        payload,
        MinecraftSkinUsernameClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn read_save_skin_from_username_request(
    body: Body,
) -> Result<SaveSkinFromUsernameRequest, ApiError> {
    let bytes = to_bytes(body, SAVE_SKIN_FROM_USERNAME_REQUEST_MAX_BYTES)
        .await
        .map_err(|_| {
            json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "skin username save request is too large",
            )
        })?;

    serde_json::from_slice(&bytes).map_err(|_| {
        json_error(
            StatusCode::BAD_REQUEST,
            "skin username save request must be JSON",
        )
    })
}

async fn handle_save_skin_from_username_with_clients(
    State(state): State<AppState>,
    payload: SaveSkinFromUsernameRequest,
    profile_client: MinecraftSkinUsernameClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let username = validate_username(&payload.username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;
    let profile = profile_client
        .skin_profile(&username, texture_client.allowed_prefix())
        .await
        .map_err(skin_username_lookup_error)?;
    let name = match payload.name.as_deref() {
        Some(name) => validate_saved_skin_name(name)?,
        None => validate_saved_skin_name(&default_username_skin_name(&profile.name))?,
    };
    let variant_override = match payload.variant.as_deref() {
        Some(variant) => Some(validate_saved_skin_variant(Some(variant))?),
        None => None,
    };
    let cache_path =
        profile_skin_file_cache_path(&state.config().paths().config_dir, &profile.texture_url);
    let normalized = match read_profile_skin_file_cache(&cache_path).await {
        Some(png_bytes) => normalize_skin_png(&png_bytes)?,
        None => {
            let bytes = texture_client
                .download(&profile.texture_url)
                .await
                .map_err(skin_texture_download_error)?;
            let normalized = normalize_skin_png(&bytes)?;
            let _ = write_profile_skin_file_cache(&cache_path, &normalized.png_bytes).await;
            normalized
        }
    };
    let variant = variant_override.unwrap_or_else(|| normalized.variant_suggestion.to_string());
    let texture_key = texture_key(&normalized.png_bytes);
    let record = save_saved_skin(
        &state,
        texture_key,
        name,
        variant,
        SAVED_SKIN_USERNAME_SOURCE.to_string(),
        None,
        normalized.png_bytes,
    )
    .await?;

    Ok(Json(record))
}

async fn handle_delete_skin(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    match delete_saved_skin(&state, texture_key.clone()).await? {
        SavedSkinDeleteResult::Deleted(_) => {
            clear_pending_saved_skin_apply_for_texture(&texture_key).await;
            Ok(Json(serde_json::json!({ "status": "deleted" })))
        }
        SavedSkinDeleteResult::Applied => Err(json_error(
            StatusCode::CONFLICT,
            "applied saved skin cannot be deleted; reset or apply another skin first",
        )),
        SavedSkinDeleteResult::Missing => {
            Err(json_error(StatusCode::NOT_FOUND, "saved skin not found"))
        }
    }
}

async fn handle_update_saved_skin(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
    Json(payload): Json<UpdateSavedSkinRequest>,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let name = payload
        .name
        .as_deref()
        .map(validate_saved_skin_name)
        .transpose()?;
    let variant = if payload.variant.is_some() {
        Some(validate_saved_skin_variant(payload.variant.as_deref())?)
    } else {
        None
    };
    let cape_id = validate_saved_skin_cape_update(&state, &payload.cape_id).await?;
    let updated = update_saved_skin_metadata(&state, texture_key, name, variant, cape_id)
        .await?
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "saved skin not found"))?;

    Ok(Json(updated))
}

async fn handle_replace_saved_skin_texture(
    State(state): State<AppState>,
    Path(path_texture_key): Path<String>,
    Query(query): Query<ReplaceSavedSkinTextureQuery>,
    body: Body,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let old_texture_key = validate_texture_key(&path_texture_key)?;
    let saved_skins = list_saved_skins(&state).await?;
    let current = saved_skins
        .iter()
        .find(|skin| skin.texture_key == old_texture_key)
        .cloned()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "saved skin not found"))?;
    let bytes = to_bytes(body, SKIN_UPLOAD_MAX_BYTES)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "skin upload is too large"))?;
    let normalized = normalize_skin_png(&bytes)?;
    let name = query
        .name
        .as_deref()
        .map(validate_saved_skin_name)
        .transpose()?
        .unwrap_or_else(|| current.name.clone());
    let variant = if query.variant.is_some() {
        validate_saved_skin_variant(query.variant.as_deref())?
    } else {
        normalized.variant_suggestion.to_string()
    };
    let cape_update = replace_texture_cape_update(&query)?;
    let cape_id = validate_saved_skin_cape_update(&state, &cape_update)
        .await?
        .unwrap_or_else(|| current.cape_id.clone());
    let new_texture_key = texture_key(&normalized.png_bytes);
    let updated = replace_saved_skin_texture(
        &state,
        old_texture_key.clone(),
        new_texture_key.clone(),
        name,
        variant,
        cape_id,
        normalized.png_bytes,
    )
    .await?
    .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "saved skin not found"))?;

    if updated.texture_key != old_texture_key {
        retarget_pending_saved_skin_apply(&old_texture_key, &updated.texture_key).await;
    }

    Ok(Json(updated))
}

async fn handle_saved_skin_file(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
) -> Result<Response<Body>, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let Some(bytes) = read_saved_skin_png(&state, texture_key).await? else {
        return Err(json_error(StatusCode::NOT_FOUND, "saved skin not found"));
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, SAVED_SKIN_FILE_CACHE_CONTROL)
        .body(Body::from(bytes))
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to build saved skin response" })),
            )
        })
}

async fn handle_apply_saved_skin(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
    Query(query): Query<ApplySavedSkinQuery>,
) -> Result<Json<SkinApplyResponse>, ApiError> {
    if query.defer.unwrap_or(false) {
        return queue_saved_skin_apply(State(state), Path(texture_key)).await;
    }

    handle_apply_saved_skin_with_client(
        State(state),
        Path(texture_key),
        MinecraftSkinUploadClient::default(),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn handle_skin_profile_reset(
    State(state): State<AppState>,
) -> Result<Json<SkinProfileResetResponse>, ApiError> {
    handle_skin_profile_reset_with_clients(
        State(state),
        MinecraftSkinResetClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn handle_skin_profile_reset_with_clients(
    State(state): State<AppState>,
    reset_client: MinecraftSkinResetClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Json<SkinProfileResetResponse>, ApiError> {
    let account = active_ready_minecraft_account_for_skin_change(
        &state,
        "Minecraft account is not ready for skin reset",
    )
    .await?;
    let saved_skins = list_saved_skins(&state).await?;
    preserve_current_profile_skin_before_change(
        &state,
        &account,
        &saved_skins,
        &texture_client,
        None,
    )
    .await?;

    let reset_profile = reset_client
        .reset(&account.access_token)
        .await
        .map_err(skin_reset_error)?;
    clear_pending_saved_skin_apply_for_login_id(&account.login_id).await;
    clear_saved_skin_applied(&state).await?;
    let profile_updated = if let Some(profile) = reset_profile {
        state
            .auth_logins()
            .update_active_current_minecraft_profile(&account.login_id, profile)
            .await
    } else {
        false
    };

    Ok(Json(SkinProfileResetResponse {
        status: "reset",
        profile_updated,
    }))
}

async fn handle_skin_cape_reset(
    State(state): State<AppState>,
) -> Result<Json<SkinCapeResetResponse>, ApiError> {
    handle_skin_cape_reset_with_clients(
        State(state),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

async fn handle_skin_cape_reset_with_clients(
    State(state): State<AppState>,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Json<SkinCapeResetResponse>, ApiError> {
    let account = active_ready_minecraft_account_for_skin_change(
        &state,
        "Minecraft account is not ready for cape reset",
    )
    .await?;
    let saved_skins = list_saved_skins(&state).await?;
    preserve_current_profile_skin_before_change(
        &state,
        &account,
        &saved_skins,
        &texture_client,
        None,
    )
    .await?;

    let cape_profile = cape_client
        .sync(&account.access_token, &account.profile, None)
        .await
        .map_err(skin_cape_error)?;
    clear_pending_saved_skin_apply_for_login_id(&account.login_id).await;
    clear_saved_skin_applied(&state).await?;
    let profile_updated = if let Some(profile) = cape_profile {
        state
            .auth_logins()
            .update_active_current_minecraft_profile(&account.login_id, profile)
            .await
    } else {
        false
    };

    Ok(Json(SkinCapeResetResponse {
        status: "reset",
        profile_updated,
    }))
}

async fn handle_flush_saved_skin_applies(
    State(state): State<AppState>,
) -> Result<Json<SkinFlushResponse>, ApiError> {
    let applied = flush_pending_saved_skin_applies_for_active_account_with_clients(
        &state,
        MinecraftSkinUploadClient::default(),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await?;

    Ok(Json(SkinFlushResponse {
        status: "flushed",
        applied,
    }))
}

async fn handle_clear_pending_saved_skin_apply(
    State(state): State<AppState>,
) -> Result<Json<SkinPendingClearResponse>, ApiError> {
    let cleared = clear_pending_saved_skin_apply_for_active_account(&state).await;

    Ok(Json(SkinPendingClearResponse {
        status: "cleared",
        cleared,
    }))
}

async fn handle_apply_saved_skin_with_client(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<Json<SkinApplyResponse>, ApiError> {
    apply_saved_skin_now_with_clients(
        &state,
        texture_key,
        skin_client,
        cape_client,
        texture_client,
    )
    .await
    .map(Json)
}

async fn apply_saved_skin_now_with_clients(
    state: &AppState,
    texture_key: String,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<SkinApplyResponse, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let account = active_ready_minecraft_account_for_skin_apply(state).await?;
    apply_saved_skin_for_account_with_clients(
        state,
        account,
        texture_key,
        skin_client,
        cape_client,
        texture_client,
    )
    .await
}

async fn apply_saved_skin_for_login_with_clients(
    state: &AppState,
    login_id: &str,
    texture_key: String,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<SkinApplyResponse, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let account = ready_minecraft_account_for_login_id_for_skin_apply(state, login_id).await?;
    apply_saved_skin_for_account_with_clients(
        state,
        account,
        texture_key,
        skin_client,
        cape_client,
        texture_client,
    )
    .await
}

async fn apply_saved_skin_for_account_with_clients(
    state: &AppState,
    account: AuthLoginMinecraftAccount,
    texture_key: String,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<SkinApplyResponse, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let saved_skins = list_saved_skins(state).await?;
    let saved_skin = saved_skins
        .iter()
        .find(|skin| skin.texture_key == texture_key)
        .cloned()
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "saved skin not found"))?;
    let Some(png_bytes) = read_saved_skin_png(state, texture_key.clone()).await? else {
        return Err(json_error(StatusCode::NOT_FOUND, "saved skin not found"));
    };

    preserve_current_profile_skin_before_apply(
        state,
        &account,
        &saved_skin,
        &saved_skins,
        &texture_client,
    )
    .await?;

    let uploaded_profile = skin_client
        .upload(&account.access_token, &saved_skin.variant, png_bytes)
        .await
        .map_err(skin_upload_error)?;
    let profile_after_upload = uploaded_profile
        .as_ref()
        .filter(|profile| !profile.capes.is_empty())
        .unwrap_or(&account.profile);
    let cape_profile = cape_client
        .sync(
            &account.access_token,
            profile_after_upload,
            saved_skin.cape_id.as_deref(),
        )
        .await
        .map_err(skin_cape_error)?;
    mark_saved_skin_applied(state, texture_key.clone())
        .await?
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "saved skin not found"))?;
    let profile_updated = if let Some(profile) = cape_profile.or(uploaded_profile) {
        state
            .auth_logins()
            .update_active_current_minecraft_profile(&account.login_id, profile)
            .await
    } else {
        false
    };

    Ok(SkinApplyResponse {
        status: "applied",
        texture_key,
        profile_updated,
    })
}

async fn queue_saved_skin_apply(
    State(state): State<AppState>,
    Path(texture_key): Path<String>,
) -> Result<Json<SkinApplyResponse>, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let account = active_ready_minecraft_account_for_skin_apply(&state).await?;
    let saved_skins = list_saved_skins(&state).await?;
    if !saved_skins
        .iter()
        .any(|skin| skin.texture_key == texture_key)
    {
        return Err(json_error(StatusCode::NOT_FOUND, "saved skin not found"));
    }

    set_pending_saved_skin_apply(
        state,
        PendingSkinApplyChange {
            login_id: account.login_id,
            texture_key: texture_key.clone(),
        },
    )
    .await;

    Ok(Json(SkinApplyResponse {
        status: "queued",
        texture_key,
        profile_updated: false,
    }))
}

async fn pending_saved_skin_apply_texture_key_for_active_account(
    state: &AppState,
) -> Option<String> {
    let login_id = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await?
        .account
        .login_id;
    PENDING_SKIN_APPLIES
        .lock()
        .await
        .pending
        .get(&login_id)
        .map(|entry| entry.change.texture_key.clone())
}

async fn retarget_pending_saved_skin_apply(old_texture_key: &str, new_texture_key: &str) {
    let mut pending = PENDING_SKIN_APPLIES.lock().await;
    for entry in pending.pending.values_mut() {
        if entry.change.texture_key == old_texture_key {
            entry.change.texture_key = new_texture_key.to_string();
        }
    }
}

pub(crate) async fn clear_pending_saved_skin_apply_for_login_id(login_id: &str) -> bool {
    PENDING_SKIN_APPLIES
        .lock()
        .await
        .pending
        .remove(login_id)
        .is_some()
}

pub(crate) async fn clear_all_pending_saved_skin_applies() -> usize {
    let mut pending = PENDING_SKIN_APPLIES.lock().await;
    let cleared = pending.pending.len();
    pending.pending.clear();
    cleared
}

async fn clear_pending_saved_skin_apply_for_active_account(state: &AppState) -> bool {
    let Some(account_state) = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
    else {
        return false;
    };

    PENDING_SKIN_APPLIES
        .lock()
        .await
        .pending
        .remove(&account_state.account.login_id)
        .is_some()
}

async fn clear_pending_saved_skin_apply_for_texture(texture_key: &str) {
    PENDING_SKIN_APPLIES
        .lock()
        .await
        .pending
        .retain(|_, entry| entry.change.texture_key != texture_key);
}

async fn active_ready_minecraft_account_for_skin_apply(
    state: &AppState,
) -> Result<AuthLoginMinecraftAccount, ApiError> {
    active_ready_minecraft_account_for_skin_change(
        state,
        "Minecraft account is not ready for skin upload",
    )
    .await
}

async fn active_ready_minecraft_account_for_skin_change(
    state: &AppState,
    not_ready_message: &'static str,
) -> Result<AuthLoginMinecraftAccount, ApiError> {
    let minecraft_state = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .ok_or_else(|| {
            json_status_error(
                StatusCode::UNAUTHORIZED,
                "Minecraft account login required",
                "minecraft_account_required",
            )
        })?;
    let account = minecraft_state.account;
    if !account.owns_minecraft_java
        || account.profile.id.trim().is_empty()
        || account.profile.name.trim().is_empty()
    {
        return Err(json_status_error(
            StatusCode::CONFLICT,
            not_ready_message,
            "minecraft_account_not_ready",
        ));
    }

    Ok(account)
}

async fn ready_minecraft_account_for_login_id_for_skin_apply(
    state: &AppState,
    login_id: &str,
) -> Result<AuthLoginMinecraftAccount, ApiError> {
    ready_minecraft_account_for_login_id_for_skin_change(
        state,
        login_id,
        "Minecraft account is not ready for skin upload",
    )
    .await
}

async fn ready_minecraft_account_for_login_id_for_skin_change(
    state: &AppState,
    login_id: &str,
    not_ready_message: &'static str,
) -> Result<AuthLoginMinecraftAccount, ApiError> {
    let minecraft_state = state
        .auth_logins()
        .current_minecraft_account_state_for_login(login_id)
        .await
        .ok_or_else(|| {
            json_status_error(
                StatusCode::UNAUTHORIZED,
                "Minecraft account login required",
                "minecraft_account_required",
            )
        })?;
    let account = minecraft_state.account;
    if !account.owns_minecraft_java
        || account.profile.id.trim().is_empty()
        || account.profile.name.trim().is_empty()
    {
        return Err(json_status_error(
            StatusCode::CONFLICT,
            not_ready_message,
            "minecraft_account_not_ready",
        ));
    }

    Ok(account)
}

pub(crate) async fn flush_pending_saved_skin_applies_for_launch(
    state: &AppState,
) -> Result<usize, (StatusCode, Json<serde_json::Value>)> {
    flush_pending_saved_skin_applies_for_active_account_with_clients(
        state,
        MinecraftSkinUploadClient::default(),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

pub async fn flush_pending_saved_skin_applies_for_shutdown(
    state: &AppState,
) -> Result<usize, (StatusCode, Json<serde_json::Value>)> {
    flush_pending_saved_skin_applies_for_active_account_with_clients(
        state,
        MinecraftSkinUploadClient::default(),
        MinecraftCapeSyncClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

#[derive(Debug)]
enum PendingSkinApplyFilter {
    Login(String),
    Generation { login_id: String, generation: u64 },
}

async fn flush_pending_saved_skin_applies_for_active_account_with_clients(
    state: &AppState,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<usize, ApiError> {
    let Some(account_state) = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
    else {
        return Ok(0);
    };

    flush_pending_saved_skin_applies_with_clients(
        state,
        PendingSkinApplyFilter::Login(account_state.account.login_id),
        skin_client,
        cape_client,
        texture_client,
    )
    .await
}

async fn set_pending_saved_skin_apply(state: AppState, change: PendingSkinApplyChange) {
    let login_id = change.login_id.clone();
    let generation = {
        let mut pending = PENDING_SKIN_APPLIES.lock().await;
        let generation = pending
            .pending
            .get(&login_id)
            .map_or(1, |entry| entry.generation.wrapping_add(1));
        pending.pending.insert(
            login_id.clone(),
            PendingSkinApplyEntry { change, generation },
        );
        generation
    };

    schedule_pending_saved_skin_apply_flush(state, login_id, generation);
}

fn schedule_pending_saved_skin_apply_flush(state: AppState, login_id: String, generation: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(SKIN_CHANGE_DEBOUNCE).await;
        if let Err(error) = flush_pending_saved_skin_applies_with_clients(
            &state,
            PendingSkinApplyFilter::Generation {
                login_id,
                generation,
            },
            MinecraftSkinUploadClient::default(),
            MinecraftCapeSyncClient::default(),
            MinecraftSkinTextureClient::default(),
        )
        .await
        {
            tracing::warn!(
                "failed to flush pending Minecraft skin change: {}",
                bounded_error_message(&error)
            );
        }
    });
}

async fn flush_pending_saved_skin_applies_with_clients(
    state: &AppState,
    filter: PendingSkinApplyFilter,
    skin_client: MinecraftSkinUploadClient,
    cape_client: MinecraftCapeSyncClient,
    texture_client: MinecraftSkinTextureClient,
) -> Result<usize, ApiError> {
    let _guard = PENDING_SKIN_APPLY_FLUSH_LOCK.lock().await;

    let entry = {
        let mut pending = PENDING_SKIN_APPLIES.lock().await;
        match &filter {
            PendingSkinApplyFilter::Login(login_id) => pending.pending.remove(login_id),
            PendingSkinApplyFilter::Generation {
                login_id,
                generation,
            } => {
                let Some(entry) = pending.pending.get(login_id) else {
                    return Ok(0);
                };
                if entry.generation != *generation {
                    return Ok(0);
                }
                pending.pending.remove(login_id)
            }
        }
    };

    let Some(entry) = entry else {
        return Ok(0);
    };

    let result = apply_saved_skin_for_login_with_clients(
        state,
        &entry.change.login_id,
        entry.change.texture_key.clone(),
        skin_client.clone(),
        cape_client.clone(),
        texture_client.clone(),
    )
    .await;

    match result {
        Ok(_) => Ok(1),
        Err(error) => {
            let login_id = entry.change.login_id.clone();
            let generation = entry.generation;
            let mut pending = PENDING_SKIN_APPLIES.lock().await;
            pending.pending.entry(login_id.clone()).or_insert(entry);
            schedule_pending_saved_skin_apply_flush(state.clone(), login_id, generation);
            Err(error)
        }
    }
}

async fn preserve_current_profile_skin_before_apply(
    state: &AppState,
    account: &AuthLoginMinecraftAccount,
    target_skin: &SavedSkinRecord,
    saved_skins: &[SavedSkinRecord],
    client: &MinecraftSkinTextureClient,
) -> Result<(), ApiError> {
    preserve_current_profile_skin_before_change(
        state,
        account,
        saved_skins,
        client,
        Some(target_skin.texture_key.as_str()),
    )
    .await
}

async fn preserve_current_profile_skin_before_change(
    state: &AppState,
    account: &AuthLoginMinecraftAccount,
    saved_skins: &[SavedSkinRecord],
    client: &MinecraftSkinTextureClient,
    skip_texture_key: Option<&str>,
) -> Result<(), ApiError> {
    let Some(profile_skin) =
        select_sane_minecraft_skin_with_prefix(&account.profile.skins, client.allowed_prefix())
    else {
        return Ok(());
    };
    let Some(texture_url) =
        sane_minecraft_texture_url_with_prefix(&profile_skin.url, client.allowed_prefix())
    else {
        return Ok(());
    };

    let cache_path = profile_skin_file_cache_path(&state.config().paths().config_dir, &texture_url);
    let bytes = match read_profile_skin_file_cache(&cache_path).await {
        Some(bytes) => bytes,
        None => client
            .download(&texture_url)
            .await
            .map_err(skin_preserve_download_error)?,
    };
    let normalized = normalize_skin_png(&bytes).map_err(|_| skin_preserve_invalid_error())?;
    let _ = write_profile_skin_file_cache(&cache_path, &normalized.png_bytes).await;
    let current_texture_key = texture_key(&normalized.png_bytes);
    if skip_texture_key == Some(current_texture_key.as_str()) {
        return Ok(());
    }

    let current_variant = skin_variant(&profile_skin.variant).to_string();
    let current_cape_id = active_minecraft_cape_id(&account.profile);
    if let Some(saved_skin) = saved_skins
        .iter()
        .find(|skin| skin.texture_key == current_texture_key)
    {
        if saved_skin.variant != current_variant || saved_skin.cape_id != current_cape_id {
            let _ = update_saved_skin_metadata(
                state,
                current_texture_key,
                None,
                Some(current_variant),
                Some(current_cape_id),
            )
            .await?;
        }
        return Ok(());
    }

    let name = validate_saved_skin_name(&default_profile_skin_name(&account.profile.name))?;
    let _ = save_saved_skin(
        state,
        current_texture_key,
        name,
        current_variant,
        SAVED_SKIN_PROFILE_SOURCE.to_string(),
        current_cape_id,
        normalized.png_bytes,
    )
    .await?;

    Ok(())
}

async fn list_saved_skins(state: &AppState) -> Result<Vec<SavedSkinRecord>, ApiError> {
    let skins = state.skins().clone();
    tokio::task::spawn_blocking(move || skins.list())
        .await
        .map_err(|_| skin_read_error(saved_skin_store_task_error()))?
        .map_err(skin_read_error)
}

async fn save_saved_skin(
    state: &AppState,
    texture_key: String,
    name: String,
    variant: String,
    source: String,
    cape_id: Option<String>,
    png_bytes: Vec<u8>,
) -> Result<SavedSkinRecord, ApiError> {
    let skins = state.skins().clone();
    tokio::task::spawn_blocking(move || {
        skins.save(texture_key, name, variant, source, cape_id, &png_bytes)
    })
    .await
    .map_err(|_| skin_write_error(saved_skin_store_task_error()))?
    .map_err(skin_write_error)
}

async fn delete_saved_skin(
    state: &AppState,
    texture_key: String,
) -> Result<SavedSkinDeleteResult, ApiError> {
    let skins = state.skins().clone();
    tokio::task::spawn_blocking(move || skins.delete_unapplied(&texture_key))
        .await
        .map_err(|_| skin_write_error(saved_skin_store_task_error()))?
        .map_err(skin_write_error)
}

async fn update_saved_skin_metadata(
    state: &AppState,
    texture_key: String,
    name: Option<String>,
    variant: Option<String>,
    cape_id: Option<Option<String>>,
) -> Result<Option<SavedSkinRecord>, ApiError> {
    let skins = state.skins().clone();
    tokio::task::spawn_blocking(move || skins.update_metadata(&texture_key, name, variant, cape_id))
        .await
        .map_err(|_| skin_write_error(saved_skin_store_task_error()))?
        .map_err(skin_write_error)
}

async fn replace_saved_skin_texture(
    state: &AppState,
    texture_key: String,
    new_texture_key: String,
    name: String,
    variant: String,
    cape_id: Option<String>,
    png_bytes: Vec<u8>,
) -> Result<Option<SavedSkinRecord>, ApiError> {
    let skins = state.skins().clone();
    tokio::task::spawn_blocking(move || {
        skins.replace_texture(
            &texture_key,
            new_texture_key,
            name,
            variant,
            cape_id,
            &png_bytes,
        )
    })
    .await
    .map_err(|_| skin_write_error(saved_skin_store_task_error()))?
    .map_err(skin_write_error)
}

async fn read_saved_skin_png(
    state: &AppState,
    texture_key: String,
) -> Result<Option<Vec<u8>>, ApiError> {
    let skins = state.skins().clone();
    tokio::task::spawn_blocking(move || skins.read_png(&texture_key))
        .await
        .map_err(|_| skin_read_error(saved_skin_store_task_error()))?
        .map_err(skin_read_error)
}

async fn mark_saved_skin_applied(
    state: &AppState,
    texture_key: String,
) -> Result<Option<String>, ApiError> {
    let skins = state.skins().clone();
    tokio::task::spawn_blocking(move || skins.mark_applied(&texture_key))
        .await
        .map_err(|_| skin_write_error(saved_skin_store_task_error()))?
        .map_err(skin_write_error)
}

async fn clear_saved_skin_applied(state: &AppState) -> Result<(), ApiError> {
    let skins = state.skins().clone();
    tokio::task::spawn_blocking(move || skins.clear_applied())
        .await
        .map_err(|_| skin_write_error(saved_skin_store_task_error()))?
        .map_err(skin_write_error)
}

fn saved_skin_store_task_error() -> std::io::Error {
    std::io::Error::other("saved skin store task failed")
}

#[derive(Clone)]
struct MinecraftSkinUsernameClient {
    http: reqwest::Client,
    profile_endpoint: String,
    session_profile_endpoint: String,
}

impl MinecraftSkinUsernameClient {
    fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            profile_endpoint: MOJANG_PROFILE_ENDPOINT.to_string(),
            session_profile_endpoint: MINECRAFT_SESSION_PROFILE_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    fn with_endpoints(profile_endpoint: String, session_profile_endpoint: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            profile_endpoint,
            session_profile_endpoint,
        }
    }

    async fn skin_profile(
        &self,
        username: &str,
        allowed_texture_prefix: &str,
    ) -> Result<MinecraftUsernameSkinProfile, MinecraftUsernameSkinError> {
        if let Some(profile) = self
            .cached_skin_profile(username, allowed_texture_prefix)
            .await
        {
            return Ok(profile);
        }

        let mojang_profile = self.mojang_profile(username).await?;
        if !is_mojang_uuid(&mojang_profile.id) || mojang_profile.name.trim().is_empty() {
            return Err(MinecraftUsernameSkinError::Unavailable);
        }

        let session_profile = self.session_profile(&mojang_profile.id).await?;
        let profile = session_profile
            .skin_profile(allowed_texture_prefix)
            .map(|skin| MinecraftUsernameSkinProfile {
                uuid: mojang_profile.id,
                name: mojang_profile.name,
                variant: skin.variant,
                texture_url: skin.texture_url,
                cape_url: skin.cape_url,
            })?;
        self.store_cached_skin_profile(username, allowed_texture_prefix, &profile)
            .await;
        Ok(profile)
    }

    async fn cached_skin_profile(
        &self,
        username: &str,
        allowed_texture_prefix: &str,
    ) -> Option<MinecraftUsernameSkinProfile> {
        let key = self.lookup_cache_key(username, allowed_texture_prefix);
        let now = Instant::now();
        let mut cache = MINECRAFT_USERNAME_SKIN_CACHE.lock().await;
        if let Some(entry) = cache.get(&key)
            && entry.expires_at > now
        {
            return Some(entry.profile.clone());
        }
        cache.remove(&key);
        None
    }

    async fn store_cached_skin_profile(
        &self,
        requested_username: &str,
        allowed_texture_prefix: &str,
        profile: &MinecraftUsernameSkinProfile,
    ) {
        let expires_at = Instant::now() + MINECRAFT_USERNAME_LOOKUP_CACHE_TTL;
        let entry = MinecraftUsernameSkinCacheEntry {
            profile: profile.clone(),
            expires_at,
        };
        let requested_key = self.lookup_cache_key(requested_username, allowed_texture_prefix);
        let resolved_key = self.lookup_cache_key(&profile.name, allowed_texture_prefix);
        let mut cache = MINECRAFT_USERNAME_SKIN_CACHE.lock().await;
        cache.retain(|_, entry| entry.expires_at > Instant::now());
        if cache.len() >= MINECRAFT_USERNAME_LOOKUP_CACHE_MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(requested_key, entry.clone());
        cache.insert(resolved_key, entry);
    }

    fn lookup_cache_key(&self, username: &str, allowed_texture_prefix: &str) -> String {
        format!(
            "{}\n{}\n{}\n{}",
            self.profile_endpoint,
            self.session_profile_endpoint,
            allowed_texture_prefix,
            username.trim().to_ascii_lowercase()
        )
    }

    async fn mojang_profile(
        &self,
        username: &str,
    ) -> Result<MojangUsernameProfile, MinecraftUsernameSkinError> {
        let response = self
            .http
            .get(format!("{}/{username}", self.profile_endpoint))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| MinecraftUsernameSkinError::Unavailable)?;
        read_minecraft_json_response(response, MOJANG_PROFILE_RESPONSE_MAX_BYTES)
            .await
            .and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| MinecraftUsernameSkinError::Unavailable)
            })
    }

    async fn session_profile(
        &self,
        uuid: &str,
    ) -> Result<MinecraftSessionProfile, MinecraftUsernameSkinError> {
        let response = self
            .http
            .get(format!("{}/{uuid}", self.session_profile_endpoint))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| MinecraftUsernameSkinError::Unavailable)?;
        read_minecraft_json_response(response, MINECRAFT_SESSION_PROFILE_RESPONSE_MAX_BYTES)
            .await
            .and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| MinecraftUsernameSkinError::Unavailable)
            })
    }
}

#[derive(Debug, Clone)]
struct MinecraftUsernameSkinProfile {
    uuid: String,
    name: String,
    variant: &'static str,
    texture_url: String,
    cape_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MinecraftUsernameSkinError {
    NotFound,
    RateLimited,
    Unavailable,
    MissingSkin,
    MalformedTextures,
}

#[derive(Debug, Deserialize)]
struct MojangUsernameProfile {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionProfile {
    #[serde(default)]
    properties: Vec<MinecraftSessionProfileProperty>,
}

impl MinecraftSessionProfile {
    fn skin_profile(
        &self,
        allowed_texture_prefix: &str,
    ) -> Result<MinecraftSessionSkinProfile, MinecraftUsernameSkinError> {
        let property = self
            .properties
            .iter()
            .find(|property| property.name == "textures")
            .ok_or(MinecraftUsernameSkinError::MissingSkin)?;
        if property.value.len() > MINECRAFT_SESSION_TEXTURES_PROPERTY_MAX_BYTES {
            return Err(MinecraftUsernameSkinError::MalformedTextures);
        }

        let bytes = BASE64_STANDARD
            .decode(property.value.as_bytes())
            .map_err(|_| MinecraftUsernameSkinError::MalformedTextures)?;
        if bytes.len() > MINECRAFT_SESSION_TEXTURES_PROPERTY_MAX_BYTES {
            return Err(MinecraftUsernameSkinError::MalformedTextures);
        }
        let textures: MinecraftSessionTextures = serde_json::from_slice(&bytes)
            .map_err(|_| MinecraftUsernameSkinError::MalformedTextures)?;
        let skin = textures
            .textures
            .skin
            .ok_or(MinecraftUsernameSkinError::MissingSkin)?;
        let texture_url = sane_minecraft_texture_url_with_prefix(&skin.url, allowed_texture_prefix)
            .ok_or(MinecraftUsernameSkinError::MissingSkin)?;
        let cape_url = textures.textures.cape.and_then(|cape| {
            sane_minecraft_texture_url_with_prefix(&cape.url, allowed_texture_prefix)
        });
        let variant = skin
            .metadata
            .as_ref()
            .map(|metadata| skin_variant(&metadata.model))
            .unwrap_or("classic");

        Ok(MinecraftSessionSkinProfile {
            texture_url,
            cape_url,
            variant,
        })
    }
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionProfileProperty {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionTextures {
    textures: MinecraftSessionTextureMap,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionTextureMap {
    #[serde(rename = "SKIN")]
    skin: Option<MinecraftSessionSkinTexture>,
    #[serde(rename = "CAPE")]
    cape: Option<MinecraftSessionCapeTexture>,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionSkinTexture {
    url: String,
    metadata: Option<MinecraftSessionSkinMetadata>,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionSkinMetadata {
    model: String,
}

#[derive(Debug, Deserialize)]
struct MinecraftSessionCapeTexture {
    url: String,
}

struct MinecraftSessionSkinProfile {
    texture_url: String,
    cape_url: Option<String>,
    variant: &'static str,
}

async fn read_minecraft_json_response(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, MinecraftUsernameSkinError> {
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::NO_CONTENT {
        return Err(MinecraftUsernameSkinError::NotFound);
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(MinecraftUsernameSkinError::RateLimited);
    }
    if status.is_client_error() || status.is_server_error() {
        return Err(MinecraftUsernameSkinError::Unavailable);
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(MinecraftUsernameSkinError::Unavailable);
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| MinecraftUsernameSkinError::Unavailable)?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(MinecraftUsernameSkinError::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

fn is_mojang_uuid(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone)]
struct MinecraftSkinTextureClient {
    http: reqwest::Client,
    allowed_prefix: String,
}

impl MinecraftSkinTextureClient {
    fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            allowed_prefix: MINECRAFT_TEXTURE_URL_PREFIX.to_string(),
        }
    }

    #[cfg(test)]
    fn with_allowed_prefix(allowed_prefix: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            allowed_prefix,
        }
    }

    fn allowed_prefix(&self) -> &str {
        &self.allowed_prefix
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, SkinTextureDownloadError> {
        if sane_minecraft_texture_url_with_prefix(url, &self.allowed_prefix).is_none() {
            return Err(SkinTextureDownloadError::InvalidUrl);
        }

        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "image/png")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| SkinTextureDownloadError::Unavailable)?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SkinTextureDownloadError::RateLimited);
        }
        if status.is_client_error() || status.is_server_error() {
            return Err(SkinTextureDownloadError::Unavailable);
        }
        if response
            .content_length()
            .is_some_and(|length| length > SKIN_UPLOAD_MAX_BYTES as u64)
        {
            return Err(SkinTextureDownloadError::TooLarge);
        }

        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| SkinTextureDownloadError::Unavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > SKIN_UPLOAD_MAX_BYTES {
                return Err(SkinTextureDownloadError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }

        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SkinTextureDownloadError {
    InvalidUrl,
    RateLimited,
    TooLarge,
    Unavailable,
}

#[derive(Clone)]
struct MinecraftSkinUploadClient {
    http: reqwest::Client,
    endpoint: String,
}

impl MinecraftSkinUploadClient {
    fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint: MINECRAFT_SKIN_UPLOAD_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    fn with_endpoint(endpoint: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint,
        }
    }

    async fn upload(
        &self,
        access_token: &str,
        variant: &str,
        png_bytes: Vec<u8>,
    ) -> Result<Option<AuthLoginMinecraftProfile>, SkinUploadError> {
        let file = reqwest::multipart::Part::bytes(png_bytes)
            .file_name("skin.png")
            .mime_str("image/png")
            .map_err(|_| SkinUploadError::Unavailable)?;
        let form = reqwest::multipart::Form::new()
            .text("variant", skin_variant(variant).to_string())
            .part("file", file);
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .multipart(form)
            .send()
            .await
            .map_err(|_| SkinUploadError::Unavailable)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SkinUploadError::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SkinUploadError::RateLimited);
        }
        if status.is_client_error() {
            return Err(SkinUploadError::Rejected);
        }
        if status.is_server_error() {
            return Err(SkinUploadError::Unavailable);
        }

        let bytes = read_skin_upload_response(response).await?;
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(None);
        }

        let profile = serde_json::from_slice::<SkinUploadMinecraftProfile>(&bytes)
            .ok()
            .map(AuthLoginMinecraftProfile::from);
        Ok(profile)
    }
}

#[derive(Clone)]
struct MinecraftSkinResetClient {
    http: reqwest::Client,
    endpoint: String,
}

impl MinecraftSkinResetClient {
    fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint: MINECRAFT_SKIN_RESET_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    fn with_endpoint(endpoint: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint,
        }
    }

    async fn reset(
        &self,
        access_token: &str,
    ) -> Result<Option<AuthLoginMinecraftProfile>, SkinUploadError> {
        let response = self
            .http
            .delete(&self.endpoint)
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| SkinUploadError::Unavailable)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SkinUploadError::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SkinUploadError::RateLimited);
        }
        if status.is_client_error() {
            return Err(SkinUploadError::Rejected);
        }
        if status.is_server_error() {
            return Err(SkinUploadError::Unavailable);
        }

        let bytes = read_minecraft_profile_response(response).await?;
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(None);
        }

        let profile = serde_json::from_slice::<SkinUploadMinecraftProfile>(&bytes)
            .ok()
            .map(AuthLoginMinecraftProfile::from);
        Ok(profile)
    }
}

#[derive(Clone)]
struct MinecraftCapeSyncClient {
    http: reqwest::Client,
    endpoint: String,
}

impl MinecraftCapeSyncClient {
    fn default() -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint: MINECRAFT_CAPE_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    fn with_endpoint(endpoint: String) -> Self {
        Self {
            http: minecraft_skin_http_client(),
            endpoint,
        }
    }

    async fn sync(
        &self,
        access_token: &str,
        profile: &AuthLoginMinecraftProfile,
        target_cape_id: Option<&str>,
    ) -> Result<Option<AuthLoginMinecraftProfile>, SkinCapeError> {
        if let Some(cape_id) = target_cape_id
            && !profile.capes.iter().any(|cape| cape.id == cape_id)
        {
            return Err(SkinCapeError::UnavailableCape);
        }
        if active_minecraft_cape_id(profile).as_deref() == target_cape_id {
            return Ok(None);
        }

        let request = match target_cape_id {
            Some(cape_id) => self
                .http
                .put(&self.endpoint)
                .json(&serde_json::json!({ "capeId": cape_id })),
            None => self.http.delete(&self.endpoint),
        };
        let response = request
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CROOPOR_USER_AGENT)
            .send()
            .await
            .map_err(|_| SkinCapeError::Unavailable)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SkinCapeError::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SkinCapeError::RateLimited);
        }
        if status.is_client_error() {
            return Err(SkinCapeError::Rejected);
        }
        if status.is_server_error() {
            return Err(SkinCapeError::Unavailable);
        }

        let bytes =
            read_minecraft_profile_response(response)
                .await
                .map_err(|error| match error {
                    SkinUploadError::TooLarge => SkinCapeError::TooLarge,
                    SkinUploadError::Unavailable => SkinCapeError::Unavailable,
                    SkinUploadError::Auth => SkinCapeError::Auth,
                    SkinUploadError::RateLimited => SkinCapeError::RateLimited,
                    SkinUploadError::Rejected => SkinCapeError::Rejected,
                })?;
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(None);
        }

        let profile = serde_json::from_slice::<SkinUploadMinecraftProfile>(&bytes)
            .ok()
            .map(AuthLoginMinecraftProfile::from);
        Ok(profile)
    }
}

async fn read_skin_upload_response(
    response: reqwest::Response,
) -> Result<Vec<u8>, SkinUploadError> {
    read_minecraft_profile_response(response).await
}

async fn read_minecraft_profile_response(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, SkinUploadError> {
    if response
        .content_length()
        .is_some_and(|length| length > MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES as u64)
    {
        return Err(SkinUploadError::TooLarge);
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| SkinUploadError::Unavailable)?
    {
        if bytes.len().saturating_add(chunk.len()) > MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES {
            return Err(SkinUploadError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

fn minecraft_skin_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(MINECRAFT_SKIN_HTTP_CONNECT_TIMEOUT)
                .timeout(MINECRAFT_SKIN_HTTP_TIMEOUT)
                .user_agent(CROOPOR_USER_AGENT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SkinUploadError {
    Auth,
    RateLimited,
    Rejected,
    TooLarge,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SkinCapeError {
    Auth,
    RateLimited,
    Rejected,
    TooLarge,
    Unavailable,
    UnavailableCape,
}

#[derive(Debug, Deserialize)]
struct SkinUploadMinecraftProfile {
    id: String,
    name: String,
    #[serde(default)]
    skins: Vec<SkinUploadMinecraftSkin>,
    #[serde(default)]
    capes: Vec<SkinUploadMinecraftCape>,
}

#[derive(Debug, Deserialize)]
struct SkinUploadMinecraftSkin {
    id: String,
    state: String,
    url: String,
    variant: String,
}

#[derive(Debug, Deserialize)]
struct SkinUploadMinecraftCape {
    id: String,
    state: String,
    url: String,
}

impl From<SkinUploadMinecraftProfile> for AuthLoginMinecraftProfile {
    fn from(profile: SkinUploadMinecraftProfile) -> Self {
        Self {
            id: profile.id,
            name: profile.name,
            skins: profile
                .skins
                .into_iter()
                .map(AuthLoginMinecraftSkin::from)
                .collect(),
            capes: profile
                .capes
                .into_iter()
                .map(AuthLoginMinecraftCape::from)
                .collect(),
        }
    }
}

impl From<SkinUploadMinecraftSkin> for AuthLoginMinecraftSkin {
    fn from(skin: SkinUploadMinecraftSkin) -> Self {
        Self {
            id: skin.id,
            state: skin.state,
            url: skin.url,
            variant: skin.variant,
        }
    }
}

impl From<SkinUploadMinecraftCape> for AuthLoginMinecraftCape {
    fn from(cape: SkinUploadMinecraftCape) -> Self {
        Self {
            id: cape.id,
            state: cape.state,
            url: cape.url,
        }
    }
}

struct NormalizedSkinPng {
    original_width: u32,
    original_height: u32,
    variant_suggestion: &'static str,
    png_bytes: Vec<u8>,
}

fn normalize_skin_png(bytes: &[u8]) -> Result<NormalizedSkinPng, ApiError> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin upload must be a PNG",
        ));
    }

    let decoded = decode_skin_png(bytes)?;
    if decoded.width != SKIN_WIDTH || !matches!(decoded.height, LEGACY_SKIN_HEIGHT | SKIN_HEIGHT) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin image must be 64x64 or 64x32",
        ));
    }

    let original_height = decoded.height;
    let normalized_rgba = if original_height == LEGACY_SKIN_HEIGHT {
        normalize_legacy_skin_rgba(&decoded.rgba)
    } else {
        decoded.rgba
    };
    let variant_suggestion = if original_height == LEGACY_SKIN_HEIGHT {
        "classic"
    } else {
        suggest_skin_variant(&normalized_rgba)
    };
    let png_bytes = encode_skin_png(&normalized_rgba)?;

    Ok(NormalizedSkinPng {
        original_width: decoded.width,
        original_height,
        variant_suggestion,
        png_bytes,
    })
}

struct DecodedSkinPng {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn decode_skin_png(bytes: &[u8]) -> Result<DecodedSkinPng, ApiError> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "skin upload must be a valid PNG"))?;
    let info = reader.info();
    if info.width != SKIN_WIDTH || !matches!(info.height, LEGACY_SKIN_HEIGHT | SKIN_HEIGHT) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin image must be 64x64 or 64x32",
        ));
    }

    let mut buffer = vec![0; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "skin upload must be a valid PNG"))?;
    let rgba = png_frame_to_rgba(
        &buffer[..frame.buffer_size()],
        frame.color_type,
        frame.bit_depth,
    )?;

    Ok(DecodedSkinPng {
        width: frame.width,
        height: frame.height,
        rgba,
    })
}

fn png_frame_to_rgba(
    data: &[u8],
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
) -> Result<Vec<u8>, ApiError> {
    if bit_depth != png::BitDepth::Eight {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin upload must be a valid PNG",
        ));
    }

    match color_type {
        png::ColorType::Rgba => Ok(data.to_vec()),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(data.len() / 3 * 4);
            for pixel in data.chunks_exact(3) {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            Ok(rgba)
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(data.len() / 2 * 4);
            for pixel in data.chunks_exact(2) {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            Ok(rgba)
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(data.len() * 4);
            for value in data {
                rgba.extend_from_slice(&[*value, *value, *value, 255]);
            }
            Ok(rgba)
        }
        png::ColorType::Indexed => Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin upload must be a valid PNG",
        )),
    }
}

fn normalize_legacy_skin_rgba(rgba: &[u8]) -> Vec<u8> {
    let mut normalized = vec![0; (SKIN_WIDTH * SKIN_HEIGHT * 4) as usize];
    let row_len = (SKIN_WIDTH * 4) as usize;
    for row in 0..LEGACY_SKIN_HEIGHT as usize {
        let offset = row * row_len;
        normalized[offset..offset + row_len].copy_from_slice(&rgba[offset..offset + row_len]);
    }
    normalized
}

fn suggest_skin_variant(rgba: &[u8]) -> &'static str {
    for y in 20..32 {
        for x in 54..56 {
            let alpha_index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
            if rgba.get(alpha_index).copied().unwrap_or(255) != 0 {
                return "classic";
            }
        }
    }
    "slim"
}

fn render_skin_head_png(skin_png: &[u8], size: u32) -> Result<Vec<u8>, ApiError> {
    let decoded = decode_skin_png(skin_png)?;
    let mut head_rgba = vec![0; (size * size * 4) as usize];
    draw_scaled_skin_region(&decoded.rgba, &mut head_rgba, 8, 8, size, false);
    draw_scaled_skin_region(&decoded.rgba, &mut head_rgba, 40, 8, size, true);
    encode_rgba_png(&head_rgba, size, size, "failed to build skin head image")
}

fn draw_scaled_skin_region(
    source_rgba: &[u8],
    target_rgba: &mut [u8],
    source_x: u32,
    source_y: u32,
    target_size: u32,
    blend: bool,
) {
    for target_y in 0..target_size {
        for target_x in 0..target_size {
            let skin_x = source_x + target_x * 8 / target_size;
            let skin_y = source_y + target_y * 8 / target_size;
            let source_index = ((skin_y * SKIN_WIDTH + skin_x) * 4) as usize;
            let target_index = ((target_y * target_size + target_x) * 4) as usize;
            let source_pixel = [
                source_rgba[source_index],
                source_rgba[source_index + 1],
                source_rgba[source_index + 2],
                source_rgba[source_index + 3],
            ];

            if blend {
                blend_rgba_pixel(target_rgba, target_index, source_pixel);
            } else {
                target_rgba[target_index..target_index + 4].copy_from_slice(&source_pixel);
            }
        }
    }
}

fn blend_rgba_pixel(target_rgba: &mut [u8], target_index: usize, source: [u8; 4]) {
    let source_alpha = source[3] as u16;
    if source_alpha == 0 {
        return;
    }
    if source_alpha == 255 {
        target_rgba[target_index..target_index + 4].copy_from_slice(&source);
        return;
    }

    let inverse_alpha = 255 - source_alpha;
    for channel in 0..3 {
        let source_channel = source[channel] as u16;
        let target_channel = target_rgba[target_index + channel] as u16;
        target_rgba[target_index + channel] =
            ((source_channel * source_alpha + target_channel * inverse_alpha) / 255) as u8;
    }
    let target_alpha = target_rgba[target_index + 3] as u16;
    target_rgba[target_index + 3] =
        (source_alpha + target_alpha * inverse_alpha / 255).min(255) as u8;
}

fn encode_skin_png(rgba: &[u8]) -> Result<Vec<u8>, ApiError> {
    encode_rgba_png(
        rgba,
        SKIN_WIDTH,
        SKIN_HEIGHT,
        "failed to normalize skin image",
    )
}

fn encode_rgba_png(
    rgba: &[u8],
    width: u32,
    height: u32,
    error_message: &'static str,
) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, error_message))?;
        writer
            .write_image_data(rgba)
            .map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, error_message))?;
    }
    Ok(bytes)
}

fn texture_key(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut key = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    key
}

fn validate_saved_skin_name(value: &str) -> Result<String, ApiError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "skin name is required"));
    }
    if trimmed.chars().count() > SAVED_SKIN_NAME_MAX_CHARS {
        return Err(json_error(StatusCode::BAD_REQUEST, "skin name is too long"));
    }
    if trimmed
        .chars()
        .any(|value| value.is_control() || matches!(value, '/' | '\\'))
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "skin name contains unsupported characters",
        ));
    }

    Ok(trimmed.to_string())
}

fn default_profile_skin_name(profile_name: &str) -> String {
    format!("{} profile skin", profile_name.trim())
        .chars()
        .take(SAVED_SKIN_NAME_MAX_CHARS)
        .collect()
}

fn default_username_skin_name(profile_name: &str) -> String {
    format!("{} skin", profile_name.trim())
        .chars()
        .take(SAVED_SKIN_NAME_MAX_CHARS)
        .collect()
}

fn validate_saved_skin_upload_source(value: Option<&str>) -> Result<String, ApiError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(SAVED_SKIN_SOURCE.to_string());
    };
    if value == SAVED_SKIN_DEFAULT_SOURCE {
        return Ok(SAVED_SKIN_DEFAULT_SOURCE.to_string());
    }

    Err(json_error(
        StatusCode::BAD_REQUEST,
        "skin source is not supported for uploads",
    ))
}

fn validate_saved_skin_variant(value: Option<&str>) -> Result<String, ApiError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok("classic".to_string());
    };
    if value.eq_ignore_ascii_case("classic") {
        return Ok("classic".to_string());
    }
    if value.eq_ignore_ascii_case("slim") {
        return Ok("slim".to_string());
    }

    Err(json_error(
        StatusCode::BAD_REQUEST,
        "skin variant must be classic or slim",
    ))
}

async fn validate_saved_skin_cape_update(
    state: &AppState,
    value: &CapeUpdate,
) -> Result<Option<Option<String>>, ApiError> {
    let cape_id = match value {
        CapeUpdate::Unchanged => return Ok(None),
        CapeUpdate::Clear => return Ok(Some(None)),
        CapeUpdate::Set(cape_id) => cape_id,
    };
    let cape_id = validate_saved_skin_cape_id(cape_id)?;
    let minecraft_state = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .ok_or_else(|| {
            json_status_error(
                StatusCode::CONFLICT,
                "Minecraft account is required to select a cape",
                "minecraft_account_required",
            )
        })?;
    if !minecraft_state
        .account
        .profile
        .capes
        .iter()
        .any(|cape| cape.id == cape_id)
    {
        return Err(json_status_error(
            StatusCode::BAD_REQUEST,
            "Minecraft cape is not available for this account",
            "minecraft_cape_unavailable",
        ));
    }

    Ok(Some(Some(cape_id)))
}

fn replace_texture_cape_update(
    query: &ReplaceSavedSkinTextureQuery,
) -> Result<CapeUpdate, ApiError> {
    if query.clear_cape.unwrap_or(false) {
        if query
            .cape_id
            .as_deref()
            .is_some_and(|cape_id| !cape_id.trim().is_empty())
        {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "cape_id and clear_cape cannot both be set",
            ));
        }
        return Ok(CapeUpdate::Clear);
    }

    if let Some(cape_id) = query.cape_id.as_deref() {
        return Ok(CapeUpdate::Set(cape_id.to_string()));
    }

    Ok(CapeUpdate::Unchanged)
}

fn validate_saved_skin_cape_id(value: &str) -> Result<String, ApiError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 80 {
        return Err(json_error(StatusCode::BAD_REQUEST, "invalid cape id"));
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(json_error(StatusCode::BAD_REQUEST, "invalid cape id"));
    }

    Ok(trimmed.to_string())
}

fn validate_texture_key(value: &str) -> Result<String, ApiError> {
    let trimmed = value.trim();
    if trimmed.len() != 64
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(json_error(StatusCode::BAD_REQUEST, "invalid texture key"));
    }

    Ok(trimmed.to_string())
}

struct OfflineIdentity {
    username: String,
    uuid: String,
    variant: &'static str,
}

fn select_offline_identity(
    query_username: Option<&str>,
    config_username: &str,
) -> Result<OfflineIdentity, ApiError> {
    let selected_username = query_username
        .map(str::trim)
        .filter(|username| !username.is_empty())
        .unwrap_or(config_username);
    let username = validate_username(selected_username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;
    let uuid = offline_uuid(&username);
    let variant = offline_variant(&uuid);

    Ok(OfflineIdentity {
        username,
        uuid,
        variant,
    })
}

fn json_error(status: StatusCode, message: &'static str) -> ApiError {
    (status, Json(serde_json::json!({ "error": message })))
}

fn bounded_error_message(error: &ApiError) -> &str {
    error
        .1
        .0
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("skin operation failed")
}

fn json_status_error(
    status: StatusCode,
    message: &'static str,
    status_code: &'static str,
) -> ApiError {
    (
        status,
        Json(serde_json::json!({
            "error": message,
            "status": status_code,
        })),
    )
}

fn skin_upload_error(error: SkinUploadError) -> ApiError {
    match error {
        SkinUploadError::Auth => json_status_error(
            StatusCode::UNAUTHORIZED,
            "Minecraft skin upload authorization failed",
            "minecraft_skin_auth_failed",
        ),
        SkinUploadError::RateLimited => json_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Minecraft skin upload is rate limited. Try again later.",
            "minecraft_skin_rate_limited",
        ),
        SkinUploadError::Rejected => json_status_error(
            StatusCode::BAD_REQUEST,
            "Minecraft rejected the saved skin",
            "minecraft_skin_rejected",
        ),
        SkinUploadError::TooLarge => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft skin upload response is too large",
            "minecraft_skin_response_too_large",
        ),
        SkinUploadError::Unavailable => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft skin upload is unavailable. Try again later.",
            "minecraft_skin_unavailable",
        ),
    }
}

fn skin_reset_error(error: SkinUploadError) -> ApiError {
    match error {
        SkinUploadError::Auth => json_status_error(
            StatusCode::UNAUTHORIZED,
            "Minecraft skin reset authorization failed",
            "minecraft_skin_reset_auth_failed",
        ),
        SkinUploadError::RateLimited => json_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Minecraft skin reset is rate limited. Try again later.",
            "minecraft_skin_reset_rate_limited",
        ),
        SkinUploadError::Rejected => json_status_error(
            StatusCode::BAD_REQUEST,
            "Minecraft rejected the skin reset",
            "minecraft_skin_reset_rejected",
        ),
        SkinUploadError::TooLarge => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft skin reset response is too large",
            "minecraft_skin_reset_response_too_large",
        ),
        SkinUploadError::Unavailable => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft skin reset is unavailable. Try again later.",
            "minecraft_skin_reset_unavailable",
        ),
    }
}

fn skin_cape_error(error: SkinCapeError) -> ApiError {
    match error {
        SkinCapeError::Auth => json_status_error(
            StatusCode::UNAUTHORIZED,
            "Minecraft cape authorization failed",
            "minecraft_cape_auth_failed",
        ),
        SkinCapeError::RateLimited => json_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Minecraft cape change is rate limited. Try again later.",
            "minecraft_cape_rate_limited",
        ),
        SkinCapeError::Rejected => json_status_error(
            StatusCode::BAD_REQUEST,
            "Minecraft rejected the cape change",
            "minecraft_cape_rejected",
        ),
        SkinCapeError::TooLarge => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft cape response is too large",
            "minecraft_cape_response_too_large",
        ),
        SkinCapeError::Unavailable => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft cape change is unavailable. Try again later.",
            "minecraft_cape_unavailable",
        ),
        SkinCapeError::UnavailableCape => json_status_error(
            StatusCode::BAD_REQUEST,
            "Saved skin cape is not available for this Minecraft account",
            "minecraft_cape_not_available",
        ),
    }
}

fn skin_texture_download_error(error: SkinTextureDownloadError) -> ApiError {
    match error {
        SkinTextureDownloadError::InvalidUrl => json_status_error(
            StatusCode::CONFLICT,
            "Minecraft profile does not have a usable skin texture",
            "minecraft_profile_skin_missing",
        ),
        SkinTextureDownloadError::RateLimited => json_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Minecraft profile skin download is rate limited. Try again later.",
            "minecraft_profile_skin_rate_limited",
        ),
        SkinTextureDownloadError::TooLarge => json_status_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Minecraft profile skin is too large",
            "minecraft_profile_skin_too_large",
        ),
        SkinTextureDownloadError::Unavailable => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft profile skin download is unavailable. Try again later.",
            "minecraft_profile_skin_unavailable",
        ),
    }
}

fn cape_texture_download_error(error: SkinTextureDownloadError) -> ApiError {
    match error {
        SkinTextureDownloadError::InvalidUrl => json_status_error(
            StatusCode::CONFLICT,
            "Minecraft cape does not have a usable texture",
            "minecraft_cape_texture_missing",
        ),
        SkinTextureDownloadError::RateLimited => json_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Minecraft cape texture download is rate limited. Try again later.",
            "minecraft_cape_texture_rate_limited",
        ),
        SkinTextureDownloadError::TooLarge => json_status_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Minecraft cape texture is too large",
            "minecraft_cape_texture_too_large",
        ),
        SkinTextureDownloadError::Unavailable => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft cape texture download is unavailable. Try again later.",
            "minecraft_cape_texture_unavailable",
        ),
    }
}

fn cape_texture_invalid_error() -> ApiError {
    json_status_error(
        StatusCode::BAD_GATEWAY,
        "Minecraft cape texture is invalid",
        "minecraft_cape_texture_invalid",
    )
}

fn skin_preserve_download_error(error: SkinTextureDownloadError) -> ApiError {
    match error {
        SkinTextureDownloadError::InvalidUrl => json_status_error(
            StatusCode::CONFLICT,
            "Current Minecraft profile skin cannot be preserved before changing it",
            "minecraft_profile_skin_preserve_missing",
        ),
        SkinTextureDownloadError::RateLimited => json_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Current Minecraft profile skin preservation is rate limited. Try again later.",
            "minecraft_profile_skin_preserve_rate_limited",
        ),
        SkinTextureDownloadError::TooLarge => json_status_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Current Minecraft profile skin is too large to preserve before changing it",
            "minecraft_profile_skin_preserve_too_large",
        ),
        SkinTextureDownloadError::Unavailable => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Current Minecraft profile skin cannot be preserved right now. Try again later.",
            "minecraft_profile_skin_preserve_unavailable",
        ),
    }
}

fn skin_preserve_invalid_error() -> ApiError {
    json_status_error(
        StatusCode::CONFLICT,
        "Current Minecraft profile skin cannot be preserved before changing it",
        "minecraft_profile_skin_preserve_invalid",
    )
}

fn skin_username_lookup_error(error: MinecraftUsernameSkinError) -> ApiError {
    match error {
        MinecraftUsernameSkinError::NotFound => json_status_error(
            StatusCode::NOT_FOUND,
            "Minecraft player not found",
            "minecraft_player_not_found",
        ),
        MinecraftUsernameSkinError::RateLimited => json_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Minecraft profile lookup is rate limited. Try again later.",
            "minecraft_profile_lookup_rate_limited",
        ),
        MinecraftUsernameSkinError::Unavailable => json_status_error(
            StatusCode::BAD_GATEWAY,
            "Minecraft profile lookup is unavailable. Try again later.",
            "minecraft_profile_lookup_unavailable",
        ),
        MinecraftUsernameSkinError::MissingSkin => json_status_error(
            StatusCode::CONFLICT,
            "Minecraft player profile does not have a usable skin texture",
            "minecraft_username_skin_missing",
        ),
        MinecraftUsernameSkinError::MalformedTextures => json_status_error(
            StatusCode::CONFLICT,
            "Minecraft player profile skin textures are malformed",
            "minecraft_username_skin_malformed",
        ),
    }
}

fn skin_read_error(_error: std::io::Error) -> ApiError {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not read saved skins. Check app data permissions and try again.",
    )
}

fn skin_write_error(_error: std::io::Error) -> ApiError {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not update saved skins. Check app data permissions and try again.",
    )
}

fn offline_variant(uuid: &str) -> &'static str {
    // Mirrors Java String.hashCode parity so the offline hint is stable across platforms.
    let hash = uuid.bytes().fold(0_i32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(i32::from(byte))
    });
    if hash & 1 == 0 { "classic" } else { "slim" }
}

fn clamp_head_size(size: Option<u32>) -> u32 {
    size.unwrap_or(DEFAULT_HEAD_SIZE)
        .clamp(MIN_HEAD_SIZE, MAX_HEAD_SIZE)
}

fn offline_head_svg(uuid: &str, size: u32) -> String {
    let seed = fnv1a64(uuid.as_bytes());
    let background = mix_color(seed, 0x111827, 0x374151);
    let outline = mix_color(seed.rotate_left(7), 0x111827, 0x1f2937);
    let skin = mix_color(seed.rotate_left(17), 0xc58c65, 0xf1c27d);
    let accent = mix_color(seed.rotate_left(31), 0x2563eb, 0x22c55e);
    let shadow = mix_color(seed.rotate_left(43), 0x4b5563, 0x7c2d12);
    let palette = [background, outline, skin, accent, shadow];
    let mut state = seed;

    let mut svg = String::with_capacity(2600);
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 8 8" shape-rendering="crispEdges">"#
    );

    for y in 0..8 {
        for x in 0..8 {
            state = splitmix64(state.wrapping_add(((y * 8 + x) as u64) + 1));
            let palette_index = if x == 0 || x == 7 || y == 0 || y == 7 {
                1
            } else {
                (state as usize % (palette.len() - 2)) + 2
            };
            let _ = write!(
                svg,
                r##"<rect x="{x}" y="{y}" width="1" height="1" fill="#{:06x}"/>"##,
                palette[palette_index]
            );
        }
    }

    svg.push_str("</svg>");
    svg
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn mix_color(seed: u64, first: u32, second: u32) -> u32 {
    let amount = (seed & 0xff) as u32;
    let inverse = 255 - amount;
    let red = (((first >> 16) & 0xff) * inverse + ((second >> 16) & 0xff) * amount) / 255;
    let green = (((first >> 8) & 0xff) * inverse + ((second >> 8) & 0xff) * amount) / 255;
    let blue = ((first & 0xff) * inverse + (second & 0xff) * amount) / 255;

    (red << 16) | (green << 8) | blue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use crate::state::{
        AuthLoginMinecraftProfile, AuthLoginMinecraftSkin, NewAuthLoginMinecraftAccount,
        NewAuthLoginMsaToken,
    };
    use axum::{
        body::{Bytes, to_bytes},
        extract::State as AxumState,
        http::HeaderMap,
    };
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::{fs, path::PathBuf, sync::Arc};
    use tokio::sync::mpsc;

    #[test]
    fn skin_profile_router_builds_with_from_profile_route() {
        let _ = router();
    }

    #[tokio::test]
    async fn skin_profile_defaults_to_configured_username() {
        let fixture = TestFixture::new("default-username", "ConfigUser");

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(response.source, "default");
        assert_eq!(response.texture_url, None);
        assert_eq!(
            response.head_url,
            Some("/api/v1/skin/head?username=ConfigUser".to_string())
        );
    }

    #[tokio::test]
    async fn skin_profile_query_username_overrides_config_username() {
        let fixture = TestFixture::new("query-username", "ConfigUser");

        let response = fixture
            .profile(Some("QueryUser".to_string()), None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.username, "QueryUser");
        assert_eq!(response.uuid, offline_uuid("QueryUser"));
    }

    #[tokio::test]
    async fn skin_profile_blank_username_falls_back_to_config_username() {
        let fixture = TestFixture::new("blank-username", "ConfigUser");

        let response = fixture
            .profile(Some("   ".to_string()), None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
    }

    #[tokio::test]
    async fn skin_profile_uses_active_minecraft_profile_when_no_username_query() {
        let fixture = TestFixture::new("online-profile", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![
                    minecraft_skin(
                        "inactive",
                        "INACTIVE",
                        "https://textures.minecraft.net/texture/inactive",
                        "classic",
                    ),
                    minecraft_skin(
                        "active",
                        "ACTIVE",
                        "https://textures.minecraft.net/texture/activeTexture123",
                        "SLIM",
                    ),
                ],
            ))
            .await;

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "online");
        assert_eq!(response.username, "MinecraftName");
        assert_eq!(response.uuid, "MinecraftName-id");
        assert_eq!(response.source, "minecraft_profile_skin");
        assert_eq!(response.variant, "slim");
        assert_eq!(
            response.texture_url.as_deref(),
            Some("https://textures.minecraft.net/texture/activeTexture123")
        );
        assert_eq!(response.head_url, None);
    }

    #[tokio::test]
    async fn skin_profile_ignores_preserved_stale_minecraft_profile() {
        let fixture = TestFixture::new("online-profile-stale", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "OldMinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/oldTexture123",
                    "slim",
                )],
            ))
            .await;
        fixture
            .state
            .auth_logins()
            .refresh_with_msa_token(
                NewAuthLoginMsaToken {
                    access_token: "new-msa-access-token".to_string(),
                    refresh_token: Some("new-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                "old-msa-refresh-token",
            )
            .await
            .expect("msa-only refresh");

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(response.source, "default");
        assert_eq!(
            response.variant,
            offline_variant(&offline_uuid("ConfigUser"))
        );
        assert_eq!(response.texture_url, None);
        assert_eq!(
            response.head_url,
            Some("/api/v1/skin/head?username=ConfigUser".to_string())
        );
        assert_eq!(
            fixture
                .state
                .auth_logins()
                .active_minecraft_account()
                .await
                .expect("preserved raw minecraft account")
                .profile
                .name,
            "OldMinecraftName"
        );
        assert_eq!(
            fixture
                .state
                .auth_logins()
                .active_current_minecraft_account_state()
                .await,
            None
        );
    }

    #[tokio::test]
    async fn skin_profile_username_query_keeps_offline_override_with_active_minecraft_profile() {
        let fixture = TestFixture::new("online-query-override", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/active",
                    "slim",
                )],
            ))
            .await;

        let response = fixture
            .profile(Some("QueryUser".to_string()), None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "offline");
        assert_eq!(response.username, "QueryUser");
        assert_eq!(response.uuid, offline_uuid("QueryUser"));
        assert_eq!(response.texture_url, None);
    }

    #[tokio::test]
    async fn skin_profile_expired_minecraft_profile_falls_back_to_offline() {
        let fixture = TestFixture::new("online-expired", "ConfigUser");
        fixture
            .add_minecraft_account_with_expiry(
                test_profile(
                    "MinecraftName",
                    vec![minecraft_skin(
                        "active",
                        "ACTIVE",
                        "https://textures.minecraft.net/texture/active",
                        "slim",
                    )],
                ),
                0,
            )
            .await;

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(
            fixture.state.auth_logins().active_minecraft_account().await,
            None
        );
    }

    #[tokio::test]
    async fn skin_profile_omits_unsane_minecraft_texture_url() {
        let fixture = TestFixture::new("online-bad-texture", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://example.com/texture/active",
                    "unknown",
                )],
            ))
            .await;

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.auth_mode, "online");
        assert_eq!(response.source, "minecraft_profile_skin");
        assert_eq!(response.variant, "classic");
        assert_eq!(response.texture_url, None);
    }

    #[tokio::test]
    async fn skin_profile_without_active_skin_uses_first_sane_skin() {
        let fixture = TestFixture::new("online-first-sane-texture", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![
                    minecraft_skin("bad", "INACTIVE", "https://example.com/texture/bad", "slim"),
                    minecraft_skin(
                        "good",
                        "INACTIVE",
                        "https://textures.minecraft.net/texture/goodTexture123",
                        "classic",
                    ),
                ],
            ))
            .await;

        let response = fixture
            .profile(None, None)
            .await
            .expect("profile response")
            .0;

        assert_eq!(response.source, "minecraft_profile_skin");
        assert_eq!(response.variant, "classic");
        assert_eq!(
            response.texture_url.as_deref(),
            Some("https://textures.minecraft.net/texture/goodTexture123")
        );
    }

    #[tokio::test]
    async fn skin_profile_file_downloads_normalizes_active_skin() {
        let fixture = TestFixture::new("profile-file-active", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![
                    minecraft_skin(
                        "inactive",
                        "INACTIVE",
                        &format!("{texture_prefix}inactiveTexture123"),
                        "classic",
                    ),
                    minecraft_skin(
                        "active",
                        "ACTIVE",
                        &format!("{texture_prefix}activeTexture123"),
                        "slim",
                    ),
                ],
            ))
            .await;

        let file = fixture
            .profile_file(texture_prefix.clone())
            .await
            .expect("profile skin file");
        let request = requests.recv().await.expect("texture request");
        let content_type = file
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let cache_control = file
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        assert_eq!(request.path, "/texture/activeTexture123");
        assert_eq!(request.accept.as_deref(), Some("image/png"));
        assert_eq!(request.user_agent.as_deref(), Some(CROOPOR_USER_AGENT));
        assert_eq!(content_type.as_deref(), Some("image/png"));
        assert_eq!(
            cache_control.as_deref(),
            Some(PROFILE_SKIN_FILE_CACHE_CONTROL)
        );
        assert_eq!(response_bytes(file).await, normalized.png_bytes);
    }

    #[tokio::test]
    async fn skin_profile_file_texture_query_fetches_requested_profile_texture() {
        let fixture = TestFixture::new("profile-file-query-texture", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let active_texture_url = format!("{texture_prefix}activeTexture123");
        let requested_texture_url = format!("{texture_prefix}otherAccountTexture456");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    &active_texture_url,
                    "slim",
                )],
            ))
            .await;

        let file = fixture
            .profile_file_with_texture(texture_prefix.clone(), Some(requested_texture_url.clone()))
            .await
            .expect("profile skin file");
        let request = requests.recv().await.expect("texture request");
        let cache_path = profile_skin_file_cache_path(
            &fixture.state.config().paths().config_dir,
            &requested_texture_url,
        );

        assert_eq!(request.path, "/texture/otherAccountTexture456");
        assert_eq!(response_bytes(file).await, normalized.png_bytes);
        assert_eq!(
            tokio::fs::read(cache_path)
                .await
                .expect("read requested profile cache"),
            normalized.png_bytes
        );
    }

    #[tokio::test]
    async fn skin_profile_file_cache_hit_avoids_second_texture_request() {
        let fixture = TestFixture::new("profile-file-cache-hit", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let texture_url = format!("{texture_prefix}activeTexture123");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin("active", "ACTIVE", &texture_url, "slim")],
            ))
            .await;

        let first = fixture
            .profile_file(texture_prefix.clone())
            .await
            .expect("first profile skin file");
        let request = requests.recv().await.expect("texture request");
        let second = fixture
            .profile_file(texture_prefix)
            .await
            .expect("second profile skin file");
        let cache_path =
            profile_skin_file_cache_path(&fixture.state.config().paths().config_dir, &texture_url);

        assert_eq!(request.path, "/texture/activeTexture123");
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(response_bytes(first).await, normalized.png_bytes);
        assert_eq!(response_bytes(second).await, normalized.png_bytes);
        assert_eq!(
            tokio::fs::read(cache_path)
                .await
                .expect("read profile cache"),
            normalized.png_bytes
        );
    }

    #[tokio::test]
    async fn skin_profile_file_corrupt_cache_redownloads_and_refreshes_cache() {
        let fixture = TestFixture::new("profile-file-corrupt-cache", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let texture_url = format!("{texture_prefix}activeTexture123");
        let cache_path =
            profile_skin_file_cache_path(&fixture.state.config().paths().config_dir, &texture_url);
        tokio::fs::create_dir_all(cache_path.parent().expect("profile cache parent"))
            .await
            .expect("create profile cache dir");
        tokio::fs::write(&cache_path, b"\x89PNG\r\n\x1a\n/home/zero/corrupt-cache")
            .await
            .expect("write corrupt profile cache");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin("active", "ACTIVE", &texture_url, "slim")],
            ))
            .await;

        let file = fixture
            .profile_file(texture_prefix)
            .await
            .expect("profile skin file");
        let request = requests.recv().await.expect("texture request");

        assert_eq!(request.path, "/texture/activeTexture123");
        assert_eq!(response_bytes(file).await, normalized.png_bytes);
        assert_eq!(
            tokio::fs::read(cache_path)
                .await
                .expect("read refreshed cache"),
            normalized.png_bytes
        );
    }

    #[tokio::test]
    async fn skin_cape_file_downloads_available_account_cape() {
        let fixture = TestFixture::new("cape-file-download", "ConfigUser");
        let cape_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(cape_png.clone()))
                .await;
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                Vec::new(),
                vec![minecraft_cape(
                    "cape-id",
                    "INACTIVE",
                    &format!("{texture_prefix}capeTexture123"),
                )],
            ))
            .await;

        let file = fixture
            .cape_file("cape-id", texture_prefix.clone())
            .await
            .expect("profile cape file");
        let request = requests.recv().await.expect("cape texture request");
        let content_type = file
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let cache_control = file
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        assert_eq!(request.path, "/texture/capeTexture123");
        assert_eq!(request.accept.as_deref(), Some("image/png"));
        assert_eq!(request.user_agent.as_deref(), Some(CROOPOR_USER_AGENT));
        assert_eq!(content_type.as_deref(), Some("image/png"));
        assert_eq!(
            cache_control.as_deref(),
            Some(PROFILE_CAPE_FILE_CACHE_CONTROL)
        );
        assert_eq!(response_bytes(file).await, cape_png);
    }

    #[tokio::test]
    async fn skin_cape_file_cache_hit_avoids_second_texture_request() {
        let fixture = TestFixture::new("cape-file-cache-hit", "ConfigUser");
        let cape_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(cape_png.clone()))
                .await;
        let texture_url = format!("{texture_prefix}capeTexture123");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                Vec::new(),
                vec![minecraft_cape("cape-id", "INACTIVE", &texture_url)],
            ))
            .await;

        let first = fixture
            .cape_file("cape-id", texture_prefix.clone())
            .await
            .expect("first profile cape file");
        let request = requests.recv().await.expect("cape texture request");
        let second = fixture
            .cape_file("cape-id", texture_prefix)
            .await
            .expect("second profile cape file");
        let cache_path =
            profile_cape_file_cache_path(&fixture.state.config().paths().config_dir, &texture_url);

        assert_eq!(request.path, "/texture/capeTexture123");
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(response_bytes(first).await, cape_png);
        assert_eq!(response_bytes(second).await, cape_png);
        assert_eq!(
            tokio::fs::read(cache_path).await.expect("read cape cache"),
            cape_png
        );
    }

    #[tokio::test]
    async fn skin_cape_file_requires_available_sane_cape_texture() {
        let fixture = TestFixture::new("cape-file-bad-texture", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                Vec::new(),
                vec![minecraft_cape(
                    "cape-id",
                    "INACTIVE",
                    "https://example.com/texture/capeTexture123",
                )],
            ))
            .await;

        let error = fixture
            .cape_file("cape-id", "http://127.0.0.1:9/texture/".to_string())
            .await
            .expect_err("bad cape texture should fail");
        let missing = fixture
            .cape_file("missing-cape", "http://127.0.0.1:9/texture/".to_string())
            .await
            .expect_err("missing cape should fail");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft cape does not have a usable texture",
                "status": "minecraft_cape_texture_missing",
            })
        );
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
        assert_eq!(
            missing.1.0,
            serde_json::json!({
                "error": "Minecraft cape is not available for this account",
                "status": "minecraft_cape_not_found",
            })
        );
    }

    #[tokio::test]
    async fn skin_profile_file_missing_active_account_returns_bounded_error() {
        let fixture = TestFixture::new("profile-file-missing-active", "ConfigUser");

        let error = fixture
            .profile_file("http://127.0.0.1:9/texture/".to_string())
            .await
            .expect_err("missing active account should fail");

        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft account login required",
                "status": "minecraft_account_required",
            })
        );
    }

    #[tokio::test]
    async fn skin_profile_file_not_ready_account_returns_bounded_error() {
        let fixture = TestFixture::new("profile-file-not-ready", "ConfigUser");
        fixture
            .add_minecraft_account_with_ownership(
                test_profile(
                    "MinecraftName",
                    vec![minecraft_skin(
                        "active",
                        "ACTIVE",
                        "https://textures.minecraft.net/texture/activeTexture123",
                        "slim",
                    )],
                ),
                false,
            )
            .await;

        let error = fixture
            .profile_file("http://127.0.0.1:9/texture/".to_string())
            .await
            .expect_err("not ready account should fail");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft account is not ready for profile skin preview",
                "status": "minecraft_account_not_ready",
            })
        );
    }

    #[tokio::test]
    async fn skin_profile_file_requires_sane_texture_url() {
        let fixture = TestFixture::new("profile-file-bad-texture", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://example.com/texture/active?token=secret",
                    "slim",
                )],
            ))
            .await;

        let error = fixture
            .profile_file("http://127.0.0.1:9/texture/".to_string())
            .await
            .expect_err("unsane texture should fail");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft profile does not have a usable skin texture",
                "status": "minecraft_profile_skin_missing",
            })
        );
    }

    #[test]
    fn minecraft_texture_url_sanitization_is_strict() {
        assert_eq!(
            sane_minecraft_texture_url("https://textures.minecraft.net/texture/abcDEF123"),
            Some("https://textures.minecraft.net/texture/abcDEF123".to_string())
        );
        assert_eq!(
            sane_minecraft_texture_url("http://textures.minecraft.net/texture/abc"),
            Some("https://textures.minecraft.net/texture/abc".to_string())
        );
        assert_eq!(
            sane_minecraft_texture_url("https://textures.minecraft.net.evil/texture/abc"),
            None
        );
        assert_eq!(
            sane_minecraft_texture_url("http://textures.minecraft.net.evil/texture/abc"),
            None
        );
        assert_eq!(
            sane_minecraft_texture_url("https://textures.minecraft.net/texture/abc?token=secret"),
            None
        );
        assert_eq!(
            sane_minecraft_texture_url(" https://textures.minecraft.net/texture/abc"),
            None
        );
    }

    #[tokio::test]
    async fn skin_profile_invalid_username_returns_json_error() {
        let fixture = TestFixture::new("invalid-username", "ConfigUser");

        let error = fixture
            .profile(Some("bad name".to_string()), None)
            .await
            .expect_err("invalid username should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "Letters, numbers, and underscores only." })
        );
    }

    #[test]
    fn offline_variant_is_deterministic_and_known() {
        let uuid = offline_uuid("ConfigUser");

        let first = offline_variant(&uuid);
        let second = offline_variant(&uuid);

        assert_eq!(first, second);
        assert!(matches!(first, "classic" | "slim"));
    }

    #[tokio::test]
    async fn skin_head_defaults_to_configured_username() {
        let fixture = TestFixture::new("head-default-username", "ConfigUser");

        let response = fixture.head(None, None).await.expect("head response");
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let cache_control = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response_body(response).await;

        assert_eq!(content_type.as_deref(), Some("image/svg+xml"));
        assert_eq!(cache_control.as_deref(), Some(HEAD_CACHE_CONTROL));
        assert!(body.contains("<svg"));
        assert_eq!(
            body,
            offline_head_svg(&offline_uuid("ConfigUser"), DEFAULT_HEAD_SIZE)
        );
    }

    #[tokio::test]
    async fn skin_head_query_username_overrides_config_username() {
        let fixture = TestFixture::new("head-query-username", "ConfigUser");

        let default_response = fixture.head(None, None).await.expect("default head");
        let query_response = fixture
            .head(Some("QueryUser".to_string()), None)
            .await
            .expect("query head");

        assert_ne!(
            response_body(default_response).await,
            response_body(query_response).await
        );
    }

    #[tokio::test]
    async fn skin_head_blank_username_falls_back_to_config_username() {
        let fixture = TestFixture::new("head-blank-username", "ConfigUser");

        let default_response = fixture.head(None, None).await.expect("default head");
        let blank_response = fixture
            .head(Some("   ".to_string()), None)
            .await
            .expect("blank head");

        assert_eq!(
            response_body(default_response).await,
            response_body(blank_response).await
        );
    }

    #[tokio::test]
    async fn skin_head_invalid_username_returns_json_error() {
        let fixture = TestFixture::new("head-invalid-username", "ConfigUser");

        let error = fixture
            .head(Some("bad name".to_string()), None)
            .await
            .expect_err("invalid username should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "Letters, numbers, and underscores only." })
        );
    }

    #[tokio::test]
    async fn skin_head_size_clamps_to_sane_bounds() {
        let fixture = TestFixture::new("head-size-clamps", "ConfigUser");

        let small_response = fixture.head(None, Some(1)).await.expect("small head");
        let large_response = fixture.head(None, Some(9999)).await.expect("large head");

        assert!(
            response_body(small_response)
                .await
                .contains(r#"width="16""#)
        );
        assert!(
            response_body(large_response)
                .await
                .contains(r#"width="256""#)
        );
    }

    #[tokio::test]
    async fn skin_normalize_64x64_png_succeeds() {
        let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);

        let response = normalize_skin_body(png)
            .await
            .expect("normalize response")
            .0;

        assert_eq!(response.variant_suggestion, "classic");
        assert_eq!(response.original_width, SKIN_WIDTH);
        assert_eq!(response.original_height, SKIN_HEIGHT);
        assert_eq!(response.normalized_width, SKIN_WIDTH);
        assert_eq!(response.normalized_height, SKIN_HEIGHT);
        assert!(response.normalized_byte_size > 0);
        assert_texture_key(&response.texture_key);
        assert!(
            response
                .normalized_data_url
                .starts_with("data:image/png;base64,")
        );
    }

    #[tokio::test]
    async fn skin_normalize_64x64_png_suggests_slim_when_arm_region_is_transparent() {
        let png = test_slim_skin_png();

        let response = normalize_skin_body(png)
            .await
            .expect("normalize response")
            .0;

        assert_eq!(response.variant_suggestion, "slim");
        assert_eq!(response.original_width, SKIN_WIDTH);
        assert_eq!(response.original_height, SKIN_HEIGHT);
        assert_texture_key(&response.texture_key);
    }

    #[tokio::test]
    async fn skin_normalize_64x32_png_normalizes_to_64x64() {
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let expected = normalize_skin_png(&png).expect("expected normalized skin");

        let response = normalize_skin_body(png.clone())
            .await
            .expect("normalize response")
            .0;
        let repeated = normalize_skin_body(png)
            .await
            .expect("repeat normalize response")
            .0;

        assert_eq!(response.original_width, SKIN_WIDTH);
        assert_eq!(response.original_height, LEGACY_SKIN_HEIGHT);
        assert_eq!(response.variant_suggestion, "classic");
        assert_eq!(response.normalized_width, SKIN_WIDTH);
        assert_eq!(response.normalized_height, SKIN_HEIGHT);
        assert_eq!(response.texture_key, repeated.texture_key);
        assert_eq!(response.normalized_byte_size, repeated.normalized_byte_size);
        assert_eq!(response.normalized_byte_size, expected.png_bytes.len());
        assert_eq!(
            response.normalized_data_url,
            format!(
                "data:image/png;base64,{}",
                BASE64_STANDARD.encode(expected.png_bytes)
            )
        );
        assert_texture_key(&response.texture_key);
    }

    #[tokio::test]
    async fn skin_normalize_rejects_non_png() {
        let error = normalize_skin_body(b"/home/zero/not-a-skin".to_vec())
            .await
            .expect_err("non-png should fail");

        assert_skin_normalize_error(error, StatusCode::BAD_REQUEST, "skin upload must be a PNG");
    }

    #[tokio::test]
    async fn skin_normalize_rejects_bad_dimensions() {
        let error = normalize_skin_body(test_skin_png(32, 32))
            .await
            .expect_err("bad dimensions should fail");

        assert_skin_normalize_error(
            error,
            StatusCode::BAD_REQUEST,
            "skin image must be 64x64 or 64x32",
        );
    }

    #[tokio::test]
    async fn skin_normalize_rejects_malformed_png_with_bounded_error() {
        let mut body = PNG_SIGNATURE.to_vec();
        body.extend_from_slice(b"/home/zero/corrupt-skin");

        let error = normalize_skin_body(body)
            .await
            .expect_err("malformed png should fail");

        assert_skin_normalize_error(
            error,
            StatusCode::BAD_REQUEST,
            "skin upload must be a valid PNG",
        );
    }

    #[tokio::test]
    async fn skin_normalize_rejects_oversized_body() {
        let error = normalize_skin_body(vec![0; SKIN_UPLOAD_MAX_BYTES + 1])
            .await
            .expect_err("oversized body should fail");

        assert_skin_normalize_error(
            error,
            StatusCode::PAYLOAD_TOO_LARGE,
            "skin upload is too large",
        );
    }

    #[tokio::test]
    async fn skin_saved_list_initially_empty() {
        let fixture = TestFixture::new("saved-list-empty", "ConfigUser");

        let response = fixture.saved_skins().await.expect("saved skins").0;

        assert!(response.skins.is_empty());
        assert_eq!(response.pending_apply_texture_key, None);
    }

    #[tokio::test]
    async fn skin_saved_save_lists_metadata_without_bytes() {
        let fixture = TestFixture::new("saved-save-list", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);

        let saved = fixture
            .save_skin("  My Skin  ", Some("slim".to_string()), png.clone())
            .await
            .expect("save skin")
            .0;
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let file = fixture
            .saved_skin_file(&saved.texture_key)
            .await
            .expect("saved skin file");
        let content_type = file
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let cache_control = file
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let file_bytes = response_bytes(file).await;
        let normalized = normalize_skin_png(&png).expect("normalized skin");

        assert_eq!(listed.skins, vec![saved.clone()]);
        assert_eq!(saved.name, "My Skin");
        assert_eq!(saved.variant, "slim");
        assert_eq!(saved.source, SAVED_SKIN_SOURCE);
        assert_eq!(saved.byte_size, normalized.png_bytes.len());
        assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
        assert_texture_key(&saved.texture_key);
        assert_eq!(content_type.as_deref(), Some("image/png"));
        assert_eq!(
            cache_control.as_deref(),
            Some(SAVED_SKIN_FILE_CACHE_CONTROL)
        );
        assert_eq!(file_bytes, normalized.png_bytes);
    }

    #[tokio::test]
    async fn skin_saved_save_uses_normalized_slim_suggestion_when_variant_is_omitted() {
        let fixture = TestFixture::new("saved-save-slim-suggestion", "ConfigUser");
        let png = test_slim_skin_png();

        let saved = fixture
            .save_skin("Detected Slim", None, png)
            .await
            .expect("save skin")
            .0;

        assert_eq!(saved.variant, "slim");
    }

    #[tokio::test]
    async fn skin_saved_save_selects_available_cape() {
        let fixture = TestFixture::new("saved-save-cape", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                Vec::new(),
                vec![minecraft_cape(
                    "cape-id",
                    "INACTIVE",
                    "https://textures.minecraft.net/texture/capeTexture",
                )],
            ))
            .await;

        let saved = handle_save_skin(
            State(fixture.state.clone()),
            Query(SaveSkinQuery {
                name: Some("Cape Skin".to_string()),
                variant: None,
                cape_id: Some("cape-id".to_string()),
                source: None,
            }),
            Body::from(test_skin_png(SKIN_WIDTH, SKIN_HEIGHT)),
        )
        .await
        .expect("save skin with cape")
        .0;
        let listed = fixture.saved_skins().await.expect("saved skins").0;

        assert_eq!(saved.cape_id.as_deref(), Some("cape-id"));
        assert_eq!(listed.skins, vec![saved]);
    }

    #[tokio::test]
    async fn skin_saved_save_rejects_unavailable_cape() {
        let fixture = TestFixture::new("saved-save-unavailable-cape", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                Vec::new(),
                vec![minecraft_cape(
                    "owned-cape",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/capeTexture",
                )],
            ))
            .await;

        let error = handle_save_skin(
            State(fixture.state.clone()),
            Query(SaveSkinQuery {
                name: Some("Cape Skin".to_string()),
                variant: None,
                cape_id: Some("missing-cape".to_string()),
                source: None,
            }),
            Body::from(test_skin_png(SKIN_WIDTH, SKIN_HEIGHT)),
        )
        .await
        .expect_err("unavailable cape should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft cape is not available for this account",
                "status": "minecraft_cape_unavailable",
            }),
        );
    }

    #[tokio::test]
    async fn skin_saved_duplicate_texture_key_updates_metadata() {
        let fixture = TestFixture::new("saved-duplicate", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);

        let first = fixture
            .save_skin("First", None, png.clone())
            .await
            .expect("first save")
            .0;
        let second = fixture
            .save_skin("Second", Some("slim".to_string()), png)
            .await
            .expect("second save")
            .0;
        let listed = fixture.saved_skins().await.expect("saved skins").0;

        assert_eq!(first.texture_key, second.texture_key);
        assert_eq!(first.created_at, second.created_at);
        assert!(second.updated_at >= first.updated_at);
        assert_eq!(second.name, "Second");
        assert_eq!(second.variant, "slim");
        assert_eq!(listed.skins, vec![second]);
    }

    #[tokio::test]
    async fn skin_saved_update_metadata_changes_name_and_variant() {
        let fixture = TestFixture::new("saved-update-metadata", "ConfigUser");
        let saved = fixture
            .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;

        let updated = fixture
            .update_saved_skin(
                &saved.texture_key,
                serde_json::json!({
                    "name": " Renamed Skin ",
                    "variant": "slim"
                }),
            )
            .await
            .expect("update skin")
            .0;
        let listed = fixture.saved_skins().await.expect("saved skins").0;

        assert_eq!(updated.texture_key, saved.texture_key);
        assert_eq!(updated.created_at, saved.created_at);
        assert!(updated.updated_at >= saved.updated_at);
        assert_eq!(updated.name, "Renamed Skin");
        assert_eq!(updated.variant, "slim");
        assert_eq!(updated.cape_id, saved.cape_id);
        assert_eq!(updated.applied_at, saved.applied_at);
        assert_eq!(updated.byte_size, saved.byte_size);
        assert_eq!(listed.skins, vec![updated]);
    }

    #[tokio::test]
    async fn skin_saved_update_metadata_selects_and_clears_available_cape() {
        let fixture = TestFixture::new("saved-update-cape", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                Vec::new(),
                vec![minecraft_cape(
                    "cape-id",
                    "INACTIVE",
                    "https://textures.minecraft.net/texture/capeTexture",
                )],
            ))
            .await;
        let saved = fixture
            .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;

        let with_cape = fixture
            .update_saved_skin(
                &saved.texture_key,
                serde_json::json!({ "cape_id": "cape-id" }),
            )
            .await
            .expect("select cape")
            .0;
        let without_cape = fixture
            .update_saved_skin(&saved.texture_key, serde_json::json!({ "cape_id": null }))
            .await
            .expect("clear cape")
            .0;

        assert_eq!(with_cape.cape_id.as_deref(), Some("cape-id"));
        assert_eq!(without_cape.cape_id, None);
    }

    #[tokio::test]
    async fn skin_saved_update_metadata_rejects_invalid_values() {
        let fixture = TestFixture::new("saved-update-invalid", "ConfigUser");
        let saved = fixture
            .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;

        let invalid_name = fixture
            .update_saved_skin(
                &saved.texture_key,
                serde_json::json!({ "name": "bad/name" }),
            )
            .await
            .expect_err("invalid name should fail");
        let invalid_variant = fixture
            .update_saved_skin(&saved.texture_key, serde_json::json!({ "variant": "wide" }))
            .await
            .expect_err("invalid variant should fail");
        let invalid_key = fixture
            .update_saved_skin(
                "../not-a-texture-key",
                serde_json::json!({ "name": "Valid" }),
            )
            .await
            .expect_err("invalid key should fail");
        let missing = fixture
            .update_saved_skin(&"0".repeat(64), serde_json::json!({ "name": "Missing" }))
            .await
            .expect_err("missing skin should fail");

        assert_eq!(invalid_name.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            invalid_name.1.0,
            serde_json::json!({ "error": "skin name contains unsupported characters" })
        );
        assert_eq!(invalid_variant.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            invalid_variant.1.0,
            serde_json::json!({ "error": "skin variant must be classic or slim" })
        );
        assert_eq!(invalid_key.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            invalid_key.1.0,
            serde_json::json!({ "error": "invalid texture key" })
        );
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
        assert_eq!(
            missing.1.0,
            serde_json::json!({ "error": "saved skin not found" })
        );
    }

    #[tokio::test]
    async fn skin_saved_replace_texture_changes_identity_and_file() {
        let fixture = TestFixture::new("saved-replace-texture", "ConfigUser");
        let saved = fixture
            .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;
        let replacement_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let replacement_normalized =
            normalize_skin_png(&replacement_png).expect("replacement normalized");

        let updated = fixture
            .replace_saved_skin_texture(
                &saved.texture_key,
                ReplaceSavedSkinTextureQuery {
                    name: Some(" Replaced Skin ".to_string()),
                    variant: Some("slim".to_string()),
                    ..ReplaceSavedSkinTextureQuery::default()
                },
                replacement_png,
            )
            .await
            .expect("replace texture")
            .0;
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let replacement_file = fixture
            .saved_skin_file(&updated.texture_key)
            .await
            .expect("replacement file");
        let old_file = fixture
            .saved_skin_file(&saved.texture_key)
            .await
            .expect_err("old texture key should not be listed");

        assert_ne!(updated.texture_key, saved.texture_key);
        assert_eq!(
            updated.texture_key,
            texture_key(&replacement_normalized.png_bytes)
        );
        assert_eq!(updated.created_at, saved.created_at);
        assert!(updated.updated_at >= saved.updated_at);
        assert_eq!(updated.name, "Replaced Skin");
        assert_eq!(updated.variant, "slim");
        assert_eq!(updated.source, saved.source);
        assert_eq!(updated.cape_id, saved.cape_id);
        assert_eq!(updated.applied_at, None);
        assert_eq!(updated.byte_size, replacement_normalized.png_bytes.len());
        assert_eq!(listed.skins, vec![updated.clone()]);
        assert_eq!(
            response_bytes(replacement_file).await,
            replacement_normalized.png_bytes
        );
        assert_eq!(old_file.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn skin_saved_replace_texture_clears_stale_applied_state() {
        let fixture = TestFixture::new("saved-replace-clears-applied", "ConfigUser");
        let saved = fixture
            .save_skin("Original", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;
        fixture
            .state
            .skins()
            .mark_applied(&saved.texture_key)
            .expect("mark skin applied");

        let updated = fixture
            .replace_saved_skin_texture(
                &saved.texture_key,
                ReplaceSavedSkinTextureQuery {
                    name: Some(saved.name.clone()),
                    variant: Some(saved.variant.clone()),
                    ..ReplaceSavedSkinTextureQuery::default()
                },
                test_slim_skin_png(),
            )
            .await
            .expect("replace texture")
            .0;

        assert_ne!(updated.texture_key, saved.texture_key);
        assert_eq!(updated.applied_at, None);
    }

    #[tokio::test]
    async fn skin_saved_replace_texture_retargets_pending_apply() {
        let fixture = TestFixture::new("saved-replace-retargets-pending", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin(
                "Queued",
                None,
                test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 29),
            )
            .await
            .expect("save skin")
            .0;
        let _ = fixture
            .queue_saved_skin_apply(&saved.texture_key)
            .await
            .expect("queue apply");

        let updated = fixture
            .replace_saved_skin_texture(
                &saved.texture_key,
                ReplaceSavedSkinTextureQuery {
                    name: Some("Queued Replacement".to_string()),
                    variant: Some("slim".to_string()),
                    ..ReplaceSavedSkinTextureQuery::default()
                },
                test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 53),
            )
            .await
            .expect("replace texture")
            .0;
        let listed = fixture.saved_skins().await.expect("saved skins").0;

        assert_ne!(updated.texture_key, saved.texture_key);
        assert_eq!(
            listed.pending_apply_texture_key.as_deref(),
            Some(updated.texture_key.as_str())
        );
    }

    #[tokio::test]
    async fn skin_saved_delete_removes_local_skin() {
        let fixture = TestFixture::new("saved-delete", "ConfigUser");
        let saved = fixture
            .save_skin("Delete Me", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;

        let deleted = fixture
            .delete_saved_skin(&saved.texture_key)
            .await
            .expect("delete skin")
            .0;
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let file_error = fixture
            .saved_skin_file(&saved.texture_key)
            .await
            .expect_err("file should be gone");

        assert_eq!(deleted, serde_json::json!({ "status": "deleted" }));
        assert!(listed.skins.is_empty());
        assert_eq!(file_error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            file_error.1.0,
            serde_json::json!({ "error": "saved skin not found" })
        );
    }

    #[tokio::test]
    async fn skin_saved_delete_clears_matching_pending_apply() {
        let fixture = TestFixture::new("saved-delete-clears-pending", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin(
                "Queued Delete",
                None,
                test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 31),
            )
            .await
            .expect("save skin")
            .0;
        let _ = fixture
            .queue_saved_skin_apply(&saved.texture_key)
            .await
            .expect("queue apply");
        let listed_before = fixture.saved_skins().await.expect("saved skins").0;

        let _ = fixture
            .delete_saved_skin(&saved.texture_key)
            .await
            .expect("delete queued skin");
        let listed_after = fixture.saved_skins().await.expect("saved skins").0;

        assert_eq!(
            listed_before.pending_apply_texture_key.as_deref(),
            Some(saved.texture_key.as_str())
        );
        assert_eq!(listed_after.pending_apply_texture_key, None);
        assert!(listed_after.skins.is_empty());
    }

    #[tokio::test]
    async fn skin_saved_delete_rejects_applied_skin() {
        let fixture = TestFixture::new("saved-delete-rejects-applied", "ConfigUser");
        let saved = fixture
            .save_skin(
                "Applied Delete",
                None,
                test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 43),
            )
            .await
            .expect("save skin")
            .0;
        fixture
            .state
            .skins()
            .mark_applied(&saved.texture_key)
            .expect("mark applied")
            .expect("saved skin should exist");

        let error = fixture
            .delete_saved_skin(&saved.texture_key)
            .await
            .expect_err("applied skin delete should fail");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let file = fixture
            .saved_skin_file(&saved.texture_key)
            .await
            .expect("applied skin file should remain readable");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "applied saved skin cannot be deleted; reset or apply another skin first"
            })
        );
        assert_eq!(listed.skins.len(), 1);
        assert_eq!(listed.skins[0].texture_key, saved.texture_key);
        assert!(listed.skins[0].applied_at.is_some());
        assert_eq!(file.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn skin_saved_delete_rejects_invalid_texture_key() {
        let fixture = TestFixture::new("saved-invalid-delete", "ConfigUser");

        let error = fixture
            .delete_saved_skin("../not-a-texture-key")
            .await
            .expect_err("invalid key should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "invalid texture key" })
        );
    }

    #[tokio::test]
    async fn skin_saved_rejects_invalid_name() {
        let fixture = TestFixture::new("saved-invalid-name", "ConfigUser");

        let error = fixture
            .save_skin("bad/name", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect_err("invalid name should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "skin name contains unsupported characters" })
        );
    }

    #[tokio::test]
    async fn skin_saved_read_error_is_bounded_json() {
        let fixture = TestFixture::new("saved-read-error", "ConfigUser");
        let skin_dir = fixture.root.join("config").join("skins");
        fs::create_dir_all(&skin_dir).expect("create skin dir");
        fs::write(skin_dir.join("index.json"), "{not-json").expect("write bad index");

        let error = fixture
            .saved_skins()
            .await
            .expect_err("bad index should fail");

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Could not read saved skins. Check app data permissions and try again."
            })
        );
    }

    #[tokio::test]
    async fn skin_saved_write_error_is_bounded_json() {
        let fixture = TestFixture::new("saved-write-error", "ConfigUser");
        let skin_dir = fixture.root.join("config").join("skins");
        fs::create_dir_all(&skin_dir).expect("create skin dir");
        fs::write(skin_dir.join("files"), "blocking file").expect("write blocking file");

        let error = fixture
            .save_skin("Blocked", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect_err("blocked file dir should fail");

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Could not update saved skins. Check app data permissions and try again."
            })
        );
    }

    #[tokio::test]
    async fn skin_profile_save_from_profile_downloads_normalizes_and_saves_active_skin() {
        let fixture = TestFixture::new("profile-save-active", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![
                    minecraft_skin(
                        "inactive",
                        "INACTIVE",
                        &format!("{texture_prefix}inactiveTexture123"),
                        "classic",
                    ),
                    minecraft_skin(
                        "active",
                        "ACTIVE",
                        &format!("{texture_prefix}activeTexture123"),
                        "SLIM",
                    ),
                ],
            ))
            .await;

        let saved = fixture
            .save_skin_from_profile(
                SaveSkinFromProfileRequest::default(),
                texture_prefix.clone(),
            )
            .await
            .expect("save profile skin")
            .0;
        let request = requests.recv().await.expect("texture request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let file = fixture
            .saved_skin_file(&saved.texture_key)
            .await
            .expect("saved skin file");

        assert_eq!(request.path, "/texture/activeTexture123");
        assert_eq!(request.accept.as_deref(), Some("image/png"));
        assert_eq!(request.user_agent.as_deref(), Some(CROOPOR_USER_AGENT));
        assert_eq!(saved.name, "MinecraftName profile skin");
        assert_eq!(normalized.variant_suggestion, "classic");
        assert_eq!(saved.variant, normalized.variant_suggestion);
        assert_eq!(saved.source, SAVED_SKIN_PROFILE_SOURCE);
        assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
        assert_eq!(saved.byte_size, normalized.png_bytes.len());
        assert_eq!(listed.skins, vec![saved.clone()]);
        assert_eq!(response_bytes(file).await, normalized.png_bytes);
    }

    #[tokio::test]
    async fn skin_profile_save_from_profile_reuses_profile_file_cache() {
        let fixture = TestFixture::new("profile-save-reuses-profile-cache", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    &format!("{texture_prefix}activeTexture123"),
                    "classic",
                )],
            ))
            .await;

        let preview = fixture
            .profile_file(texture_prefix.clone())
            .await
            .expect("profile skin preview");
        let request = requests.recv().await.expect("texture request");
        assert_eq!(response_bytes(preview).await, normalized.png_bytes);

        let saved = fixture
            .save_skin_from_profile(SaveSkinFromProfileRequest::default(), texture_prefix)
            .await
            .expect("save profile skin from cache")
            .0;

        assert_eq!(request.path, "/texture/activeTexture123");
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(saved.name, "MinecraftName profile skin");
        assert_eq!(saved.source, SAVED_SKIN_PROFILE_SOURCE);
        assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
        assert_eq!(saved.byte_size, normalized.png_bytes.len());
    }

    #[tokio::test]
    async fn skin_profile_save_from_profile_accepts_name_and_variant_override() {
        let fixture = TestFixture::new("profile-save-overrides", "ConfigUser");
        let (texture_prefix, mut requests) = skin_profile_texture_test_server(
            SkinProfileTextureServerMode::Png(test_skin_png(SKIN_WIDTH, SKIN_HEIGHT)),
        )
        .await;
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    &format!("{texture_prefix}activeTexture123"),
                    "classic",
                )],
            ))
            .await;

        let saved = fixture
            .save_skin_from_profile(
                SaveSkinFromProfileRequest {
                    name: Some("  Profile Copy  ".to_string()),
                    variant: Some("SLIM".to_string()),
                    mark_current: None,
                },
                texture_prefix,
            )
            .await
            .expect("save profile skin")
            .0;
        let _ = requests.recv().await.expect("texture request");

        assert_eq!(saved.name, "Profile Copy");
        assert_eq!(saved.variant, "slim");
    }

    #[tokio::test]
    async fn skin_profile_save_from_profile_missing_active_account_returns_bounded_error() {
        let fixture = TestFixture::new("profile-save-missing-active", "ConfigUser");

        let error = fixture
            .save_skin_from_profile(
                SaveSkinFromProfileRequest::default(),
                "http://127.0.0.1:9/texture/".to_string(),
            )
            .await
            .expect_err("missing active account should fail");

        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft account login required",
                "status": "minecraft_account_required",
            })
        );
    }

    #[tokio::test]
    async fn skin_profile_save_from_profile_requires_sane_texture_url() {
        let fixture = TestFixture::new("profile-save-bad-texture", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    "https://example.com/texture/active?token=secret",
                    "slim",
                )],
            ))
            .await;

        let error = fixture
            .save_skin_from_profile(
                SaveSkinFromProfileRequest::default(),
                "http://127.0.0.1:9/texture/".to_string(),
            )
            .await
            .expect_err("unsane texture should fail");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft profile does not have a usable skin texture",
                "status": "minecraft_profile_skin_missing",
            })
        );
    }

    #[tokio::test]
    async fn skin_profile_save_from_profile_bounds_texture_download_size() {
        let fixture = TestFixture::new("profile-save-oversized-texture", "ConfigUser");
        let (texture_prefix, mut requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Oversized).await;
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "active",
                    "ACTIVE",
                    &format!("{texture_prefix}activeTexture123"),
                    "slim",
                )],
            ))
            .await;

        let error = fixture
            .save_skin_from_profile(SaveSkinFromProfileRequest::default(), texture_prefix)
            .await
            .expect_err("oversized texture should fail");
        let _ = requests.recv().await.expect("texture request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;

        assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft profile skin is too large",
                "status": "minecraft_profile_skin_too_large",
            })
        );
        assert!(listed.skins.is_empty());
    }

    #[tokio::test]
    async fn skin_lookup_resolves_username_skin_model_cape_and_head_url() {
        let fixture = TestFixture::new("username-lookup-success", "ConfigUser");
        let texture_prefix = "http://127.0.0.1:9/texture/".to_string();
        let (profile_endpoint, session_endpoint, mut profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("slim".to_string()),
                cape_url: Some(format!("{texture_prefix}usernameCape123")),
            })
            .await;

        let lookup = fixture
            .lookup(
                "  QueryUser  ",
                Some(96),
                profile_endpoint,
                session_endpoint,
                texture_prefix.clone(),
            )
            .await
            .expect("lookup username skin")
            .0;
        let profile_request = profile_requests.recv().await.expect("profile request");
        let session_request = profile_requests.recv().await.expect("session request");

        assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
        assert_eq!(
            session_request.path,
            "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(lookup.username, "ResolvedName");
        assert_eq!(lookup.uuid, "0123456789abcdef0123456789abcdef");
        assert_eq!(lookup.source, SAVED_SKIN_USERNAME_SOURCE);
        assert_eq!(lookup.variant, "slim");
        assert_eq!(
            lookup.texture_url,
            format!("{texture_prefix}usernameTexture123")
        );
        assert_eq!(
            lookup.texture_file_url,
            "/api/v1/skin/lookup/file?username=ResolvedName"
        );
        assert_eq!(
            lookup.cape_url,
            Some(format!("{texture_prefix}usernameCape123"))
        );
        assert_eq!(
            lookup.head_url,
            "/api/v1/skin/lookup/head?username=ResolvedName&size=96"
        );
    }

    #[tokio::test]
    async fn skin_lookup_media_reuses_recent_profile_lookup() {
        let fixture = TestFixture::new("username-lookup-profile-cache", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let (profile_endpoint, session_endpoint, mut profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("classic".to_string()),
                cape_url: Some(format!("{texture_prefix}usernameCape123")),
            })
            .await;

        let lookup = fixture
            .lookup(
                "QueryUser",
                Some(96),
                profile_endpoint.clone(),
                session_endpoint.clone(),
                texture_prefix.clone(),
            )
            .await
            .expect("lookup username skin")
            .0;
        let profile_request = profile_requests.recv().await.expect("profile request");
        let session_request = profile_requests.recv().await.expect("session request");

        let file = fixture
            .lookup_file(
                &lookup.username,
                None,
                profile_endpoint.clone(),
                session_endpoint.clone(),
                texture_prefix.clone(),
            )
            .await
            .expect("lookup skin file from cached profile");
        let head = fixture
            .lookup_head(
                &lookup.username,
                Some(32),
                profile_endpoint.clone(),
                session_endpoint.clone(),
                texture_prefix.clone(),
            )
            .await
            .expect("lookup head from cached profile");
        let cape = fixture
            .lookup_cape(
                &lookup.username,
                None,
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect("lookup cape from cached profile");

        let file_texture_request = texture_requests.recv().await.expect("skin texture request");
        let cape_texture_request = texture_requests.recv().await.expect("cape texture request");

        assert_eq!(lookup.username, "ResolvedName");
        assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
        assert_eq!(
            session_request.path,
            "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(file_texture_request.path, "/texture/usernameTexture123");
        assert_eq!(cape_texture_request.path, "/texture/usernameCape123");
        assert!(matches!(
            profile_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            texture_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let _ = response_bytes(file).await;
        let _ = response_bytes(head).await;
        let _ = response_bytes(cape).await;
    }

    #[tokio::test]
    async fn skin_lookup_file_downloads_normalizes_and_caches_username_skin() {
        let fixture = TestFixture::new("username-lookup-file", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("legacy skin should normalize");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let texture_url = format!("{texture_prefix}usernameTexture123");
        let (profile_endpoint, session_endpoint, mut profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: texture_url.clone(),
                model: Some("classic".to_string()),
                cape_url: None,
            })
            .await;

        let response = fixture
            .lookup_file(
                "QueryUser",
                None,
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect("lookup username skin file");
        let profile_request = profile_requests.recv().await.expect("profile request");
        let session_request = profile_requests.recv().await.expect("session request");
        let texture_request = texture_requests.recv().await.expect("texture request");
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let cache_control = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let cache_path =
            profile_skin_file_cache_path(&fixture.state.config().paths().config_dir, &texture_url);

        assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
        assert_eq!(
            session_request.path,
            "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(texture_request.path, "/texture/usernameTexture123");
        assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
        assert_eq!(
            texture_request.user_agent.as_deref(),
            Some(CROOPOR_USER_AGENT)
        );
        assert_eq!(content_type.as_deref(), Some("image/png"));
        assert_eq!(
            cache_control.as_deref(),
            Some(PROFILE_SKIN_FILE_CACHE_CONTROL)
        );
        assert_eq!(response_bytes(response).await, normalized.png_bytes);
        assert_eq!(
            tokio::fs::read(cache_path)
                .await
                .expect("read lookup skin cache"),
            normalized.png_bytes
        );
    }

    #[tokio::test]
    async fn skin_lookup_file_cache_hit_avoids_second_texture_request() {
        let fixture = TestFixture::new("username-lookup-file-cache-hit", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png.clone())).await;
        let texture_url = format!("{texture_prefix}usernameTexture123");
        let (profile_endpoint, session_endpoint, _profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url,
                model: Some("classic".to_string()),
                cape_url: None,
            })
            .await;

        let first = fixture
            .lookup_file(
                "QueryUser",
                None,
                profile_endpoint.clone(),
                session_endpoint.clone(),
                texture_prefix.clone(),
            )
            .await
            .expect("first username skin file lookup");
        let texture_request = texture_requests.recv().await.expect("texture request");
        let second = fixture
            .lookup_file(
                "QueryUser",
                None,
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect("second username skin file lookup");

        assert_eq!(texture_request.path, "/texture/usernameTexture123");
        assert!(matches!(
            texture_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(response_bytes(first).await, png);
        assert_eq!(response_bytes(second).await, png);
    }

    #[tokio::test]
    async fn skin_lookup_head_downloads_skin_and_returns_png_head() {
        let fixture = TestFixture::new("username-lookup-head", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let (profile_endpoint, session_endpoint, mut profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("classic".to_string()),
                cape_url: None,
            })
            .await;

        let response = fixture
            .lookup_head(
                "QueryUser",
                Some(32),
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect("lookup username head");
        let _ = profile_requests.recv().await.expect("profile request");
        let _ = profile_requests.recv().await.expect("session request");
        let texture_request = texture_requests.recv().await.expect("texture request");
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let bytes = response_bytes(response).await;
        let decoder = png::Decoder::new(Cursor::new(bytes.as_slice()));
        let reader = decoder.read_info().expect("head png should decode");
        let info = reader.info();

        assert_eq!(texture_request.path, "/texture/usernameTexture123");
        assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
        assert_eq!(content_type.as_deref(), Some("image/png"));
        assert_eq!(info.width, 32);
        assert_eq!(info.height, 32);
    }

    #[tokio::test]
    async fn skin_lookup_cape_downloads_session_cape_texture() {
        let fixture = TestFixture::new("username-lookup-cape", "ConfigUser");
        let cape_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(cape_png.clone()))
                .await;
        let (profile_endpoint, session_endpoint, mut profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("classic".to_string()),
                cape_url: Some(format!("{texture_prefix}usernameCape123")),
            })
            .await;

        let response = fixture
            .lookup_cape(
                "QueryUser",
                None,
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect("lookup username cape");
        let profile_request = profile_requests.recv().await.expect("profile request");
        let session_request = profile_requests.recv().await.expect("session request");
        let texture_request = texture_requests.recv().await.expect("cape texture request");
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let cache_control = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
        assert_eq!(
            session_request.path,
            "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(texture_request.path, "/texture/usernameCape123");
        assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
        assert_eq!(
            texture_request.user_agent.as_deref(),
            Some(CROOPOR_USER_AGENT)
        );
        assert_eq!(content_type.as_deref(), Some("image/png"));
        assert_eq!(
            cache_control.as_deref(),
            Some(PROFILE_CAPE_FILE_CACHE_CONTROL)
        );
        assert_eq!(response_bytes(response).await, cape_png);
    }

    #[tokio::test]
    async fn skin_lookup_cape_cache_hit_avoids_second_texture_request() {
        let fixture = TestFixture::new("username-lookup-cape-cache-hit", "ConfigUser");
        let cape_png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(cape_png.clone()))
                .await;
        let cape_url = format!("{texture_prefix}usernameCape123");
        let (profile_endpoint, session_endpoint, _profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("classic".to_string()),
                cape_url: Some(cape_url.clone()),
            })
            .await;

        let first = fixture
            .lookup_cape(
                "QueryUser",
                None,
                profile_endpoint.clone(),
                session_endpoint.clone(),
                texture_prefix.clone(),
            )
            .await
            .expect("first username cape lookup");
        let texture_request = texture_requests.recv().await.expect("cape texture request");
        let second = fixture
            .lookup_cape(
                "QueryUser",
                None,
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect("second username cape lookup");
        let cache_path =
            profile_cape_file_cache_path(&fixture.state.config().paths().config_dir, &cape_url);

        assert_eq!(texture_request.path, "/texture/usernameCape123");
        assert!(matches!(
            texture_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(response_bytes(first).await, cape_png);
        assert_eq!(response_bytes(second).await, cape_png);
        assert_eq!(
            tokio::fs::read(cache_path)
                .await
                .expect("read lookup cape cache"),
            cape_png
        );
    }

    #[tokio::test]
    async fn skin_lookup_cape_missing_returns_bounded_conflict() {
        let fixture = TestFixture::new("username-lookup-cape-missing", "ConfigUser");
        let texture_prefix = "http://127.0.0.1:9/texture/".to_string();
        let (profile_endpoint, session_endpoint, _profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("classic".to_string()),
                cape_url: None,
            })
            .await;

        let error = fixture
            .lookup_cape(
                "QueryUser",
                None,
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect_err("missing lookup cape should fail");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft player profile does not have a usable cape texture",
                "status": "minecraft_lookup_cape_missing",
            })
        );
    }

    #[tokio::test]
    async fn skin_username_save_downloads_normalizes_and_saves_session_skin() {
        let fixture = TestFixture::new("username-save-success", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let (profile_endpoint, session_endpoint, mut profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("slim".to_string()),
                cape_url: None,
            })
            .await;

        let saved = fixture
            .save_skin_from_username(
                SaveSkinFromUsernameRequest {
                    username: "  QueryUser  ".to_string(),
                    name: None,
                    variant: None,
                },
                profile_endpoint,
                session_endpoint,
                texture_prefix.clone(),
            )
            .await
            .expect("save username skin")
            .0;
        let profile_request = profile_requests.recv().await.expect("profile request");
        let session_request = profile_requests.recv().await.expect("session request");
        let texture_request = texture_requests.recv().await.expect("texture request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let file = fixture
            .saved_skin_file(&saved.texture_key)
            .await
            .expect("saved skin file");

        assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
        assert_eq!(
            session_request.path,
            "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(profile_request.accept.as_deref(), Some("application/json"));
        assert_eq!(session_request.accept.as_deref(), Some("application/json"));
        assert_eq!(
            profile_request.user_agent.as_deref(),
            Some(CROOPOR_USER_AGENT)
        );
        assert_eq!(
            session_request.user_agent.as_deref(),
            Some(CROOPOR_USER_AGENT)
        );
        assert_eq!(texture_request.path, "/texture/usernameTexture123");
        assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
        assert_eq!(saved.name, "ResolvedName skin");
        assert_eq!(normalized.variant_suggestion, "classic");
        assert_eq!(saved.variant, normalized.variant_suggestion);
        assert_eq!(saved.source, SAVED_SKIN_USERNAME_SOURCE);
        assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
        assert_eq!(saved.byte_size, normalized.png_bytes.len());
        assert_eq!(listed.skins, vec![saved.clone()]);
        assert_eq!(response_bytes(file).await, normalized.png_bytes);
    }

    #[tokio::test]
    async fn skin_username_save_reuses_lookup_skin_file_cache() {
        let fixture = TestFixture::new("username-save-reuses-lookup-cache", "ConfigUser");
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let (profile_endpoint, session_endpoint, mut profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("classic".to_string()),
                cape_url: None,
            })
            .await;

        let preview = fixture
            .lookup_file(
                "QueryUser",
                None,
                profile_endpoint.clone(),
                session_endpoint.clone(),
                texture_prefix.clone(),
            )
            .await
            .expect("lookup username skin preview");
        let profile_request = profile_requests.recv().await.expect("profile request");
        let session_request = profile_requests.recv().await.expect("session request");
        let texture_request = texture_requests.recv().await.expect("texture request");
        assert_eq!(response_bytes(preview).await, normalized.png_bytes);

        let saved = fixture
            .save_skin_from_username(
                SaveSkinFromUsernameRequest {
                    username: "QueryUser".to_string(),
                    name: None,
                    variant: None,
                },
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect("save username skin from lookup cache")
            .0;

        assert_eq!(profile_request.path, "/users/profiles/minecraft/QueryUser");
        assert_eq!(
            session_request.path,
            "/session/minecraft/profile/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(texture_request.path, "/texture/usernameTexture123");
        assert!(matches!(
            profile_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            texture_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(saved.name, "ResolvedName skin");
        assert_eq!(saved.source, SAVED_SKIN_USERNAME_SOURCE);
        assert_eq!(saved.texture_key, texture_key(&normalized.png_bytes));
        assert_eq!(saved.byte_size, normalized.png_bytes.len());
    }

    #[tokio::test]
    async fn skin_username_save_accepts_name_and_variant_override() {
        let fixture = TestFixture::new("username-save-overrides", "ConfigUser");
        let png = test_slim_skin_png();
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(png)).await;
        let (profile_endpoint, session_endpoint, mut profile_requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::Success {
                texture_url: format!("{texture_prefix}usernameTexture123"),
                model: Some("slim".to_string()),
                cape_url: None,
            })
            .await;

        let saved = fixture
            .save_skin_from_username(
                SaveSkinFromUsernameRequest {
                    username: "QueryUser".to_string(),
                    name: Some("  Username Copy  ".to_string()),
                    variant: Some("CLASSIC".to_string()),
                },
                profile_endpoint,
                session_endpoint,
                texture_prefix,
            )
            .await
            .expect("save username skin")
            .0;
        let _ = profile_requests.recv().await.expect("profile request");
        let _ = profile_requests.recv().await.expect("session request");
        let _ = texture_requests.recv().await.expect("texture request");

        assert_eq!(normalized.variant_suggestion, "slim");
        assert_eq!(saved.name, "Username Copy");
        assert_eq!(saved.variant, "classic");
    }

    #[tokio::test]
    async fn skin_username_save_invalid_username_returns_bad_request() {
        let fixture = TestFixture::new("username-save-invalid", "ConfigUser");

        let error = fixture
            .save_skin_from_username(
                SaveSkinFromUsernameRequest {
                    username: "bad name".to_string(),
                    name: None,
                    variant: None,
                },
                "http://127.0.0.1:9/users/profiles/minecraft".to_string(),
                "http://127.0.0.1:9/session/minecraft/profile".to_string(),
                "http://127.0.0.1:9/texture/".to_string(),
            )
            .await
            .expect_err("invalid username should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "Letters, numbers, and underscores only." })
        );
    }

    #[tokio::test]
    async fn skin_username_save_missing_player_returns_bounded_404() {
        let fixture = TestFixture::new("username-save-not-found", "ConfigUser");
        let (profile_endpoint, session_endpoint, mut requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::NotFound).await;

        let error = fixture
            .save_skin_from_username(
                SaveSkinFromUsernameRequest {
                    username: "MissingUser".to_string(),
                    name: None,
                    variant: None,
                },
                profile_endpoint,
                session_endpoint,
                "http://127.0.0.1:9/texture/".to_string(),
            )
            .await
            .expect_err("missing player should fail");
        let request = requests.recv().await.expect("profile request");

        assert_eq!(request.path, "/users/profiles/minecraft/MissingUser");
        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft player not found",
                "status": "minecraft_player_not_found",
            })
        );
    }

    #[tokio::test]
    async fn skin_username_save_profile_without_skin_returns_bounded_conflict() {
        let fixture = TestFixture::new("username-save-missing-skin", "ConfigUser");
        let (profile_endpoint, session_endpoint, mut requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::MissingSkin).await;

        let error = fixture
            .save_skin_from_username(
                SaveSkinFromUsernameRequest {
                    username: "NoSkinUser".to_string(),
                    name: None,
                    variant: None,
                },
                profile_endpoint,
                session_endpoint,
                "http://127.0.0.1:9/texture/".to_string(),
            )
            .await
            .expect_err("profile without skin should fail");
        let _ = requests.recv().await.expect("profile request");
        let _ = requests.recv().await.expect("session request");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft player profile does not have a usable skin texture",
                "status": "minecraft_username_skin_missing",
            })
        );
    }

    #[tokio::test]
    async fn skin_username_save_malformed_textures_property_returns_bounded_conflict() {
        let fixture = TestFixture::new("username-save-malformed-textures", "ConfigUser");
        let (profile_endpoint, session_endpoint, mut requests) =
            minecraft_username_test_server(MinecraftUsernameServerMode::MalformedTextures).await;

        let error = fixture
            .save_skin_from_username(
                SaveSkinFromUsernameRequest {
                    username: "BrokenUser".to_string(),
                    name: None,
                    variant: None,
                },
                profile_endpoint,
                session_endpoint,
                "http://127.0.0.1:9/texture/".to_string(),
            )
            .await
            .expect_err("malformed textures should fail");
        let _ = requests.recv().await.expect("profile request");
        let _ = requests.recv().await.expect("session request");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft player profile skin textures are malformed",
                "status": "minecraft_username_skin_malformed",
            })
        );
    }

    #[tokio::test]
    async fn skin_apply_missing_active_account_returns_bounded_error() {
        let fixture = TestFixture::new("apply-missing-active", "ConfigUser");
        let saved = fixture
            .save_skin("Apply Me", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;

        let error = fixture
            .apply_saved_skin_with_endpoint(&saved.texture_key, "http://127.0.0.1:9/skins")
            .await
            .expect_err("missing active account should fail");

        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft account login required",
                "status": "minecraft_account_required",
            })
        );
    }

    #[tokio::test]
    async fn skin_apply_missing_saved_skin_returns_404() {
        let fixture = TestFixture::new("apply-missing-saved", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;

        let error = fixture
            .apply_saved_skin_with_endpoint(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "http://127.0.0.1:9/skins",
            )
            .await
            .expect_err("missing skin should fail");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "saved skin not found" })
        );
    }

    #[tokio::test]
    async fn skin_apply_rejects_invalid_texture_key() {
        let fixture = TestFixture::new("apply-invalid-key", "ConfigUser");

        let error = fixture
            .apply_saved_skin_with_endpoint("../not-a-texture-key", "http://127.0.0.1:9/skins")
            .await
            .expect_err("invalid key should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "invalid texture key" })
        );
    }

    #[tokio::test]
    async fn skin_profile_reset_preserves_current_skin_and_clears_local_apply_state() {
        let fixture = TestFixture::new("profile-reset-success", "ConfigUser");
        let external_png = test_slim_skin_png();
        let external_normalized = normalize_skin_png(&external_png).expect("external normalized");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(external_png)).await;
        let external_texture_url = format!("{texture_prefix}externalTexture");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                vec![minecraft_skin(
                    "external-skin",
                    "ACTIVE",
                    &external_texture_url,
                    "SLIM",
                )],
                vec![minecraft_cape(
                    "external-cape",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/externalCape",
                )],
            ))
            .await;
        let applied = fixture
            .save_skin("Applied", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save applied skin")
            .0;
        let queued = fixture
            .save_skin(
                "Queued",
                None,
                test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 83),
            )
            .await
            .expect("save queued skin")
            .0;
        fixture
            .state
            .skins()
            .mark_applied(&applied.texture_key)
            .expect("mark applied skin");
        let _ = fixture
            .queue_saved_skin_apply(&queued.texture_key)
            .await
            .expect("queue pending apply");
        let (reset_endpoint, mut reset_requests) =
            skin_reset_route_test_server(SkinResetServerMode::Success).await;

        let response = fixture
            .reset_profile_skin_with_endpoints(&reset_endpoint, &texture_prefix)
            .await
            .expect("reset profile skin")
            .0;
        let texture_request = texture_requests.recv().await.expect("texture request");
        let reset_request = reset_requests.recv().await.expect("reset request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let account = fixture
            .state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
            .expect("active minecraft account")
            .account;
        let external_texture_key = texture_key(&external_normalized.png_bytes);
        let preserved = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == external_texture_key)
            .expect("external profile skin preserved");

        assert_eq!(response.status, "reset");
        assert!(response.profile_updated);
        assert_eq!(texture_request.path, "/texture/externalTexture");
        assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
        assert_eq!(
            texture_request.user_agent.as_deref(),
            Some(CROOPOR_USER_AGENT)
        );
        assert_eq!(reset_request.method, "DELETE");
        assert_eq!(reset_request.path, "/minecraft/profile/skins/active");
        assert_eq!(
            reset_request.authorization.as_deref(),
            Some("Bearer minecraft-access-token")
        );
        assert_eq!(reset_request.accept.as_deref(), Some("application/json"));
        assert_eq!(
            reset_request.user_agent.as_deref(),
            Some(CROOPOR_USER_AGENT)
        );
        assert_eq!(listed.pending_apply_texture_key, None);
        assert!(listed.skins.iter().all(|skin| skin.applied_at.is_none()));
        assert_eq!(preserved.name, "MinecraftName profile skin");
        assert_eq!(preserved.source, SAVED_SKIN_PROFILE_SOURCE);
        assert_eq!(preserved.variant, "slim");
        assert_eq!(preserved.cape_id.as_deref(), Some("external-cape"));
        assert_eq!(account.profile.name, "ResetProfileName");
        assert!(account.profile.skins.is_empty());
    }

    #[tokio::test]
    async fn skin_profile_reset_does_not_call_upstream_when_preservation_fails() {
        let fixture = TestFixture::new("profile-reset-preserve-fails", "ConfigUser");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Oversized).await;
        let external_texture_url = format!("{texture_prefix}externalTexture");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "external-skin",
                    "ACTIVE",
                    &external_texture_url,
                    "classic",
                )],
            ))
            .await;
        let (reset_endpoint, mut reset_requests) =
            skin_reset_route_test_server(SkinResetServerMode::Success).await;

        let error = fixture
            .reset_profile_skin_with_endpoints(&reset_endpoint, &texture_prefix)
            .await
            .expect_err("preservation failure should stop reset");
        let texture_request = texture_requests.recv().await.expect("texture request");

        assert_eq!(texture_request.path, "/texture/externalTexture");
        assert!(matches!(
            reset_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Current Minecraft profile skin is too large to preserve before changing it",
                "status": "minecraft_profile_skin_preserve_too_large",
            })
        );
    }

    #[tokio::test]
    async fn skin_profile_reset_upstream_429_maps_to_bounded_rate_limit() {
        let fixture = TestFixture::new("profile-reset-rate-limit", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let (reset_endpoint, mut reset_requests) =
            skin_reset_route_test_server(SkinResetServerMode::RateLimited).await;

        let error = fixture
            .reset_profile_skin_with_endpoints(&reset_endpoint, "http://127.0.0.1:9/texture/")
            .await
            .expect_err("rate limited reset should fail");
        let request = reset_requests.recv().await.expect("reset request");

        assert_eq!(request.method, "DELETE");
        assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft skin reset is rate limited. Try again later.",
                "status": "minecraft_skin_reset_rate_limited",
            })
        );
    }

    #[tokio::test]
    async fn skin_cape_reset_preserves_current_skin_and_clears_local_apply_state() {
        let fixture = TestFixture::new("cape-reset-success", "ConfigUser");
        let external_png = test_slim_skin_png();
        let external_normalized = normalize_skin_png(&external_png).expect("external normalized");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(external_png)).await;
        let external_texture_url = format!("{texture_prefix}externalTexture");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                vec![minecraft_skin(
                    "external-skin",
                    "ACTIVE",
                    &external_texture_url,
                    "SLIM",
                )],
                vec![minecraft_cape(
                    "external-cape",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/externalCape",
                )],
            ))
            .await;
        let applied = fixture
            .save_skin("Applied", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save applied skin")
            .0;
        let queued = fixture
            .save_skin(
                "Queued",
                None,
                test_skin_png_with_seed(SKIN_WIDTH, SKIN_HEIGHT, 97),
            )
            .await
            .expect("save queued skin")
            .0;
        fixture
            .state
            .skins()
            .mark_applied(&applied.texture_key)
            .expect("mark applied skin");
        let _ = fixture
            .queue_saved_skin_apply(&queued.texture_key)
            .await
            .expect("queue pending apply");
        let (cape_endpoint, mut cape_requests) =
            cape_sync_route_test_server(CapeSyncServerMode::Success).await;

        let response = fixture
            .reset_profile_cape_with_endpoints(&cape_endpoint, &texture_prefix)
            .await
            .expect("reset profile cape")
            .0;
        let texture_request = texture_requests.recv().await.expect("texture request");
        let cape_request = cape_requests.recv().await.expect("cape reset request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let account = fixture
            .state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
            .expect("active minecraft account")
            .account;
        let external_texture_key = texture_key(&external_normalized.png_bytes);
        let preserved = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == external_texture_key)
            .expect("external profile skin preserved");

        assert_eq!(response.status, "reset");
        assert!(response.profile_updated);
        assert_eq!(texture_request.path, "/texture/externalTexture");
        assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
        assert_eq!(
            texture_request.user_agent.as_deref(),
            Some(CROOPOR_USER_AGENT)
        );
        assert_eq!(cape_request.method, "DELETE");
        assert_eq!(cape_request.path, "/minecraft/profile/capes/active");
        assert_eq!(
            cape_request.authorization.as_deref(),
            Some("Bearer minecraft-access-token")
        );
        assert_eq!(cape_request.accept.as_deref(), Some("application/json"));
        assert_eq!(cape_request.user_agent.as_deref(), Some(CROOPOR_USER_AGENT));
        assert_eq!(listed.pending_apply_texture_key, None);
        assert!(listed.skins.iter().all(|skin| skin.applied_at.is_none()));
        assert_eq!(preserved.name, "MinecraftName profile skin");
        assert_eq!(preserved.source, SAVED_SKIN_PROFILE_SOURCE);
        assert_eq!(preserved.variant, "slim");
        assert_eq!(preserved.cape_id.as_deref(), Some("external-cape"));
        assert_eq!(account.profile.capes[0].state, "INACTIVE");
    }

    #[tokio::test]
    async fn skin_cape_reset_does_not_call_upstream_when_preservation_fails() {
        let fixture = TestFixture::new("cape-reset-preserve-fails", "ConfigUser");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Oversized).await;
        let external_texture_url = format!("{texture_prefix}externalTexture");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                vec![minecraft_skin(
                    "external-skin",
                    "ACTIVE",
                    &external_texture_url,
                    "classic",
                )],
                vec![minecraft_cape(
                    "external-cape",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/externalCape",
                )],
            ))
            .await;
        let (cape_endpoint, mut cape_requests) =
            cape_sync_route_test_server(CapeSyncServerMode::Success).await;

        let error = fixture
            .reset_profile_cape_with_endpoints(&cape_endpoint, &texture_prefix)
            .await
            .expect_err("preservation failure should stop cape reset");
        let texture_request = texture_requests.recv().await.expect("texture request");

        assert_eq!(texture_request.path, "/texture/externalTexture");
        assert!(matches!(
            cape_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Current Minecraft profile skin is too large to preserve before changing it",
                "status": "minecraft_profile_skin_preserve_too_large",
            })
        );
    }

    #[tokio::test]
    async fn skin_cape_reset_upstream_429_maps_to_bounded_rate_limit() {
        let fixture = TestFixture::new("cape-reset-rate-limit", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                Vec::new(),
                vec![minecraft_cape(
                    "external-cape",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/externalCape",
                )],
            ))
            .await;
        let (cape_endpoint, mut cape_requests) =
            cape_sync_route_test_server(CapeSyncServerMode::RateLimited).await;

        let error = fixture
            .reset_profile_cape_with_endpoints(&cape_endpoint, "http://127.0.0.1:9/texture/")
            .await
            .expect_err("rate limited cape reset should fail");
        let request = cape_requests.recv().await.expect("cape reset request");

        assert_eq!(request.method, "DELETE");
        assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft cape change is rate limited. Try again later.",
                "status": "minecraft_cape_rate_limited",
            })
        );
    }

    #[tokio::test]
    async fn skin_apply_upstream_success_uploads_saved_skin_and_updates_profile() {
        let fixture = TestFixture::new("apply-upstream-success", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("OldMinecraftName", Vec::new()))
            .await;
        let png = test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT);
        let saved = fixture
            .save_skin("Slim Skin", Some("slim".to_string()), png.clone())
            .await
            .expect("save skin")
            .0;
        let normalized = normalize_skin_png(&png).expect("normalized skin");
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;

        let response = fixture
            .apply_saved_skin_with_endpoint(&saved.texture_key, &endpoint)
            .await
            .expect("apply skin")
            .0;
        let request = requests.recv().await.expect("skin upload request");
        let account = fixture
            .state
            .auth_logins()
            .active_minecraft_account()
            .await
            .expect("active minecraft account");

        assert_eq!(response.status, "applied");
        assert_eq!(response.texture_key, saved.texture_key);
        assert!(response.profile_updated);
        assert_eq!(request.path, "/minecraft/profile/skins");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer minecraft-access-token")
        );
        assert_eq!(request.accept.as_deref(), Some("application/json"));
        assert_eq!(request.user_agent.as_deref(), Some(CROOPOR_USER_AGENT));
        assert!(
            request
                .content_type
                .as_deref()
                .is_some_and(|value| value.starts_with("multipart/form-data; boundary="))
        );
        assert!(body_contains(&request.body, b"name=\"variant\""));
        assert!(body_contains(&request.body, b"slim"));
        assert!(body_contains(
            &request.body,
            b"name=\"file\"; filename=\"skin.png\""
        ));
        assert!(body_contains(&request.body, &normalized.png_bytes));
        assert_eq!(account.profile.name, "UpdatedProfileName");
        assert_eq!(account.profile.skins[0].variant, "SLIM");
    }

    #[tokio::test]
    async fn skin_apply_success_marks_saved_skin_applied_and_clears_prior_marker() {
        let fixture = TestFixture::new("apply-marks-active", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let prior = fixture
            .save_skin("Prior", None, test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT))
            .await
            .expect("save prior skin")
            .0;
        let next = fixture
            .save_skin(
                "Next",
                Some("slim".to_string()),
                test_skin_png(SKIN_WIDTH, SKIN_HEIGHT),
            )
            .await
            .expect("save next skin")
            .0;
        fixture
            .state
            .skins()
            .mark_applied(&prior.texture_key)
            .expect("mark prior skin applied");
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;

        let _ = fixture
            .apply_saved_skin_with_endpoint(&next.texture_key, &endpoint)
            .await
            .expect("apply next skin");
        let _ = requests.recv().await.expect("skin upload request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let prior_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == prior.texture_key)
            .expect("prior skin listed");
        let next_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == next.texture_key)
            .expect("next skin listed");

        assert_eq!(prior_after.applied_at, None);
        assert!(next_after.applied_at.is_some());
    }

    #[tokio::test]
    async fn skin_apply_defer_queues_until_flush() {
        let fixture = TestFixture::new("apply-defer-flush", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin("Queued", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;

        let queued = fixture
            .queue_saved_skin_apply(&saved.texture_key)
            .await
            .expect("queue skin apply")
            .0;
        let listed_before_flush = fixture.saved_skins().await.expect("saved skins").0;
        let saved_before_flush = listed_before_flush
            .skins
            .iter()
            .find(|skin| skin.texture_key == saved.texture_key)
            .expect("saved skin listed");

        assert_eq!(queued.status, "queued");
        assert_eq!(queued.texture_key, saved.texture_key);
        assert!(!queued.profile_updated);
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(
            listed_before_flush.pending_apply_texture_key.as_deref(),
            Some(saved.texture_key.as_str())
        );
        assert_eq!(saved_before_flush.applied_at, None);

        let flushed = fixture
            .flush_saved_skin_applies_with_endpoints(
                &endpoint,
                "http://127.0.0.1:9/capes",
                "http://127.0.0.1:9/texture/",
            )
            .await
            .expect("flush pending skin apply")
            .0;
        let _ = requests.recv().await.expect("skin upload request");
        let listed_after_flush = fixture.saved_skins().await.expect("saved skins").0;
        let saved_after_flush = listed_after_flush
            .skins
            .iter()
            .find(|skin| skin.texture_key == saved.texture_key)
            .expect("saved skin listed");

        assert_eq!(flushed.status, "flushed");
        assert_eq!(flushed.applied, 1);
        assert_eq!(listed_after_flush.pending_apply_texture_key, None);
        assert!(saved_after_flush.applied_at.is_some());
    }

    #[tokio::test]
    async fn skin_apply_shutdown_flushes_active_pending_change() {
        let fixture = TestFixture::new("apply-shutdown-flush", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin(
                "Shutdown Queued",
                None,
                test_skin_png(SKIN_WIDTH, SKIN_HEIGHT),
            )
            .await
            .expect("save skin")
            .0;
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;

        let _ = fixture
            .queue_saved_skin_apply(&saved.texture_key)
            .await
            .expect("queue skin apply");
        let flushed = fixture
            .flush_saved_skin_applies_with_endpoints(
                &endpoint,
                "http://127.0.0.1:9/capes",
                "http://127.0.0.1:9/texture/",
            )
            .await
            .expect("shutdown flush pending skin apply")
            .0;
        let _ = requests.recv().await.expect("skin upload request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let saved_after_flush = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == saved.texture_key)
            .expect("saved skin listed");

        assert_eq!(flushed.status, "flushed");
        assert_eq!(flushed.applied, 1);
        assert_eq!(listed.pending_apply_texture_key, None);
        assert!(saved_after_flush.applied_at.is_some());
    }

    #[tokio::test]
    async fn skin_apply_defer_clear_removes_pending_for_active_account() {
        let fixture = TestFixture::new("apply-defer-clear", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin("Queued", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;

        let _ = fixture
            .queue_saved_skin_apply(&saved.texture_key)
            .await
            .expect("queue skin apply");
        let listed_before_clear = fixture.saved_skins().await.expect("saved skins").0;
        let cleared = fixture
            .clear_pending_saved_skin_apply()
            .await
            .expect("clear pending skin apply")
            .0;
        let listed_after_clear = fixture.saved_skins().await.expect("saved skins").0;

        assert_eq!(
            listed_before_clear.pending_apply_texture_key.as_deref(),
            Some(saved.texture_key.as_str())
        );
        assert_eq!(cleared.status, "cleared");
        assert!(cleared.cleared);
        assert_eq!(listed_after_clear.pending_apply_texture_key, None);
    }

    #[tokio::test]
    async fn skin_apply_clear_for_login_id_removes_pending_apply() {
        let fixture = TestFixture::new("apply-clear-login-id", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin("Queued", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;

        let _ = fixture
            .queue_saved_skin_apply(&saved.texture_key)
            .await
            .expect("queue skin apply");
        let login_id = fixture
            .state
            .auth_logins()
            .active_minecraft_account()
            .await
            .expect("active minecraft account")
            .login_id;

        assert!(clear_pending_saved_skin_apply_for_login_id(&login_id).await);
        assert!(!clear_pending_saved_skin_apply_for_login_id(&login_id).await);
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        assert_eq!(listed.pending_apply_texture_key, None);
    }

    #[tokio::test]
    async fn skin_apply_defer_keeps_latest_for_same_account() {
        let fixture = TestFixture::new("apply-defer-latest-wins", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let prior_png = test_skin_png(SKIN_WIDTH, SKIN_HEIGHT);
        let next_png = test_slim_skin_png();
        let next_normalized = normalize_skin_png(&next_png).expect("next normalized");
        let prior = fixture
            .save_skin("Prior", None, prior_png)
            .await
            .expect("save prior")
            .0;
        let next = fixture
            .save_skin("Next", Some("slim".to_string()), next_png)
            .await
            .expect("save next")
            .0;
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;

        let _ = fixture
            .queue_saved_skin_apply(&prior.texture_key)
            .await
            .expect("queue prior");
        let _ = fixture
            .queue_saved_skin_apply(&next.texture_key)
            .await
            .expect("queue next");
        let flushed = fixture
            .flush_saved_skin_applies_with_endpoints(
                &endpoint,
                "http://127.0.0.1:9/capes",
                "http://127.0.0.1:9/texture/",
            )
            .await
            .expect("flush pending skin apply")
            .0;
        let request = requests.recv().await.expect("skin upload request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let prior_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == prior.texture_key)
            .expect("prior skin listed");
        let next_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == next.texture_key)
            .expect("next skin listed");

        assert_eq!(flushed.applied, 1);
        assert!(body_contains(&request.body, &next_normalized.png_bytes));
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(prior_after.applied_at, None);
        assert!(next_after.applied_at.is_some());
    }

    #[tokio::test]
    async fn skin_apply_defer_flushes_against_queued_login_after_account_switch() {
        let fixture = TestFixture::new("apply-defer-original-login", "ConfigUser");
        let first_account = fixture
            .add_minecraft_account_with_tokens(
                test_profile("FirstPlayer", Vec::new()),
                "first-msa-access-token",
                "first-minecraft-access-token",
            )
            .await;
        let saved = fixture
            .save_skin("Queued", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;

        let _ = fixture
            .queue_saved_skin_apply(&saved.texture_key)
            .await
            .expect("queue skin apply");
        let second_account = fixture
            .add_minecraft_account_with_tokens(
                test_profile("SecondPlayer", Vec::new()),
                "second-msa-access-token",
                "second-minecraft-access-token",
            )
            .await;

        assert_ne!(first_account.login_id, second_account.login_id);
        assert_eq!(
            fixture
                .state
                .auth_logins()
                .active_current_minecraft_account_state()
                .await
                .expect("second account active")
                .account
                .login_id,
            second_account.login_id
        );

        let applied = flush_pending_saved_skin_applies_with_clients(
            &fixture.state,
            PendingSkinApplyFilter::Generation {
                login_id: first_account.login_id.clone(),
                generation: 1,
            },
            MinecraftSkinUploadClient::with_endpoint(endpoint),
            MinecraftCapeSyncClient::with_endpoint("http://127.0.0.1:9/capes".to_string()),
            MinecraftSkinTextureClient::with_allowed_prefix(
                "http://127.0.0.1:9/texture/".to_string(),
            ),
        )
        .await
        .expect("flush queued skin apply");
        let request = requests.recv().await.expect("skin upload request");

        assert_eq!(applied, 1);
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer first-minecraft-access-token")
        );
        assert_eq!(
            fixture
                .state
                .auth_logins()
                .active_current_minecraft_account_state()
                .await
                .expect("second account remains active")
                .account
                .login_id,
            second_account.login_id
        );
    }

    #[tokio::test]
    async fn skin_apply_flush_requeues_failed_pending_change() {
        let fixture = TestFixture::new("apply-defer-requeues-failure", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin("Retry", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;
        let (rejected_endpoint, mut rejected_requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Rejected).await;
        let (success_endpoint, mut success_requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;

        let _ = fixture
            .queue_saved_skin_apply(&saved.texture_key)
            .await
            .expect("queue skin apply");
        let error = fixture
            .flush_saved_skin_applies_with_endpoints(
                &rejected_endpoint,
                "http://127.0.0.1:9/capes",
                "http://127.0.0.1:9/texture/",
            )
            .await
            .expect_err("rejected flush should fail");
        let _ = rejected_requests
            .recv()
            .await
            .expect("rejected skin upload request");
        let listed_after_error = fixture.saved_skins().await.expect("saved skins").0;
        let saved_after_error = listed_after_error
            .skins
            .iter()
            .find(|skin| skin.texture_key == saved.texture_key)
            .expect("saved skin listed");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(saved_after_error.applied_at, None);

        let flushed = fixture
            .flush_saved_skin_applies_with_endpoints(
                &success_endpoint,
                "http://127.0.0.1:9/capes",
                "http://127.0.0.1:9/texture/",
            )
            .await
            .expect("retry pending skin apply")
            .0;
        let _ = success_requests
            .recv()
            .await
            .expect("success skin upload request");
        let listed_after_retry = fixture.saved_skins().await.expect("saved skins").0;
        let saved_after_retry = listed_after_retry
            .skins
            .iter()
            .find(|skin| skin.texture_key == saved.texture_key)
            .expect("saved skin listed");

        assert_eq!(flushed.applied, 1);
        assert!(saved_after_retry.applied_at.is_some());
    }

    #[tokio::test]
    async fn skin_apply_success_syncs_selected_cape() {
        let fixture = TestFixture::new("apply-syncs-cape", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                Vec::new(),
                vec![minecraft_cape(
                    "cape-id",
                    "INACTIVE",
                    "https://textures.minecraft.net/texture/capeTexture",
                )],
            ))
            .await;
        let saved = fixture
            .save_skin("Cape Skin", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;
        let saved = fixture
            .update_saved_skin(
                &saved.texture_key,
                serde_json::json!({ "cape_id": "cape-id" }),
            )
            .await
            .expect("select cape")
            .0;
        let (skin_endpoint, mut skin_requests) =
            skin_apply_route_test_server(SkinApplyServerMode::SuccessWithCapeAvailable).await;
        let (cape_endpoint, mut cape_requests) =
            cape_sync_route_test_server(CapeSyncServerMode::Success).await;

        let response = fixture
            .apply_saved_skin_with_endpoints(&saved.texture_key, &skin_endpoint, &cape_endpoint)
            .await
            .expect("apply saved skin with cape")
            .0;
        let _ = skin_requests.recv().await.expect("skin upload request");
        let cape_request = cape_requests.recv().await.expect("cape sync request");
        let account = fixture
            .state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
            .expect("active minecraft account")
            .account;
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let saved_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == saved.texture_key)
            .expect("saved skin listed");

        assert!(response.profile_updated);
        assert!(saved_after.applied_at.is_some());
        assert_eq!(cape_request.method, "PUT");
        assert_eq!(cape_request.path, "/minecraft/profile/capes/active");
        assert_eq!(
            cape_request.authorization.as_deref(),
            Some("Bearer minecraft-access-token")
        );
        assert_eq!(cape_request.accept.as_deref(), Some("application/json"));
        assert_eq!(cape_request.user_agent.as_deref(), Some(CROOPOR_USER_AGENT));
        assert_eq!(
            cape_request.content_type.as_deref(),
            Some("application/json")
        );
        assert!(body_contains(&cape_request.body, br#""capeId":"cape-id""#));
        assert_eq!(account.profile.capes[0].state, "ACTIVE");
    }

    #[tokio::test]
    async fn skin_apply_preserves_external_profile_skin_before_upload() {
        let fixture = TestFixture::new("apply-preserves-external", "ConfigUser");
        let external_png = test_slim_skin_png();
        let external_normalized = normalize_skin_png(&external_png).expect("external normalized");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Png(external_png)).await;
        let external_texture_url = format!("{texture_prefix}externalTexture");
        fixture
            .add_minecraft_account(test_profile_with_capes(
                "MinecraftName",
                vec![minecraft_skin(
                    "external-skin",
                    "ACTIVE",
                    &external_texture_url,
                    "SLIM",
                )],
                vec![minecraft_cape(
                    "external-cape",
                    "ACTIVE",
                    "https://textures.minecraft.net/texture/externalCape",
                )],
            ))
            .await;
        let target = fixture
            .save_skin("Target", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save target skin")
            .0;
        let (skin_endpoint, mut skin_requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;
        let (cape_endpoint, mut cape_requests) =
            cape_sync_route_test_server(CapeSyncServerMode::Success).await;

        let response = fixture
            .apply_saved_skin_with_all_endpoints(
                &target.texture_key,
                &skin_endpoint,
                &cape_endpoint,
                &texture_prefix,
            )
            .await
            .expect("apply skin")
            .0;
        let texture_request = texture_requests.recv().await.expect("texture request");
        let _ = skin_requests.recv().await.expect("skin upload request");
        let cape_request = cape_requests.recv().await.expect("cape sync request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let external_texture_key = texture_key(&external_normalized.png_bytes);
        let preserved = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == external_texture_key)
            .expect("external profile skin preserved");

        assert_eq!(response.status, "applied");
        assert_eq!(texture_request.path, "/texture/externalTexture");
        assert_eq!(texture_request.accept.as_deref(), Some("image/png"));
        assert_eq!(
            texture_request.user_agent.as_deref(),
            Some(CROOPOR_USER_AGENT)
        );
        assert_eq!(cape_request.method, "DELETE");
        assert_eq!(preserved.name, "MinecraftName profile skin");
        assert_eq!(preserved.source, SAVED_SKIN_PROFILE_SOURCE);
        assert_eq!(preserved.variant, "slim");
        assert_eq!(preserved.cape_id.as_deref(), Some("external-cape"));
        assert_eq!(preserved.applied_at, None);
    }

    #[tokio::test]
    async fn skin_apply_does_not_upload_when_external_preservation_fails() {
        let fixture = TestFixture::new("apply-preserve-fails-before-upload", "ConfigUser");
        let (texture_prefix, mut texture_requests) =
            skin_profile_texture_test_server(SkinProfileTextureServerMode::Oversized).await;
        let external_texture_url = format!("{texture_prefix}externalTexture");
        fixture
            .add_minecraft_account(test_profile(
                "MinecraftName",
                vec![minecraft_skin(
                    "external-skin",
                    "ACTIVE",
                    &external_texture_url,
                    "CLASSIC",
                )],
            ))
            .await;
        let target = fixture
            .save_skin("Target", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save target skin")
            .0;
        let (skin_endpoint, mut skin_requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Success).await;

        let error = fixture
            .apply_saved_skin_with_all_endpoints(
                &target.texture_key,
                &skin_endpoint,
                "http://127.0.0.1:9/capes",
                &texture_prefix,
            )
            .await
            .expect_err("preservation failure should stop apply");
        let texture_request = texture_requests.recv().await.expect("texture request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let target_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == target.texture_key)
            .expect("target skin listed");

        assert_eq!(texture_request.path, "/texture/externalTexture");
        assert!(matches!(
            skin_requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(listed.skins.len(), 1);
        assert_eq!(target_after.applied_at, None);
        assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Current Minecraft profile skin is too large to preserve before changing it",
                "status": "minecraft_profile_skin_preserve_too_large",
            })
        );
    }

    #[tokio::test]
    async fn skin_apply_upstream_failure_does_not_mark_saved_skin_applied() {
        let fixture = TestFixture::new("apply-failure-keeps-marker", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let prior = fixture
            .save_skin("Prior", None, test_skin_png(SKIN_WIDTH, LEGACY_SKIN_HEIGHT))
            .await
            .expect("save prior skin")
            .0;
        let rejected = fixture
            .save_skin("Rejected", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save rejected skin")
            .0;
        let prior_applied_at = fixture
            .state
            .skins()
            .mark_applied(&prior.texture_key)
            .expect("mark prior skin applied")
            .expect("prior skin exists");
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Rejected).await;

        let _ = fixture
            .apply_saved_skin_with_endpoint(&rejected.texture_key, &endpoint)
            .await
            .expect_err("rejected upload should fail");
        let _ = requests.recv().await.expect("skin upload request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let prior_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == prior.texture_key)
            .expect("prior skin listed");
        let rejected_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == rejected.texture_key)
            .expect("rejected skin listed");

        assert_eq!(
            prior_after.applied_at.as_deref(),
            Some(prior_applied_at.as_str())
        );
        assert_eq!(rejected_after.applied_at, None);
    }

    #[tokio::test]
    async fn skin_apply_oversized_success_response_is_bounded() {
        let fixture = TestFixture::new("apply-oversized-success-response", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin(
                "Oversized Response",
                None,
                test_skin_png(SKIN_WIDTH, SKIN_HEIGHT),
            )
            .await
            .expect("save skin")
            .0;
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::OversizedSuccess).await;

        let error = fixture
            .apply_saved_skin_with_endpoint(&saved.texture_key, &endpoint)
            .await
            .expect_err("oversized upload response should fail");
        let _ = requests.recv().await.expect("skin upload request");
        let listed = fixture.saved_skins().await.expect("saved skins").0;
        let saved_after = listed
            .skins
            .iter()
            .find(|skin| skin.texture_key == saved.texture_key)
            .expect("saved skin listed");

        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft skin upload response is too large",
                "status": "minecraft_skin_response_too_large",
            })
        );
        assert_eq!(saved_after.applied_at, None);
    }

    #[tokio::test]
    async fn skin_apply_upstream_429_maps_to_bounded_rate_limit() {
        let fixture = TestFixture::new("apply-upstream-rate-limit", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin("Rate Limited", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::RateLimited).await;

        let error = fixture
            .apply_saved_skin_with_endpoint(&saved.texture_key, &endpoint)
            .await
            .expect_err("rate limited upload should fail");
        let _ = requests.recv().await.expect("skin upload request");

        assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft skin upload is rate limited. Try again later.",
                "status": "minecraft_skin_rate_limited",
            })
        );
    }

    #[tokio::test]
    async fn skin_apply_upstream_rejected_error_is_bounded() {
        let fixture = TestFixture::new("apply-upstream-rejected", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin("Rejected", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;
        let (endpoint, mut requests) =
            skin_apply_route_test_server(SkinApplyServerMode::Rejected).await;

        let error = fixture
            .apply_saved_skin_with_endpoint(&saved.texture_key, &endpoint)
            .await
            .expect_err("rejected upload should fail");
        let _ = requests.recv().await.expect("skin upload request");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft rejected the saved skin",
                "status": "minecraft_skin_rejected",
            })
        );
    }

    #[tokio::test]
    async fn skin_apply_upstream_unavailable_error_is_bounded() {
        let fixture = TestFixture::new("apply-upstream-unavailable", "ConfigUser");
        fixture
            .add_minecraft_account(test_profile("MinecraftName", Vec::new()))
            .await;
        let saved = fixture
            .save_skin("Unavailable", None, test_skin_png(SKIN_WIDTH, SKIN_HEIGHT))
            .await
            .expect("save skin")
            .0;

        let error = fixture
            .apply_saved_skin_with_endpoint(&saved.texture_key, "http://127.0.0.1:9/skins")
            .await
            .expect_err("unavailable upload should fail");

        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "Minecraft skin upload is unavailable. Try again later.",
                "status": "minecraft_skin_unavailable",
            })
        );
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str, username: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            config
                .replace_in_memory(AppConfig {
                    username: username.to_string(),
                    ..AppConfig::default()
                })
                .expect("set username");
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });

            Self { state, root }
        }

        async fn profile(
            &self,
            username: Option<String>,
            size: Option<u32>,
        ) -> Result<Json<SkinProfileResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_profile(
                State(self.state.clone()),
                Query(SkinQuery { username, size }),
            )
            .await
        }

        async fn head(
            &self,
            username: Option<String>,
            size: Option<u32>,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_head(
                State(self.state.clone()),
                Query(SkinQuery { username, size }),
            )
            .await
        }

        async fn lookup(
            &self,
            username: &str,
            size: Option<u32>,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Json<SkinLookupResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_lookup_with_client(
                Query(SkinLookupQuery {
                    username: username.to_string(),
                    size,
                }),
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                allowed_prefix,
            )
            .await
        }

        async fn lookup_head(
            &self,
            username: &str,
            size: Option<u32>,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_lookup_head_with_clients(
                State(self.state.clone()),
                Query(SkinLookupQuery {
                    username: username.to_string(),
                    size,
                }),
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn lookup_file(
            &self,
            username: &str,
            size: Option<u32>,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_lookup_file_with_clients(
                State(self.state.clone()),
                Query(SkinLookupQuery {
                    username: username.to_string(),
                    size,
                }),
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn lookup_cape(
            &self,
            username: &str,
            size: Option<u32>,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_lookup_cape_with_clients(
                State(self.state.clone()),
                Query(SkinLookupQuery {
                    username: username.to_string(),
                    size,
                }),
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn profile_file(
            &self,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            self.profile_file_with_texture(allowed_prefix, None).await
        }

        async fn profile_file_with_texture(
            &self,
            allowed_prefix: String,
            texture: Option<String>,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_profile_file_with_client(
                State(self.state.clone()),
                Query(SkinProfileFileQuery { texture }),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn cape_file(
            &self,
            cape_id: &str,
            allowed_prefix: String,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_cape_file_with_client(
                State(self.state.clone()),
                Query(SkinCapeFileQuery {
                    id: cape_id.to_string(),
                }),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn saved_skins(
            &self,
        ) -> Result<Json<SavedSkinsResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_saved_skins(State(self.state.clone())).await
        }

        async fn save_skin(
            &self,
            name: &str,
            variant: Option<String>,
            body: Vec<u8>,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            handle_save_skin(
                State(self.state.clone()),
                Query(SaveSkinQuery {
                    name: Some(name.to_string()),
                    variant,
                    cape_id: None,
                    source: None,
                }),
                Body::from(body),
            )
            .await
        }

        async fn delete_saved_skin(
            &self,
            texture_key: &str,
        ) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
            handle_delete_skin(State(self.state.clone()), Path(texture_key.to_string())).await
        }

        async fn update_saved_skin(
            &self,
            texture_key: &str,
            payload: serde_json::Value,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            let payload = serde_json::from_value::<UpdateSavedSkinRequest>(payload)
                .expect("valid update payload");
            handle_update_saved_skin(
                State(self.state.clone()),
                Path(texture_key.to_string()),
                Json(payload),
            )
            .await
        }

        async fn replace_saved_skin_texture(
            &self,
            texture_key: &str,
            query: ReplaceSavedSkinTextureQuery,
            body: Vec<u8>,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            handle_replace_saved_skin_texture(
                State(self.state.clone()),
                Path(texture_key.to_string()),
                Query(query),
                Body::from(body),
            )
            .await
        }

        async fn saved_skin_file(
            &self,
            texture_key: &str,
        ) -> Result<Response<Body>, (StatusCode, Json<serde_json::Value>)> {
            handle_saved_skin_file(State(self.state.clone()), Path(texture_key.to_string())).await
        }

        async fn save_skin_from_profile(
            &self,
            payload: SaveSkinFromProfileRequest,
            allowed_prefix: String,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            handle_save_skin_from_profile_with_client(
                State(self.state.clone()),
                payload,
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn save_skin_from_username(
            &self,
            payload: SaveSkinFromUsernameRequest,
            profile_endpoint: String,
            session_profile_endpoint: String,
            allowed_prefix: String,
        ) -> Result<Json<SavedSkinRecord>, (StatusCode, Json<serde_json::Value>)> {
            handle_save_skin_from_username_with_clients(
                State(self.state.clone()),
                payload,
                MinecraftSkinUsernameClient::with_endpoints(
                    profile_endpoint,
                    session_profile_endpoint,
                ),
                MinecraftSkinTextureClient::with_allowed_prefix(allowed_prefix),
            )
            .await
        }

        async fn apply_saved_skin_with_endpoint(
            &self,
            texture_key: &str,
            endpoint: &str,
        ) -> Result<Json<SkinApplyResponse>, (StatusCode, Json<serde_json::Value>)> {
            self.apply_saved_skin_with_endpoints(texture_key, endpoint, "http://127.0.0.1:9/capes")
                .await
        }

        async fn apply_saved_skin_with_endpoints(
            &self,
            texture_key: &str,
            skin_endpoint: &str,
            cape_endpoint: &str,
        ) -> Result<Json<SkinApplyResponse>, (StatusCode, Json<serde_json::Value>)> {
            self.apply_saved_skin_with_all_endpoints(
                texture_key,
                skin_endpoint,
                cape_endpoint,
                "http://127.0.0.1:9/texture/",
            )
            .await
        }

        async fn apply_saved_skin_with_all_endpoints(
            &self,
            texture_key: &str,
            skin_endpoint: &str,
            cape_endpoint: &str,
            texture_prefix: &str,
        ) -> Result<Json<SkinApplyResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_apply_saved_skin_with_client(
                State(self.state.clone()),
                Path(texture_key.to_string()),
                MinecraftSkinUploadClient::with_endpoint(skin_endpoint.to_string()),
                MinecraftCapeSyncClient::with_endpoint(cape_endpoint.to_string()),
                MinecraftSkinTextureClient::with_allowed_prefix(texture_prefix.to_string()),
            )
            .await
        }

        async fn reset_profile_skin_with_endpoints(
            &self,
            reset_endpoint: &str,
            texture_prefix: &str,
        ) -> Result<Json<SkinProfileResetResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_profile_reset_with_clients(
                State(self.state.clone()),
                MinecraftSkinResetClient::with_endpoint(reset_endpoint.to_string()),
                MinecraftSkinTextureClient::with_allowed_prefix(texture_prefix.to_string()),
            )
            .await
        }

        async fn reset_profile_cape_with_endpoints(
            &self,
            cape_endpoint: &str,
            texture_prefix: &str,
        ) -> Result<Json<SkinCapeResetResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_skin_cape_reset_with_clients(
                State(self.state.clone()),
                MinecraftCapeSyncClient::with_endpoint(cape_endpoint.to_string()),
                MinecraftSkinTextureClient::with_allowed_prefix(texture_prefix.to_string()),
            )
            .await
        }

        async fn queue_saved_skin_apply(
            &self,
            texture_key: &str,
        ) -> Result<Json<SkinApplyResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_apply_saved_skin(
                State(self.state.clone()),
                Path(texture_key.to_string()),
                Query(ApplySavedSkinQuery { defer: Some(true) }),
            )
            .await
        }

        async fn clear_pending_saved_skin_apply(
            &self,
        ) -> Result<Json<SkinPendingClearResponse>, (StatusCode, Json<serde_json::Value>)> {
            handle_clear_pending_saved_skin_apply(State(self.state.clone())).await
        }

        async fn flush_saved_skin_applies_with_endpoints(
            &self,
            skin_endpoint: &str,
            cape_endpoint: &str,
            texture_prefix: &str,
        ) -> Result<Json<SkinFlushResponse>, (StatusCode, Json<serde_json::Value>)> {
            let login_id = self
                .state
                .auth_logins()
                .active_current_minecraft_account_state()
                .await
                .expect("active minecraft account")
                .account
                .login_id;
            let applied = flush_pending_saved_skin_applies_with_clients(
                &self.state,
                PendingSkinApplyFilter::Login(login_id),
                MinecraftSkinUploadClient::with_endpoint(skin_endpoint.to_string()),
                MinecraftCapeSyncClient::with_endpoint(cape_endpoint.to_string()),
                MinecraftSkinTextureClient::with_allowed_prefix(texture_prefix.to_string()),
            )
            .await?;

            Ok(Json(SkinFlushResponse {
                status: "flushed",
                applied,
            }))
        }

        async fn add_minecraft_account(&self, profile: AuthLoginMinecraftProfile) {
            self.add_minecraft_account_with_expiry(profile, 86_400)
                .await;
        }

        async fn add_minecraft_account_with_ownership(
            &self,
            profile: AuthLoginMinecraftProfile,
            owns_minecraft_java: bool,
        ) {
            self.add_minecraft_account_with_expiry_and_ownership(
                profile,
                86_400,
                owns_minecraft_java,
            )
            .await;
        }

        async fn add_minecraft_account_with_expiry(
            &self,
            profile: AuthLoginMinecraftProfile,
            expires_in: u64,
        ) {
            self.add_minecraft_account_with_expiry_and_ownership(profile, expires_in, true)
                .await;
        }

        async fn add_minecraft_account_with_expiry_and_ownership(
            &self,
            profile: AuthLoginMinecraftProfile,
            expires_in: u64,
            owns_minecraft_java: bool,
        ) -> AuthLoginMinecraftAccount {
            self.add_minecraft_account_with_tokens_and_expiry_and_ownership(
                profile,
                "msa-access-token",
                "minecraft-access-token",
                expires_in,
                owns_minecraft_java,
            )
            .await
        }

        async fn add_minecraft_account_with_tokens(
            &self,
            profile: AuthLoginMinecraftProfile,
            msa_access_token: &str,
            minecraft_access_token: &str,
        ) -> AuthLoginMinecraftAccount {
            self.add_minecraft_account_with_tokens_and_expiry_and_ownership(
                profile,
                msa_access_token,
                minecraft_access_token,
                86_400,
                true,
            )
            .await
        }

        async fn add_minecraft_account_with_tokens_and_expiry_and_ownership(
            &self,
            profile: AuthLoginMinecraftProfile,
            msa_access_token: &str,
            minecraft_access_token: &str,
            expires_in: u64,
            owns_minecraft_java: bool,
        ) -> AuthLoginMinecraftAccount {
            let (_token, account) = self
                .state
                .auth_logins()
                .replace_with_msa_and_minecraft_account(
                    NewAuthLoginMsaToken {
                        access_token: msa_access_token.to_string(),
                        refresh_token: Some("msa-refresh-token".to_string()),
                        id_token: None,
                        token_type: "Bearer".to_string(),
                        expires_in: 3600,
                        scope: Some("XboxLive.signin offline_access".to_string()),
                    },
                    NewAuthLoginMinecraftAccount {
                        access_token: minecraft_access_token.to_string(),
                        token_type: Some("Bearer".to_string()),
                        expires_in,
                        profile,
                        owns_minecraft_java,
                    },
                )
                .await;
            account
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-skin-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    async fn response_body(response: Response<Body>) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    async fn response_bytes(response: Response<Body>) -> Vec<u8> {
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body")
            .to_vec()
    }

    async fn normalize_skin_body(
        body: Vec<u8>,
    ) -> Result<Json<SkinNormalizeResponse>, (StatusCode, Json<serde_json::Value>)> {
        handle_skin_normalize(Body::from(body)).await
    }

    async fn skin_apply_route_test_server(
        mode: SkinApplyServerMode,
    ) -> (String, mpsc::UnboundedReceiver<RecordedSkinApplyRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/minecraft/profile/skins", post(record_skin_apply_route))
            .with_state(SkinApplyRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind skin apply route test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("skin apply route test server");
        });

        (format!("{base_url}/minecraft/profile/skins"), rx)
    }

    async fn skin_reset_route_test_server(
        mode: SkinResetServerMode,
    ) -> (String, mpsc::UnboundedReceiver<RecordedSkinResetRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/minecraft/profile/skins/active",
                delete(record_skin_reset_route),
            )
            .with_state(SkinResetRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind skin reset route test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("skin reset route test server");
        });

        (format!("{base_url}/minecraft/profile/skins/active"), rx)
    }

    async fn cape_sync_route_test_server(
        mode: CapeSyncServerMode,
    ) -> (String, mpsc::UnboundedReceiver<RecordedCapeSyncRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/minecraft/profile/capes/active",
                axum::routing::put(record_cape_sync_route).delete(record_cape_sync_route),
            )
            .with_state(CapeSyncRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind cape sync route test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("cape sync route test server");
        });

        (format!("{base_url}/minecraft/profile/capes/active"), rx)
    }

    async fn skin_profile_texture_test_server(
        mode: SkinProfileTextureServerMode,
    ) -> (
        String,
        mpsc::UnboundedReceiver<RecordedSkinProfileTextureRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/texture/{texture_id}",
                get(record_skin_profile_texture_route),
            )
            .with_state(SkinProfileTextureRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind skin profile texture test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("skin profile texture test server");
        });

        (format!("{base_url}/texture/"), rx)
    }

    async fn minecraft_username_test_server(
        mode: MinecraftUsernameServerMode,
    ) -> (
        String,
        String,
        mpsc::UnboundedReceiver<RecordedMinecraftUsernameRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/users/profiles/minecraft/{username}",
                get(record_minecraft_username_profile_route),
            )
            .route(
                "/session/minecraft/profile/{uuid}",
                get(record_minecraft_username_session_route),
            )
            .with_state(MinecraftUsernameRouteState { tx, mode });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind minecraft username test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("minecraft username test server");
        });

        (
            format!("{base_url}/users/profiles/minecraft"),
            format!("{base_url}/session/minecraft/profile"),
            rx,
        )
    }

    #[derive(Clone, Copy)]
    enum SkinApplyServerMode {
        Success,
        SuccessWithCapeAvailable,
        OversizedSuccess,
        RateLimited,
        Rejected,
    }

    #[derive(Clone, Copy)]
    enum SkinResetServerMode {
        Success,
        RateLimited,
    }

    #[derive(Clone, Copy)]
    enum CapeSyncServerMode {
        Success,
        RateLimited,
    }

    #[derive(Clone)]
    enum SkinProfileTextureServerMode {
        Png(Vec<u8>),
        Oversized,
    }

    #[derive(Clone)]
    enum MinecraftUsernameServerMode {
        Success {
            texture_url: String,
            model: Option<String>,
            cape_url: Option<String>,
        },
        NotFound,
        MissingSkin,
        MalformedTextures,
    }

    #[derive(Clone)]
    struct SkinApplyRouteState {
        tx: mpsc::UnboundedSender<RecordedSkinApplyRequest>,
        mode: SkinApplyServerMode,
    }

    #[derive(Clone)]
    struct SkinResetRouteState {
        tx: mpsc::UnboundedSender<RecordedSkinResetRequest>,
        mode: SkinResetServerMode,
    }

    #[derive(Clone)]
    struct CapeSyncRouteState {
        tx: mpsc::UnboundedSender<RecordedCapeSyncRequest>,
        mode: CapeSyncServerMode,
    }

    #[derive(Clone)]
    struct SkinProfileTextureRouteState {
        tx: mpsc::UnboundedSender<RecordedSkinProfileTextureRequest>,
        mode: SkinProfileTextureServerMode,
    }

    #[derive(Clone)]
    struct MinecraftUsernameRouteState {
        tx: mpsc::UnboundedSender<RecordedMinecraftUsernameRequest>,
        mode: MinecraftUsernameServerMode,
    }

    #[derive(Debug)]
    struct RecordedSkinApplyRequest {
        path: String,
        authorization: Option<String>,
        accept: Option<String>,
        user_agent: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    #[derive(Debug)]
    struct RecordedSkinResetRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        accept: Option<String>,
        user_agent: Option<String>,
    }

    #[derive(Debug)]
    struct RecordedCapeSyncRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        accept: Option<String>,
        user_agent: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    #[derive(Debug)]
    struct RecordedSkinProfileTextureRequest {
        path: String,
        accept: Option<String>,
        user_agent: Option<String>,
    }

    #[derive(Debug)]
    struct RecordedMinecraftUsernameRequest {
        path: String,
        accept: Option<String>,
        user_agent: Option<String>,
    }

    async fn record_skin_apply_route(
        AxumState(state): AxumState<SkinApplyRouteState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedSkinApplyRequest {
            path: "/minecraft/profile/skins".to_string(),
            authorization: header_value(&headers, "authorization"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
            content_type: header_value(&headers, "content-type"),
            body: body.to_vec(),
        });

        match state.mode {
            SkinApplyServerMode::Success => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "updated-profile-id",
                    "name": "UpdatedProfileName",
                    "skins": [{
                        "id": "updated-skin-id",
                        "state": "ACTIVE",
                        "url": "https://textures.minecraft.net/texture/updatedSkin",
                        "variant": "SLIM"
                    }],
                    "capes": []
                })),
            ),
            SkinApplyServerMode::SuccessWithCapeAvailable => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "updated-profile-id",
                    "name": "UpdatedProfileName",
                    "skins": [{
                        "id": "updated-skin-id",
                        "state": "ACTIVE",
                        "url": "https://textures.minecraft.net/texture/updatedSkin",
                        "variant": "SLIM"
                    }],
                    "capes": [{
                        "id": "cape-id",
                        "state": "INACTIVE",
                        "url": "https://textures.minecraft.net/texture/capeTexture"
                    }]
                })),
            ),
            SkinApplyServerMode::OversizedSuccess => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "payload": "x".repeat(MINECRAFT_SKIN_UPLOAD_RESPONSE_MAX_BYTES + 1),
                })),
            ),
            SkinApplyServerMode::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                })),
            ),
            SkinApplyServerMode::Rejected => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "path": "/home/zero/skin.png",
                    "error": "provider-secret-payload",
                })),
            ),
        }
    }

    async fn record_skin_reset_route(
        AxumState(state): AxumState<SkinResetRouteState>,
        method: axum::http::Method,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedSkinResetRequest {
            method: method.to_string(),
            path: "/minecraft/profile/skins/active".to_string(),
            authorization: header_value(&headers, "authorization"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
        });

        match state.mode {
            SkinResetServerMode::Success => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "reset-profile-id",
                    "name": "ResetProfileName",
                    "skins": [],
                    "capes": []
                })),
            ),
            SkinResetServerMode::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                })),
            ),
        }
    }

    async fn record_cape_sync_route(
        AxumState(state): AxumState<CapeSyncRouteState>,
        method: axum::http::Method,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedCapeSyncRequest {
            method: method.to_string(),
            path: "/minecraft/profile/capes/active".to_string(),
            authorization: header_value(&headers, "authorization"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
            content_type: header_value(&headers, "content-type"),
            body: body.to_vec(),
        });

        match state.mode {
            CapeSyncServerMode::Success => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "updated-profile-id",
                    "name": "UpdatedProfileName",
                    "skins": [{
                        "id": "updated-skin-id",
                        "state": "ACTIVE",
                        "url": "https://textures.minecraft.net/texture/updatedSkin",
                        "variant": "SLIM"
                    }],
                    "capes": [{
                        "id": "cape-id",
                        "state": if method == axum::http::Method::PUT { "ACTIVE" } else { "INACTIVE" },
                        "url": "https://textures.minecraft.net/texture/capeTexture"
                    }]
                })),
            ),
            CapeSyncServerMode::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                })),
            ),
        }
    }

    async fn record_skin_profile_texture_route(
        AxumState(state): AxumState<SkinProfileTextureRouteState>,
        Path(texture_id): Path<String>,
        headers: HeaderMap,
    ) -> Response<Body> {
        let _ = state.tx.send(RecordedSkinProfileTextureRequest {
            path: format!("/texture/{texture_id}"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
        });

        let (status, body) = match state.mode {
            SkinProfileTextureServerMode::Png(bytes) => (StatusCode::OK, bytes),
            SkinProfileTextureServerMode::Oversized => {
                (StatusCode::OK, vec![0; SKIN_UPLOAD_MAX_BYTES + 1])
            }
        };

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "image/png")
            .body(Body::from(body))
            .expect("skin profile texture response")
    }

    async fn record_minecraft_username_profile_route(
        AxumState(state): AxumState<MinecraftUsernameRouteState>,
        Path(username): Path<String>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedMinecraftUsernameRequest {
            path: format!("/users/profiles/minecraft/{username}"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
        });

        match state.mode {
            MinecraftUsernameServerMode::NotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "provider-secret-payload" })),
            ),
            _ => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": "0123456789abcdef0123456789abcdef",
                    "name": "ResolvedName",
                })),
            ),
        }
    }

    async fn record_minecraft_username_session_route(
        AxumState(state): AxumState<MinecraftUsernameRouteState>,
        Path(uuid): Path<String>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let _ = state.tx.send(RecordedMinecraftUsernameRequest {
            path: format!("/session/minecraft/profile/{uuid}"),
            accept: header_value(&headers, "accept"),
            user_agent: header_value(&headers, "user-agent"),
        });

        let textures_value = match state.mode {
            MinecraftUsernameServerMode::Success {
                texture_url,
                model,
                cape_url,
            } => {
                let mut skin = serde_json::json!({ "url": texture_url });
                if let Some(model) = model {
                    skin["metadata"] = serde_json::json!({ "model": model });
                }
                let mut textures = serde_json::json!({ "SKIN": skin });
                if let Some(cape_url) = cape_url {
                    textures["CAPE"] = serde_json::json!({ "url": cape_url });
                }
                base64_encode_standard(
                    serde_json::json!({ "textures": textures })
                        .to_string()
                        .as_bytes(),
                )
            }
            MinecraftUsernameServerMode::MissingSkin => {
                base64_encode_standard(br#"{"textures":{}}"#)
            }
            MinecraftUsernameServerMode::MalformedTextures => "not-base64!".to_string(),
            MinecraftUsernameServerMode::NotFound => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "provider-secret-payload" })),
                );
            }
        };

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": "0123456789abcdef0123456789abcdef",
                "name": "ResolvedName",
                "properties": [{
                    "name": "textures",
                    "value": textures_value,
                }],
            })),
        )
    }

    fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
    }

    fn body_contains(body: &[u8], needle: &[u8]) -> bool {
        body.windows(needle.len()).any(|window| window == needle)
    }

    fn base64_encode_standard(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = *chunk.get(1).unwrap_or(&0);
            let third = *chunk.get(2).unwrap_or(&0);

            encoded.push(ALPHABET[(first >> 2) as usize] as char);
            encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            if chunk.len() > 1 {
                encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
            } else {
                encoded.push('=');
            }
            if chunk.len() > 2 {
                encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
            } else {
                encoded.push('=');
            }
        }

        encoded
    }

    fn test_skin_png(width: u32, height: u32) -> Vec<u8> {
        let rgba = test_skin_rgba(width, height);
        encode_test_png(width, height, &rgba)
    }

    fn test_skin_png_with_seed(width: u32, height: u32, seed: u8) -> Vec<u8> {
        let mut rgba = test_skin_rgba(width, height);
        for pixel in rgba.chunks_mut(4) {
            pixel[0] = pixel[0].wrapping_add(seed);
            pixel[1] = pixel[1].wrapping_add(seed.wrapping_mul(3));
            pixel[2] = pixel[2].wrapping_add(seed.wrapping_mul(5));
        }
        encode_test_png(width, height, &rgba)
    }

    fn test_slim_skin_png() -> Vec<u8> {
        let mut rgba = test_skin_rgba(SKIN_WIDTH, SKIN_HEIGHT);
        for y in 20..32 {
            for x in 54..56 {
                let alpha_index = ((y * SKIN_WIDTH + x) * 4 + 3) as usize;
                rgba[alpha_index] = 0;
            }
        }

        encode_test_png(SKIN_WIDTH, SKIN_HEIGHT, &rgba)
    }

    fn test_skin_rgba(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[
                    x.wrapping_mul(3) as u8,
                    y.wrapping_mul(5) as u8,
                    x.wrapping_add(y) as u8,
                    255,
                ]);
            }
        }
        rgba
    }

    fn encode_test_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("write png header");
            writer.write_image_data(rgba).expect("write png pixels");
        }
        bytes
    }

    fn assert_texture_key(value: &str) {
        assert_eq!(value.len(), 64);
        assert!(value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    fn assert_skin_normalize_error(
        error: (StatusCode, Json<serde_json::Value>),
        expected_status: StatusCode,
        expected_message: &'static str,
    ) {
        assert_eq!(error.0, expected_status);
        assert_eq!(error.1.0, serde_json::json!({ "error": expected_message }));
        assert_eq!(error.1.0.as_object().expect("json object").len(), 1);
    }

    fn test_profile(name: &str, skins: Vec<AuthLoginMinecraftSkin>) -> AuthLoginMinecraftProfile {
        test_profile_with_capes(name, skins, Vec::new())
    }

    fn test_profile_with_capes(
        name: &str,
        skins: Vec<AuthLoginMinecraftSkin>,
        capes: Vec<AuthLoginMinecraftCape>,
    ) -> AuthLoginMinecraftProfile {
        AuthLoginMinecraftProfile {
            id: format!("{name}-id"),
            name: name.to_string(),
            skins,
            capes,
        }
    }

    fn minecraft_skin(id: &str, state: &str, url: &str, variant: &str) -> AuthLoginMinecraftSkin {
        AuthLoginMinecraftSkin {
            id: id.to_string(),
            state: state.to_string(),
            url: url.to_string(),
            variant: variant.to_string(),
        }
    }

    fn minecraft_cape(id: &str, state: &str, url: &str) -> AuthLoginMinecraftCape {
        AuthLoginMinecraftCape {
            id: id.to_string(),
            state: state.to_string(),
            url: url.to_string(),
        }
    }
}
