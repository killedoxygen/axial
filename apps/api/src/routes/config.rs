use crate::{
    application::{self, ConfigPatch},
    state::AppState,
};
use axial_config::AppConfig;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, put},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/config", get(handle_get_config))
        .route("/api/v1/config", put(handle_update_config))
}

async fn handle_get_config(State(state): State<AppState>) -> Json<AppConfig> {
    Json(application::current_config(&state))
}

async fn handle_update_config(
    State(state): State<AppState>,
    Json(patch): Json<ConfigPatch>,
) -> Result<Json<AppConfig>, (StatusCode, Json<serde_json::Value>)> {
    application::update_config(&state, patch).map(Json)
}
