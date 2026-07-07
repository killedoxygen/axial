use crate::{
    application::{self, FlagOverridePatch},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, put},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/flags", get(handle_list_flags))
        .route("/api/v1/flags/{key}", put(handle_update_flag))
}

async fn handle_list_flags(State(state): State<AppState>) -> Json<application::FlagsResponse> {
    Json(application::list_flags(&state))
}

async fn handle_update_flag(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(patch): Json<FlagOverridePatch>,
) -> Result<Json<application::FlagsResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::update_flag(&state, &key, patch)
        .await
        .map(Json)
}
