use crate::state::AppState;
use crate::state::skins::{SavedSkinDeleteResult, SavedSkinRecord};
use axial_config::validate_username;
use axum::{
    Json,
    body::{Body, to_bytes},
    http::{Response, StatusCode, header},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};

use super::SKIN_UPLOAD_MAX_BYTES;
use super::cache::{
    profile_skin_file_cache_path, read_profile_skin_file_cache, write_profile_file_cache,
};
use super::errors::{
    ApiError, json_error, skin_texture_download_error, skin_username_lookup_error,
};
use super::image::{SKIN_HEIGHT, SKIN_WIDTH, normalize_skin_png, texture_key};
use super::profile_media::active_minecraft_profile_skin;
use super::provider::{MinecraftSkinTextureClient, MinecraftSkinUsernameClient};
use super::saved::{
    CapeUpdate, SAVED_SKIN_PROFILE_SOURCE, SAVED_SKIN_USERNAME_SOURCE,
    clear_pending_saved_skin_apply_for_texture, default_profile_skin_name,
    default_username_skin_name, delete_saved_skin, list_saved_skins, mark_saved_skin_applied,
    pending_saved_skin_apply_texture_key_for_active_account, read_saved_skin_png,
    replace_saved_skin_texture, retarget_pending_saved_skin_apply, save_saved_skin,
    update_saved_skin_metadata, validate_saved_skin_cape_update, validate_saved_skin_name,
    validate_saved_skin_upload_source, validate_saved_skin_variant, validate_texture_key,
};

pub(super) const SAVED_SKIN_FILE_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";
const SAVE_SKIN_FROM_PROFILE_REQUEST_MAX_BYTES: usize = 4 * 1024;
const SAVE_SKIN_FROM_USERNAME_REQUEST_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Serialize)]
pub(crate) struct SkinNormalizeResponse {
    pub(crate) texture_key: String,
    pub(crate) variant_suggestion: &'static str,
    pub(crate) original_width: u32,
    pub(crate) original_height: u32,
    pub(crate) normalized_width: u32,
    pub(crate) normalized_height: u32,
    pub(crate) normalized_byte_size: usize,
    pub(crate) normalized_data_url: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SaveSkinQuery {
    pub(crate) name: Option<String>,
    pub(crate) variant: Option<String>,
    pub(crate) cape_id: Option<String>,
    pub(crate) source: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SaveSkinFromProfileRequest {
    pub(crate) name: Option<String>,
    pub(crate) variant: Option<String>,
    pub(crate) mark_current: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SaveSkinFromUsernameRequest {
    pub(crate) username: String,
    pub(crate) name: Option<String>,
    pub(crate) variant: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct UpdateSavedSkinRequest {
    pub(crate) name: Option<String>,
    pub(crate) variant: Option<String>,
    #[serde(default)]
    #[serde(deserialize_with = "super::saved::deserialize_cape_update")]
    cape_id: CapeUpdate,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ReplaceSavedSkinTextureQuery {
    pub(crate) name: Option<String>,
    pub(crate) variant: Option<String>,
    pub(crate) cape_id: Option<String>,
    pub(crate) clear_cape: Option<bool>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SavedSkinsResponse {
    pub(crate) skins: Vec<SavedSkinRecord>,
    pub(crate) pending_apply_texture_key: Option<String>,
}

pub(crate) async fn handle_skin_normalize(
    body: Body,
) -> Result<Json<SkinNormalizeResponse>, ApiError> {
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

pub(crate) async fn handle_saved_skins(
    state: &AppState,
) -> Result<Json<SavedSkinsResponse>, ApiError> {
    let skins = list_saved_skins(state).await?;
    let pending_apply_texture_key =
        pending_saved_skin_apply_texture_key_for_active_account(state).await;

    Ok(Json(SavedSkinsResponse {
        skins,
        pending_apply_texture_key,
    }))
}

pub(crate) async fn handle_save_skin(
    state: &AppState,
    query: SaveSkinQuery,
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
            validate_saved_skin_cape_update(state, &CapeUpdate::Set(cape_id.to_string()))
                .await?
                .flatten()
        }
        None => None,
    };
    let source = validate_saved_skin_upload_source(query.source.as_deref())?;
    let texture_key = texture_key(&normalized.png_bytes);
    let record = save_saved_skin(
        state,
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

pub(crate) async fn handle_save_skin_from_profile(
    state: &AppState,
    body: Body,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let payload = read_save_skin_from_profile_request(body).await?;
    handle_save_skin_from_profile_with_client(state, payload, MinecraftSkinTextureClient::default())
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

pub(super) async fn handle_save_skin_from_profile_with_client(
    state: &AppState,
    payload: SaveSkinFromProfileRequest,
    client: MinecraftSkinTextureClient,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let profile_skin = active_minecraft_profile_skin(
        state,
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
        state.config().paths().skins_dir(),
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
            let _ = write_profile_file_cache(&cache_path, &normalized.png_bytes).await;
            normalized
        }
    };
    let variant = variant_override.unwrap_or_else(|| normalized.variant_suggestion.to_string());
    let texture_key = texture_key(&normalized.png_bytes);
    let mut record = save_saved_skin(
        state,
        texture_key,
        name,
        variant,
        SAVED_SKIN_PROFILE_SOURCE.to_string(),
        cape_id,
        normalized.png_bytes,
    )
    .await?;
    if payload.mark_current == Some(true)
        && let Some(applied_at) = mark_saved_skin_applied(state, record.texture_key.clone()).await?
    {
        record.applied_at = Some(applied_at);
    }

    Ok(Json(record))
}

pub(crate) async fn handle_save_skin_from_username(
    state: &AppState,
    body: Body,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    let payload = read_save_skin_from_username_request(body).await?;
    handle_save_skin_from_username_with_clients(
        state,
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

pub(super) async fn handle_save_skin_from_username_with_clients(
    state: &AppState,
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
        profile_skin_file_cache_path(state.config().paths().skins_dir(), &profile.texture_url);
    let normalized = match read_profile_skin_file_cache(&cache_path).await {
        Some(png_bytes) => normalize_skin_png(&png_bytes)?,
        None => {
            let bytes = texture_client
                .download(&profile.texture_url)
                .await
                .map_err(skin_texture_download_error)?;
            let normalized = normalize_skin_png(&bytes)?;
            let _ = write_profile_file_cache(&cache_path, &normalized.png_bytes).await;
            normalized
        }
    };
    let variant = variant_override.unwrap_or_else(|| normalized.variant_suggestion.to_string());
    let texture_key = texture_key(&normalized.png_bytes);
    let record = save_saved_skin(
        state,
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

pub(crate) async fn handle_delete_skin(
    state: &AppState,
    texture_key: String,
) -> Result<Json<serde_json::Value>, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    match delete_saved_skin(state, texture_key.clone()).await? {
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

pub(crate) async fn handle_update_saved_skin(
    state: &AppState,
    texture_key: String,
    payload: UpdateSavedSkinRequest,
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
    let cape_id = validate_saved_skin_cape_update(state, &payload.cape_id).await?;
    let updated = update_saved_skin_metadata(state, texture_key, name, variant, cape_id)
        .await?
        .ok_or_else(|| json_error(StatusCode::NOT_FOUND, "saved skin not found"))?;

    Ok(Json(updated))
}

pub(crate) async fn handle_replace_saved_skin_texture(
    state: &AppState,
    path_texture_key: String,
    query: ReplaceSavedSkinTextureQuery,
    body: Body,
) -> Result<Json<SavedSkinRecord>, ApiError> {
    // Replacing texture bytes must retarget pending apply state from the old key to the new one.
    let old_texture_key = validate_texture_key(&path_texture_key)?;
    let saved_skins = list_saved_skins(state).await?;
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
    let cape_id = validate_saved_skin_cape_update(state, &cape_update)
        .await?
        .unwrap_or_else(|| current.cape_id.clone());
    let new_texture_key = texture_key(&normalized.png_bytes);
    let updated = replace_saved_skin_texture(
        state,
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

pub(crate) async fn handle_saved_skin_file(
    state: &AppState,
    texture_key: String,
) -> Result<Response<Body>, ApiError> {
    let texture_key = validate_texture_key(&texture_key)?;
    let Some(bytes) = read_saved_skin_png(state, texture_key).await? else {
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
