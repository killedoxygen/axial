use axum::{Json, http::StatusCode};

use super::provider::{
    MinecraftUsernameSkinError, SkinCapeError, SkinTextureDownloadError, SkinUploadError,
};

pub(crate) type ApiError = (StatusCode, Json<serde_json::Value>);

pub(super) fn json_error(status: StatusCode, message: &'static str) -> ApiError {
    (status, Json(serde_json::json!({ "error": message })))
}

pub(super) fn bounded_error_message(error: &ApiError) -> &str {
    error
        .1
        .0
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("skin operation failed")
}

pub(super) fn json_status_error(
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

pub(super) fn skin_auth_store_error() -> ApiError {
    json_status_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not save Minecraft account changes. Restart Axial and try again.",
        "minecraft_account_store_failed",
    )
}

pub(super) fn skin_upload_error(error: SkinUploadError) -> ApiError {
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

pub(super) fn skin_reset_error(error: SkinUploadError) -> ApiError {
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

pub(super) fn skin_cape_error(error: SkinCapeError) -> ApiError {
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

pub(super) fn skin_texture_download_error(error: SkinTextureDownloadError) -> ApiError {
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

pub(super) fn cape_texture_download_error(error: SkinTextureDownloadError) -> ApiError {
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

pub(super) fn cape_texture_invalid_error() -> ApiError {
    json_status_error(
        StatusCode::BAD_GATEWAY,
        "Minecraft cape texture is invalid",
        "minecraft_cape_texture_invalid",
    )
}

pub(super) fn skin_preserve_download_error(error: SkinTextureDownloadError) -> ApiError {
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

pub(super) fn skin_preserve_invalid_error() -> ApiError {
    json_status_error(
        StatusCode::CONFLICT,
        "Current Minecraft profile skin cannot be preserved before changing it",
        "minecraft_profile_skin_preserve_invalid",
    )
}

pub(super) fn skin_username_lookup_error(error: MinecraftUsernameSkinError) -> ApiError {
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
