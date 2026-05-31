use crate::state::AppState;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, put},
};
use croopor_config::{AppConfig, ConfigStoreError};
use serde::Deserialize;

const CONFIG_SAVE_ERROR_MESSAGE: &str =
    "Could not save settings. Check app data permissions and try again.";

#[derive(Debug, Default, Deserialize)]
struct ConfigPatch {
    username: Option<String>,
    launch_auth_mode: Option<String>,
    max_memory_mb: Option<i32>,
    min_memory_mb: Option<i32>,
    java_path_override: Option<String>,
    window_width: Option<i32>,
    window_height: Option<i32>,
    onboarding_done: Option<bool>,
    jvm_preset: Option<String>,
    performance_mode: Option<String>,
    guardian_mode: Option<String>,
    theme: Option<String>,
    custom_hue: Option<i32>,
    custom_vibrancy: Option<i32>,
    lightness: Option<i32>,
    music_enabled: Option<bool>,
    music_volume: Option<i32>,
    music_track: Option<i32>,
    library_dir: Option<String>,
    library_mode: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/config", get(handle_get_config))
        .route("/api/v1/config", put(handle_update_config))
}

async fn handle_get_config(State(state): State<AppState>) -> Json<AppConfig> {
    Json(state.config().current())
}

async fn handle_update_config(
    State(state): State<AppState>,
    Json(patch): Json<ConfigPatch>,
) -> Result<Json<AppConfig>, (StatusCode, Json<serde_json::Value>)> {
    let mut next = state.config().current();
    if let Some(username) = patch.username {
        next.username = username;
    }
    if let Some(launch_auth_mode) = patch.launch_auth_mode {
        next.launch_auth_mode = launch_auth_mode;
    }
    if let Some(max_memory_mb) = patch.max_memory_mb.filter(|value| *value > 0) {
        next.max_memory_mb = max_memory_mb;
    }
    if let Some(min_memory_mb) = patch.min_memory_mb.filter(|value| *value > 0) {
        next.min_memory_mb = min_memory_mb;
    }
    if let Some(java_path_override) = patch.java_path_override {
        next.java_path_override = java_path_override;
    }
    if let Some(window_width) = patch.window_width {
        next.window_width = window_width;
    }
    if let Some(window_height) = patch.window_height {
        next.window_height = window_height;
    }
    if let Some(onboarding_done) = patch.onboarding_done {
        next.onboarding_done = onboarding_done;
    }
    if let Some(jvm_preset) = patch.jvm_preset {
        next.jvm_preset = jvm_preset;
    }
    if let Some(performance_mode) = patch.performance_mode {
        next.performance_mode = performance_mode;
    }
    if let Some(guardian_mode) = patch.guardian_mode {
        next.guardian_mode = guardian_mode;
    }
    if let Some(theme) = patch.theme {
        next.theme = theme;
    }
    if let Some(custom_hue) = patch.custom_hue {
        next.custom_hue = Some(custom_hue);
    }
    if let Some(custom_vibrancy) = patch.custom_vibrancy {
        next.custom_vibrancy = Some(custom_vibrancy);
    }
    if let Some(lightness) = patch.lightness {
        next.lightness = Some(lightness);
    }
    if let Some(music_enabled) = patch.music_enabled {
        next.music_enabled = Some(music_enabled);
    }
    if let Some(music_volume) = patch.music_volume {
        next.music_volume = Some(music_volume);
    }
    if let Some(music_track) = patch.music_track {
        next.music_track = music_track.max(0);
    }
    if let Some(library_dir) = patch.library_dir {
        next.library_dir = library_dir;
    }
    if let Some(library_mode) = patch.library_mode {
        next.library_mode = library_mode;
    }

    match state.config().update(next) {
        Ok(config) => {
            state.set_library_dir(config.library_dir.clone());
            Ok(Json(config))
        }
        Err(error) => Err(config_update_error_response(error)),
    }
}

fn config_update_error_response(error: ConfigStoreError) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        ConfigStoreError::Validation(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": CONFIG_SAVE_ERROR_MESSAGE })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_SAVE_ERROR_MESSAGE, config_update_error_response};
    use axum::Json;
    use croopor_config::{AppConfigValidationError, ConfigStoreError};

    #[test]
    fn config_update_validation_error_keeps_details() {
        let (status, Json(body)) = config_update_error_response(ConfigStoreError::Validation(
            AppConfigValidationError::InvalidUsername("Letters, numbers, and underscores only."),
        ));

        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            serde_json::json!({
                "error": "invalid username: Letters, numbers, and underscores only."
            })
        );
    }

    #[test]
    fn config_update_non_validation_error_hides_local_paths() {
        let paths = [
            "/Users/alice/Library/Application Support/Croopor/config.json",
            r"C:\Users\Alice\AppData\Roaming\Croopor\config.json",
        ];

        for path in paths {
            let (status, Json(body)) =
                config_update_error_response(ConfigStoreError::Read(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("permission denied writing {path}"),
                )));
            let message = body
                .get("error")
                .and_then(|value| value.as_str())
                .expect("error response should include a string message");

            assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(message, CONFIG_SAVE_ERROR_MESSAGE);
            assert!(!message.contains(path));
        }
    }
}
