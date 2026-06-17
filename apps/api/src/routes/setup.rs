use crate::{
    application::{self, SetupPathRequest},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};

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

async fn handle_setup_defaults() -> Json<application::SetupDefaultsResponse> {
    Json(application::setup_defaults())
}

async fn handle_setup_validate(
    Json(payload): Json<SetupPathRequest>,
) -> Json<application::SetupValidateResponse> {
    Json(application::setup_validate(payload))
}

async fn handle_setup_set_dir(
    State(state): State<AppState>,
    Json(payload): Json<SetupPathRequest>,
) -> Result<Json<application::SetupLibraryResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::setup_set_dir(&state, payload).map(Json)
}

async fn handle_setup_init(
    State(state): State<AppState>,
    Json(payload): Json<SetupPathRequest>,
) -> Result<Json<application::SetupLibraryResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::setup_init(&state, payload).map(Json)
}

async fn handle_setup_browse() -> Json<application::SetupBrowseResponse> {
    Json(application::setup_browse())
}

async fn handle_onboarding_complete(
    State(state): State<AppState>,
) -> Result<Json<application::SetupStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::onboarding_complete(&state).map(Json)
}
