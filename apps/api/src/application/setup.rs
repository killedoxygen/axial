//! Application-owned setup and onboarding workflows.

use crate::{application::instances::invalidate_create_view_cache, state::AppState};
use axial_config::AppPaths;
use axial_minecraft::{
    create_minecraft_dir, default_minecraft_dir, ensure_launcher_profiles, validate_installation,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Serialize)]
pub struct SetupDefaultsResponse {
    pub managed_default_path: String,
    pub existing_default_path: String,
    pub recommended_mode: &'static str,
    pub os: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct SetupPathRequest {
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct SetupValidateResponse {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SetupLibraryResponse {
    pub status: &'static str,
    pub library_dir: String,
    pub library_mode: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SetupBrowseResponse {
    pub path: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SetupStatusResponse {
    pub status: &'static str,
}

pub fn setup_defaults() -> SetupDefaultsResponse {
    let paths = AppPaths::detect();
    SetupDefaultsResponse {
        managed_default_path: paths.library_dir.to_string_lossy().to_string(),
        existing_default_path: default_minecraft_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        recommended_mode: "managed",
        os: std::env::consts::OS,
    }
}

pub fn setup_validate(payload: SetupPathRequest) -> SetupValidateResponse {
    let path = PathBuf::from(payload.path);
    if path.as_os_str().is_empty() {
        return SetupValidateResponse {
            valid: false,
            error: Some("path is empty".to_string()),
        };
    }
    if validate_installation(&path) {
        SetupValidateResponse {
            valid: true,
            error: None,
        }
    } else {
        SetupValidateResponse {
            valid: false,
            error: Some("existing library is missing required directories".to_string()),
        }
    }
}

pub async fn setup_set_dir(
    state: &AppState,
    payload: SetupPathRequest,
) -> Result<SetupLibraryResponse, ApiError> {
    let path = PathBuf::from(&payload.path);
    if !validate_installation(&path) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({ "error": "invalid existing library: existing library is missing required directories" }),
            ),
        ));
    }

    let library_dir = payload.path.clone();
    state
        .mutate_config(move |latest| {
            latest.library_dir = library_dir;
            latest.library_mode = "existing".to_string();
            Ok(())
        })
        .await
        .map_err(setup_config_error)?;
    invalidate_create_view_cache();
    let _ = ensure_launcher_profiles(&path, "");

    Ok(SetupLibraryResponse {
        status: "ok",
        library_dir: payload.path,
        library_mode: "existing",
    })
}

pub async fn setup_init(
    state: &AppState,
    payload: SetupPathRequest,
) -> Result<SetupLibraryResponse, ApiError> {
    let path = if payload.path.is_empty() {
        AppPaths::detect().library_dir
    } else {
        PathBuf::from(&payload.path)
    };
    if path.as_os_str().is_empty() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "could not determine default Axial library path" })),
        ));
    }

    create_minecraft_dir(&path).map_err(setup_managed_create_error)?;
    let _ = ensure_launcher_profiles(&path, "");

    let library_dir = path.to_string_lossy().to_string();
    state
        .mutate_config(move |latest| {
            latest.library_dir = library_dir;
            latest.library_mode = "managed".to_string();
            Ok(())
        })
        .await
        .map_err(setup_config_error)?;
    invalidate_create_view_cache();

    Ok(SetupLibraryResponse {
        status: "ok",
        library_dir: path.to_string_lossy().to_string(),
        library_mode: "managed",
    })
}

pub fn setup_browse() -> SetupBrowseResponse {
    SetupBrowseResponse { path: "" }
}

pub async fn onboarding_complete(state: &AppState) -> Result<SetupStatusResponse, ApiError> {
    state
        .mutate_config(move |latest| {
            latest.onboarding_done = true;
            Ok(())
        })
        .await
        .map_err(onboarding_save_error)?;
    Ok(SetupStatusResponse { status: "ok" })
}

fn setup_managed_create_error(_error: impl std::fmt::Display) -> ApiError {
    internal_error(
        "Could not create the managed library folder. Check folder permissions and try again.",
    )
}

fn setup_config_error(_error: impl std::fmt::Display) -> ApiError {
    internal_error(
        "Could not save the selected library folder. Check app data permissions and try again.",
    )
}

fn onboarding_save_error(_error: impl std::fmt::Display) -> ApiError {
    internal_error("Could not save onboarding progress. Check app data permissions and try again.")
}

fn internal_error(message: &'static str) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": message })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_bounded_setup_error(error: ApiError, expected_message: &str) {
        let (status, Json(body)) = error;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], expected_message);

        let rendered = body.to_string();
        assert!(!rendered.contains("/Users/alice/.axial"));
        assert!(!rendered.contains("permission denied"));
        assert!(!rendered.contains("config.toml"));
    }

    #[test]
    fn setup_managed_create_error_does_not_expose_raw_error_fragments() {
        assert_bounded_setup_error(
            setup_managed_create_error("permission denied creating /Users/alice/.axial/libraries"),
            "Could not create the managed library folder. Check folder permissions and try again.",
        );
    }

    #[test]
    fn setup_config_error_does_not_expose_raw_error_fragments() {
        assert_bounded_setup_error(
            setup_config_error("failed to write /Users/alice/.axial/config.toml"),
            "Could not save the selected library folder. Check app data permissions and try again.",
        );
    }

    #[test]
    fn setup_onboarding_save_error_does_not_expose_raw_error_fragments() {
        assert_bounded_setup_error(
            onboarding_save_error("permission denied writing /Users/alice/.axial/config.toml"),
            "Could not save onboarding progress. Check app data permissions and try again.",
        );
    }
}
