//! Application-owned setup and onboarding workflows.

use crate::{application::instances::invalidate_create_view_cache, state::AppState};
use axial_minecraft::{create_minecraft_dir, ensure_launcher_profiles};
use axum::{Json, http::StatusCode};
use serde::Serialize;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Serialize)]
pub struct SetupLibraryResponse {
    pub status: &'static str,
    pub library_dir: String,
    pub library_mode: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SetupStatusResponse {
    pub status: &'static str,
}

pub async fn setup_init(state: &AppState) -> Result<SetupLibraryResponse, ApiError> {
    let path = state.config().paths().library_dir.clone();
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
    state.invalidate_installed_versions();
    invalidate_create_view_cache();

    Ok(SetupLibraryResponse {
        status: "ok",
        library_dir: path.to_string_lossy().to_string(),
        library_mode: "managed",
    })
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
        "Could not save the managed library folder. Check app data permissions and try again.",
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
            "Could not save the managed library folder. Check app data permissions and try again.",
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
