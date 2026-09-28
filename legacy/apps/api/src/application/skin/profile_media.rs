use crate::state::{AppState, AuthLoginMinecraftAccount};
use axial_config::validate_username;
use axial_minecraft::offline_uuid;
use axum::{
    Json,
    body::Body,
    http::{Response, StatusCode, header},
};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::cache::{
    PROFILE_CAPE_FILE_CACHE_CONTROL, PROFILE_SKIN_FILE_CACHE_CONTROL, profile_cape_file_cache_path,
    profile_skin_file_cache_path, read_profile_cape_file_cache, read_profile_skin_file_cache,
    write_profile_file_cache,
};
use super::errors::{
    ApiError, cape_texture_download_error, cape_texture_invalid_error, json_status_error,
    skin_texture_download_error, skin_username_lookup_error,
};
use super::image::{is_valid_cape_texture_png, normalize_skin_png, render_skin_head_png};
use super::provider::{
    MINECRAFT_TEXTURE_URL_PREFIX, MinecraftSkinTextureClient, MinecraftSkinUsernameClient,
    MinecraftUsernameSkinProfile, active_minecraft_cape_id, sane_minecraft_texture_url,
    sane_minecraft_texture_url_with_prefix, select_minecraft_skin,
    select_sane_minecraft_skin_with_prefix, skin_variant,
};
use super::saved::{SAVED_SKIN_USERNAME_SOURCE, validate_saved_skin_cape_id};

pub(super) const DEFAULT_HEAD_SIZE: u32 = 64;
const MIN_HEAD_SIZE: u32 = 16;
const MAX_HEAD_SIZE: u32 = 256;
pub(super) const HEAD_CACHE_CONTROL: &str = "private, max-age=86400";

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SkinQuery {
    pub(crate) username: Option<String>,
    pub(crate) size: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SkinProfileFileQuery {
    pub(crate) texture: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SkinCapeFileQuery {
    pub(crate) id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SkinLookupQuery {
    pub(crate) username: String,
    pub(crate) size: Option<u32>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SkinProfileResponse {
    pub(crate) auth_mode: &'static str,
    pub(crate) username: String,
    pub(crate) uuid: String,
    pub(crate) source: &'static str,
    pub(crate) variant: &'static str,
    pub(crate) texture_url: Option<String>,
    pub(crate) head_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SkinLookupResponse {
    pub(crate) username: String,
    pub(crate) uuid: String,
    pub(crate) source: &'static str,
    pub(crate) variant: &'static str,
    pub(crate) texture_url: String,
    pub(crate) texture_file_url: String,
    pub(crate) cape_url: Option<String>,
    pub(crate) head_url: String,
}

pub(super) struct ActiveMinecraftProfileSkin {
    pub(super) account: AuthLoginMinecraftAccount,
    pub(super) texture_url: String,
    pub(super) cape_id: Option<String>,
}

struct ActiveMinecraftProfileCape {
    texture_url: String,
}

pub(crate) async fn handle_skin_profile(
    state: &AppState,
    query: SkinQuery,
) -> Result<Json<SkinProfileResponse>, ApiError> {
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

pub(super) async fn active_minecraft_profile_skin(
    state: &AppState,
    allowed_prefix: &str,
    not_ready_message: &'static str,
) -> Result<ActiveMinecraftProfileSkin, ApiError> {
    // Remote profile textures must belong to the expected Minecraft texture host before use.
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

pub(crate) async fn handle_skin_head(
    state: &AppState,
    query: SkinQuery,
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

pub(crate) async fn handle_skin_lookup(
    query: SkinLookupQuery,
) -> Result<Json<SkinLookupResponse>, ApiError> {
    handle_skin_lookup_with_client(
        query,
        MinecraftSkinUsernameClient::default(),
        MINECRAFT_TEXTURE_URL_PREFIX.to_string(),
    )
    .await
}

pub(super) async fn handle_skin_lookup_with_client(
    query: SkinLookupQuery,
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

pub(crate) async fn handle_skin_lookup_file(
    state: &AppState,
    query: SkinLookupQuery,
) -> Result<Response<Body>, ApiError> {
    handle_skin_lookup_file_with_clients(
        state,
        query,
        MinecraftSkinUsernameClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

pub(super) async fn handle_skin_lookup_file_with_clients(
    state: &AppState,
    query: SkinLookupQuery,
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
        profile_skin_file_cache_path(state.config().paths().skins_dir(), &profile.texture_url);
    if let Some(bytes) = read_profile_skin_file_cache(&cache_path).await {
        return profile_skin_file_response(bytes);
    }

    let bytes = texture_client
        .download(&profile.texture_url)
        .await
        .map_err(skin_texture_download_error)?;
    let normalized = normalize_skin_png(&bytes)?;
    let png_bytes = normalized.png_bytes;
    let _ = write_profile_file_cache(&cache_path, &png_bytes).await;

    profile_skin_file_response(png_bytes)
}

pub(crate) async fn handle_skin_lookup_head(
    state: &AppState,
    query: SkinLookupQuery,
) -> Result<Response<Body>, ApiError> {
    handle_skin_lookup_head_with_clients(
        state,
        query,
        MinecraftSkinUsernameClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

pub(super) async fn handle_skin_lookup_head_with_clients(
    state: &AppState,
    query: SkinLookupQuery,
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
        profile_skin_file_cache_path(state.config().paths().skins_dir(), &profile.texture_url);
    let normalized_png = match read_profile_skin_file_cache(&cache_path).await {
        Some(bytes) => bytes,
        None => {
            let bytes = texture_client
                .download(&profile.texture_url)
                .await
                .map_err(skin_texture_download_error)?;
            let normalized = normalize_skin_png(&bytes)?;
            let png_bytes = normalized.png_bytes;
            let _ = write_profile_file_cache(&cache_path, &png_bytes).await;
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

pub(crate) async fn handle_skin_lookup_cape(
    state: &AppState,
    query: SkinLookupQuery,
) -> Result<Response<Body>, ApiError> {
    handle_skin_lookup_cape_with_clients(
        state,
        query,
        MinecraftSkinUsernameClient::default(),
        MinecraftSkinTextureClient::default(),
    )
    .await
}

pub(super) async fn handle_skin_lookup_cape_with_clients(
    state: &AppState,
    query: SkinLookupQuery,
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
    let cache_path = profile_cape_file_cache_path(state.config().paths().skins_dir(), &cape_url);
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
    let _ = write_profile_file_cache(&cache_path, &bytes).await;

    profile_cape_file_response(bytes)
}

pub(crate) async fn handle_skin_profile_file(
    state: &AppState,
    query: SkinProfileFileQuery,
) -> Result<Response<Body>, ApiError> {
    handle_skin_profile_file_with_client(state, query, MinecraftSkinTextureClient::default()).await
}

pub(super) async fn handle_skin_profile_file_with_client(
    state: &AppState,
    query: SkinProfileFileQuery,
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
                state,
                client.allowed_prefix(),
                "Minecraft account is not ready for profile skin preview",
            )
            .await?
            .texture_url
        }
    };
    let cache_path = profile_skin_file_cache_path(state.config().paths().skins_dir(), &texture_url);
    if let Some(bytes) = read_profile_skin_file_cache(&cache_path).await {
        return profile_skin_file_response(bytes);
    }

    let bytes = client
        .download(&texture_url)
        .await
        .map_err(skin_texture_download_error)?;
    let normalized = normalize_skin_png(&bytes)?;
    let png_bytes = normalized.png_bytes;
    let _ = write_profile_file_cache(&cache_path, &png_bytes).await;

    profile_skin_file_response(png_bytes)
}

pub(crate) async fn handle_skin_cape_file(
    state: &AppState,
    query: SkinCapeFileQuery,
) -> Result<Response<Body>, ApiError> {
    handle_skin_cape_file_with_client(state, query, MinecraftSkinTextureClient::default()).await
}

pub(super) async fn handle_skin_cape_file_with_client(
    state: &AppState,
    query: SkinCapeFileQuery,
    client: MinecraftSkinTextureClient,
) -> Result<Response<Body>, ApiError> {
    let cape_id = validate_saved_skin_cape_id(&query.id)?;
    let cape = active_minecraft_profile_cape(
        state,
        &cape_id,
        client.allowed_prefix(),
        "Minecraft account is not ready for cape preview",
    )
    .await?;
    let cache_path =
        profile_cape_file_cache_path(state.config().paths().skins_dir(), &cape.texture_url);
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
    let _ = write_profile_file_cache(&cache_path, &bytes).await;

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

pub(super) fn offline_variant(uuid: &str) -> &'static str {
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

pub(super) fn offline_head_svg(uuid: &str, size: u32) -> String {
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
