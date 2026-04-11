use crate::state::AppState;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use croopor_minecraft::{
    create_minecraft_dir, default_minecraft_dir, ensure_launcher_profiles, validate_installation,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct SetupDefaultsResponse {
    default_path: String,
    os: &'static str,
}

#[derive(Debug, Deserialize)]
struct SetupPathRequest {
    path: String,
}

#[derive(Debug, Serialize)]
struct SetupValidateResponse {
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/setup/defaults", get(handle_setup_defaults))
        .route("/api/v1/setup/validate", post(handle_setup_validate))
        .route("/api/v1/setup/set-dir", post(handle_setup_set_dir))
        .route("/api/v1/setup/init", post(handle_setup_init))
        .route("/api/v1/setup/browse", post(handle_setup_browse))
        .route(
            "/api/v1/onboarding/complete",
            post(handle_onboarding_complete),
        )
}

async fn handle_setup_defaults() -> Json<SetupDefaultsResponse> {
    Json(SetupDefaultsResponse {
        default_path: default_minecraft_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        os: std::env::consts::OS,
    })
}

async fn handle_setup_validate(
    Json(payload): Json<SetupPathRequest>,
) -> Json<SetupValidateResponse> {
    let path = PathBuf::from(payload.path);
    if path.as_os_str().is_empty() {
        return Json(SetupValidateResponse {
            valid: false,
            error: Some("path is empty".to_string()),
        });
    }
    if validate_installation(&path) {
        Json(SetupValidateResponse {
            valid: true,
            error: None,
        })
    } else {
        Json(SetupValidateResponse {
            valid: false,
            error: Some("minecraft installation is missing required directories".to_string()),
        })
    }
}

async fn handle_setup_set_dir(
    State(state): State<AppState>,
    Json(payload): Json<SetupPathRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let path = PathBuf::from(&payload.path);
    if !validate_installation(&path) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({ "error": "invalid minecraft installation: minecraft installation is missing required directories" }),
            ),
        ));
    }

    let mut config = state.config().current();
    config.mc_dir = payload.path.clone();
    state.config().update(config).map_err(internal_error)?;
    state.set_mc_dir(payload.path.clone());
    let _ = ensure_launcher_profiles(&path, "");

    Ok(Json(
        serde_json::json!({ "status": "ok", "mc_dir": payload.path }),
    ))
}

async fn handle_setup_init(
    State(state): State<AppState>,
    Json(payload): Json<SetupPathRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let path = if payload.path.is_empty() {
        default_minecraft_dir().unwrap_or_default()
    } else {
        PathBuf::from(&payload.path)
    };
    if path.as_os_str().is_empty() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "could not determine default minecraft path" })),
        ));
    }

    create_minecraft_dir(&path).map_err(internal_error)?;
    let _ = ensure_launcher_profiles(&path, "");

    let mut config = state.config().current();
    config.mc_dir = path.to_string_lossy().to_string();
    state.config().update(config).map_err(internal_error)?;
    state.set_mc_dir(path.to_string_lossy().to_string());

    Ok(Json(serde_json::json!({
        "status": "ok",
        "mc_dir": path.to_string_lossy().to_string()
    })))
}

async fn handle_setup_browse() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "path": "" }))
}

async fn handle_onboarding_complete(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mut config = state.config().current();
    config.onboarding_done = true;
    state.config().update(config).map_err(internal_error)?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
}
